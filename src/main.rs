use core::time;
use std::{
    env::temp_dir,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
};

use lazycell::LazyCell;
use nix::sys::stat;
use nix::unistd;

fn make_fifo(path: &PathBuf) {
    let _ = fs::remove_file(path);

    match unistd::mkfifo(path, stat::Mode::S_IRWXU) {
        Ok(_) => {}
        Err(err) => panic!("Failed to create pipe: {}", err),
    }
}

fn create_socket_listener(path: &PathBuf) -> UnixListener {
    let _ = fs::remove_file(path);

    UnixListener::bind(path).unwrap()
}

fn handle_stream(mut stream: UnixStream, sender: Sender<String>) {
    println!("New connection!!");
    loop {
        let mut buffer = String::new();
        let size = stream.read_to_string(&mut buffer).unwrap();

        // if size is 0, that means the socket was shut down, so just exit the loop (and thread) then
        if size == 0 {
            break;
        }

        sender.send(buffer).unwrap();
    }
}

#[derive(Debug)]
struct Fifo {
    fifo: File,
    valid: bool, // an fifo can become invalid if the other side closes
}

fn main() {
    // CREATING PIPES
    // ==========================

    let in_fifo_path = temp_dir().as_path().join("joshu-in.pipe");
    let out_fifo_path = temp_dir().as_path().join("joshu-out.pipe");
    make_fifo(&in_fifo_path);
    make_fifo(&out_fifo_path);

    // LISTENING ON SOCKET
    // ===================
    let (sender, receiver): (Sender<String>, Receiver<String>) = mpsc::channel();

    let socket_path = temp_dir().as_path().join("joshu.socket");
    let socket_listener = create_socket_listener(&socket_path);
    std::thread::spawn(move || loop {
        let (stream, _address) = socket_listener.accept().unwrap();

        let sender_clone = sender.clone();
        let stream_clone = stream.try_clone().unwrap();
        std::thread::spawn(move || {
            handle_stream(stream_clone, sender_clone);
        });
    });

    // READING JOSHU OUTPUT
    // =================
    let in_fifo_path_clone = in_fifo_path.clone();

    // this buffer is used to read data from the pipe
    // for some fucking reason, reading to a string doesn't work, so I'm just using a huge 0.5MB static buffer instead
    // TODO: either figure out why read_to_string doesn't work or add multiple smaller buffers together to only allocate the amount necessary
    let mut buffer = [0; 1000 * 500];

    std::thread::spawn(move || loop {
        let mut in_fifo = OpenOptions::new()
            .read(true)
            .open(&in_fifo_path_clone)
            .unwrap();

        let size = in_fifo.read(&mut buffer).unwrap();
        let buffer_str = std::str::from_utf8(&buffer[..size]).unwrap().trim();
        println!("Joshu output: '{}'", buffer_str);
    });

    // SENDING MESSAGES TO JOSHU
    // =========================
    let mut child: Option<Child> = None;
    let mut out_fifo: LazyCell<Fifo> = LazyCell::new();

    loop {
        let received_message = receiver.recv().unwrap();

        let child_is_none = child.is_none();
        let mut child_exited = false;

        if child.is_some() {
            match &mut child {
                Some(c) => {
                    if matches!(c.try_wait(), Ok(Some(_))) {
                        child_exited = true;
                    }
                }

                None => {}
            }
        }

        if child_is_none || child_exited {
            child = Some(
                Command::new("./target/release/joshu-core")
                    // cd
                    .current_dir("/home/haris/projects/project-joshu/joshu-core")
                    // args
                    .arg(in_fifo_path.to_str().unwrap())
                    .arg(out_fifo_path.to_str().unwrap())
                    // ignore std streams
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    // spawn
                    .spawn()
                    .expect("Failed to start joshu-core"),
            );

            if out_fifo.filled() {
                out_fifo.borrow_mut().unwrap().valid = false;
            }
        }

        if !out_fifo.filled() {
            out_fifo
                .fill(Fifo {
                    fifo: OpenOptions::new()
                        .write(true)
                        .open(out_fifo_path.clone())
                        .unwrap(),
                    valid: true,
                })
                .unwrap();
        } else if !out_fifo.borrow().unwrap().valid {
            out_fifo.borrow_mut().unwrap().fifo = OpenOptions::new()
                .write(true)
                .open(out_fifo_path.clone())
                .unwrap();
        }

        out_fifo
            .borrow_mut()
            .unwrap()
            .fifo
            .write_all(format!("{}\n", received_message).as_bytes())
            .unwrap();
        out_fifo.borrow_mut().unwrap().fifo.flush().unwrap();

        // sleep before processing the next message
        // I added this because I had a problem where 2 messages would be received combined when joshu-core read them if they arrived at nearly the same time
        // 500 may be too long but eh whatever
        std::thread::sleep(time::Duration::from_millis(500));
    }
}
