#![allow(missing_docs)]

use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};

pub fn bind_listener(endpoint: &str, _backlog: i32) -> io::Result<interprocess::local_socket::tokio::Listener> {
    let _guard = UnixBindGuard::acquire(endpoint)?;
    create_listener(endpoint).or_else(|error| {
        if running_process::broker::server::singleton_bind::is_already_bound_error(&error)
            && running_process::broker::server::singleton_bind::unix_socket_path_is_stale(endpoint)
        {
            std::fs::remove_file(endpoint)?;
            return create_listener(endpoint);
        }
        Err(error)
    })
}

fn create_listener(endpoint: &str) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    use std::os::unix::fs::PermissionsExt as _;

    let name = running_process::broker::server::singleton_bind::wrap_socket_name(endpoint)
        .map_err(io::Error::other)?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio_as::<interprocess::os::unix::uds_local_socket::tokio::Listener>()?;
    std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener.into())
}

struct UnixBindGuard(std::fs::File);

impl UnixBindGuard {
    fn acquire(endpoint: &str) -> io::Result<Self> {
        use fs2::FileExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let lock_path = std::path::Path::new(endpoint)
            .parent()
            .ok_or_else(|| io::Error::other("broker endpoint has no parent"))?
            .join("bind.lock");
        let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock_path)?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for UnixBindGuard {
    fn drop(&mut self) {
        // `File::unlock` is inherent as of the pinned toolchain, so the
        // `fs2::FileExt` import this used to need is now unused and denied.
        let _ = self.0.unlock();
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
