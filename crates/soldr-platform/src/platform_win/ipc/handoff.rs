//! Windows handoff: receive the SESSION stream the broker duplicated
//! over `DuplicateHandle`.

use std::io;

use crate::platform::ipc::handoff::ReceivedFd;

/// Unsupported on Windows: the broker hands over a duplicated handle,
/// never a descriptor. Unreachable at runtime.
pub fn receive_unix_descriptor(
    _control: interprocess::local_socket::tokio::Stream,
) -> io::Result<(
    interprocess::local_socket::tokio::Stream,
    ReceivedFd,
    [u8; running_process::broker::server::HANDOFF_TOKEN_BYTES],
)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SCM_RIGHTS handoff does not exist on Windows",
    ))
}

/// Unsupported on Windows; unreachable at runtime.
pub fn session_stream_from_received_fd(
    _fd: ReceivedFd,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor handoff does not exist on Windows",
    ))
}

/// Wrap the duplicated handle value from the handoff offer into a
/// tokio named-pipe SESSION stream. Called only after the daemon
/// verified the offer's handle is non-null.
pub fn named_pipe_stream_from_handle_value(
    value: u64,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};

    let owned = unsafe { OwnedHandle::from_raw_handle(value as *mut _) };
    let stream =
        interprocess::os::windows::named_pipe::local_socket::tokio::Stream::try_from(owned)
            .map_err(|error| error.to_io_error())?;
    Ok(stream.into())
}

/// No-op on Windows: the broker hands over a duplicated handle, never
/// a descriptor, so there is never a descriptor to close.
pub fn close_received_fd(_fd: ReceivedFd) {}

/// Unsupported on Windows: there is no `SCM_RIGHTS` transport to drive.
/// Callers skip the descriptor-handoff regression when they see
/// `Unsupported`.
pub fn send_test_handoff_descriptor(
    _handoff_endpoint: &str,
    _token: &[u8; running_process::broker::server::HANDOFF_TOKEN_BYTES],
) -> io::Result<(
    interprocess::local_socket::Stream,
    interprocess::local_socket::Stream,
)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SCM_RIGHTS handoff does not exist on Windows",
    ))
}
