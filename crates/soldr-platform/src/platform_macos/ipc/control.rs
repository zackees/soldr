#![allow(missing_docs)]

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::ToFsName as _;
use std::io;
use std::time::Duration;

pub type ControlStream = interprocess::local_socket::Stream;

pub fn connect(endpoint: String, timeout: Duration) -> io::Result<ControlStream> {
    let name = endpoint.to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
    let stream = interprocess::local_socket::ConnectOptions::new()
        .name(name)
        .wait_mode(interprocess::ConnectWaitMode::Timeout(timeout))
        .connect_sync()?;
    stream.set_recv_timeout(Some(timeout))?;
    stream.set_send_timeout(Some(timeout))?;
    Ok(stream)
}

pub fn configure_timeouts(stream: &mut ControlStream, recv: Duration, send: Duration) -> io::Result<()> {
    stream.set_recv_timeout(Some(recv))?;
    stream.set_send_timeout(Some(send))
}
