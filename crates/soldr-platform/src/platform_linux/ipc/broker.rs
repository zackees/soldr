#![allow(missing_docs)]

use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};

pub fn bind_listener(
    endpoint: &str,
    backlog: i32,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    if let Some(parent) = std::path::Path::new(endpoint).parent() {
        running_process::broker::secure_dir::ensure_private_dir(parent)?;
    }
    running_process::broker::server::singleton_bind::bind_singleton_with(endpoint, || {
        create_listener(endpoint, backlog)
    })
    .map_err(map_bind_singleton_error)
}

fn create_listener(
    endpoint: &str,
    backlog: i32,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    use interprocess::os::unix::local_socket::ListenerOptionsExt as _;

    let name = running_process::broker::server::singleton_bind::wrap_socket_name(endpoint)
        .map_err(io::Error::other)?;
    let listener = ListenerOptions::new()
        .name(name)
        .mode(0o600)
        .create_tokio_as::<interprocess::os::unix::uds_local_socket::tokio::Listener>()?;
    if unsafe { libc::listen(listener.as_fd().as_raw_fd(), backlog) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(listener.into())
}

fn map_bind_singleton_error(
    error: running_process::broker::server::singleton_bind::BindSingletonError,
) -> io::Error {
    use running_process::broker::server::singleton_bind::BindSingletonError;

    match error {
        BindSingletonError::InvalidName(message) => {
            io::Error::new(io::ErrorKind::InvalidInput, message)
        }
        BindSingletonError::AlreadyBound(error) | BindSingletonError::Other(error) => error,
    }
}

pub fn duplicate_stream(
    stream: &interprocess::local_socket::tokio::Stream,
) -> io::Result<interprocess::local_socket::Stream> {
    let interprocess::local_socket::tokio::Stream::UdSocket(stream) = stream;
    let duplicated = unsafe { libc::fcntl(stream.as_fd().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicated) };
    Ok(interprocess::os::unix::uds_local_socket::Stream::from(owned).into())
}

pub fn retire_endpoint(endpoint: &str) {
    let _ = std::fs::remove_file(endpoint);
}

pub fn seed_stale_endpoint(endpoint: &std::path::Path) -> io::Result<()> {
    drop(std::os::unix::net::UnixListener::bind(endpoint)?);
    Ok(())
}
