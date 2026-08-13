// aux-build:interprocess.rs

extern crate interprocess;

use interprocess::local_socket::{ConnectOptions, ListenerOptions};
use interprocess::os::windows::named_pipe::DuplexPipeStream;

fn main() {
    let _listener = ListenerOptions::new();
    let _connector = ConnectOptions::new();
    DuplexPipeStream::connect_by_path_with_wait_mode();
}
