//! Synchronous, blocking client used by the wrapper hot path and by the
//! `soldr daemon status|stop` CLI surface. All daemon calls are best
//! effort: every error variant is mapped to a `ClientError` so the
//! caller can decide whether to fall back to direct redb writes.

use crate::cache_lib::target_registry::{current_unix_seconds, TargetRegistry};
use crate::cache_lib::{daemon_sock_path, data_db_path};
use crate::core::SoldrPaths;
use crate::daemon::ipc::{read_frame_sync, write_frame_sync};
use crate::daemon::protocol::{Request, Response, StatusInfo};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 50 ms write timeout matches the plan: short enough that the wrapper
/// never blocks the hot path on a hung daemon.
const HOT_PATH_TIMEOUT: Duration = Duration::from_millis(50);

/// Slightly more generous timeout for request/response calls that need
/// to read a body back (status, shutdown). Still small enough that the
/// CLI returns quickly even if the daemon is unresponsive.
const REPLY_TIMEOUT: Duration = Duration::from_millis(2_000);

#[derive(Debug)]
pub enum ClientError {
    /// No daemon endpoint exists at the expected path (most common case
    /// on a fresh checkout — caller should fall back to direct redb).
    NotRunning,
    /// Endpoint exists but the connect / read / write failed.
    Io(std::io::Error),
    /// Daemon answered something we didn't ask for (or an Error variant).
    Protocol(String),
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning
            }
            _ => ClientError::Io(e),
        }
    }
}

/// Submit `req` to the daemon and drop the connection without reading
/// any reply. Used for `RecordTargetTouch`. The wrapper hot path calls
/// this and ignores the result on the failure side.
pub fn submit_fire_and_forget(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    let mut stream = connect(sock_path, HOT_PATH_TIMEOUT)?;
    write_frame_sync(&mut stream, req)?;
    Ok(())
}

/// Submit `req`, wait for one `Response`, return it.
pub fn submit_request(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
    write_frame_sync(&mut stream, req)?;
    let resp: Response = read_frame_sync(&mut stream)?;
    Ok(resp)
}

pub fn status(sock_path: &Path) -> Result<StatusInfo, ClientError> {
    match submit_request(sock_path, &Request::Status)? {
        Response::Status(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn shutdown(sock_path: &Path) -> Result<(), ClientError> {
    match submit_request(sock_path, &Request::Shutdown)? {
        Response::ShuttingDown => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Wrapper-side entry point. Tries the daemon first; on any failure,
/// upserts the row directly to the redb file. **Never** propagates
/// errors — a missing daemon must not break a build.
pub fn record_target_touch_or_fallback(paths: &SoldrPaths, target: &Path) {
    let unix_seconds = match current_unix_seconds() {
        Ok(s) => s,
        Err(_) => return,
    };

    let sock = daemon_sock_path(paths);
    let req = Request::RecordTargetTouch {
        path: target.display().to_string(),
        unix_seconds,
    };
    if submit_fire_and_forget(&sock, &req).is_ok() {
        return;
    }

    let db_path = data_db_path(paths);
    if let Ok(registry) = TargetRegistry::open(&db_path) {
        let _ = registry.upsert_with_time(target, unix_seconds);
    }
}

#[cfg(unix)]
fn connect(sock_path: &Path, timeout: Duration) -> Result<UnixOrPipe, ClientError> {
    let stream = std::os::unix::net::UnixStream::connect(sock_path)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout.max(Duration::from_millis(200))))?;
    Ok(UnixOrPipe(stream))
}

#[cfg(unix)]
pub struct UnixOrPipe(std::os::unix::net::UnixStream);

#[cfg(unix)]
impl std::io::Read for UnixOrPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(unix)]
impl std::io::Write for UnixOrPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(windows)]
fn connect(sock_path: &Path, _timeout: Duration) -> Result<UnixOrPipe, ClientError> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new().read(true).write(true).open(sock_path)?;
    Ok(UnixOrPipe(file))
}

#[cfg(windows)]
pub struct UnixOrPipe(std::fs::File);

#[cfg(windows)]
impl std::io::Read for UnixOrPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(windows)]
impl std::io::Write for UnixOrPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Returns the well-known socket path the wrapper should use. Centralized
/// here so callers don't need to import `cache_lib` directly.
pub fn default_sock_path(paths: &SoldrPaths) -> PathBuf {
    #[cfg(unix)]
    {
        daemon_sock_path(paths)
    }
    #[cfg(windows)]
    {
        use crate::cache_lib::daemon_pipe_name;
        PathBuf::from(format!(r"\\.\pipe\{}", daemon_pipe_name(paths)))
    }
}
