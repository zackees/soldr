//! macOS client transport: AF_UNIX control stream with socket
//! timeouts.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::platform::ipc::connect::{BoxedSyncStream, PipeOpen};

/// Connect an AF_UNIX socket to `path` with the given socket timeouts.
/// The write timeout is the caller's deadline; the read timeout is at
/// least 200ms so a short reply deadline never starves a frame read.
pub fn connect_unix(path: &Path, read_timeout: Duration, write_timeout: Duration) -> io::Result<BoxedSyncStream> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_write_timeout(Some(write_timeout))?;
    stream.set_read_timeout(Some(read_timeout.max(Duration::from_millis(200))))?;
    Ok(Box::new(stream))
}

/// Open a `tokio` UnixStream to `path`. The Windows named-pipe busy
/// retry does not apply here: AF_UNIX connect either succeeds or fails
/// immediately, so the busy-retry count is always zero. Only reached at
/// runtime on Windows paths that the Linux host never executes; present
/// so the facade surface is uniform.
pub async fn open_pipe_with_retry(path: &Path) -> io::Result<PipeOpen> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(PipeOpen {
        stream: Box::new(stream),
        busy_retries: 0,
    })
}

/// Whether a Unix-domain stream can connect to `endpoint` (the
/// socket's filesystem path). A broker admits new peers the moment a
/// listening socket exists at the path, so a plain connect probe is a
/// faithful readiness check.
pub fn probe_accepts_connections(endpoint: &str) -> bool {
    std::os::unix::net::UnixStream::connect(endpoint).is_ok()
}
