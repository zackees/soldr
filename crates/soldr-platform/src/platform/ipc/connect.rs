//! Client-side transport: opening a control stream to the daemon
//! endpoint, with the Windows named-pipe busy-retry policy.
//!
//! Host-neutral policy lives here (retry budget, timeout error
//! construction, the deadline-bounded worker executor); the
//! OS-specific open/connect implementations live in the concrete
//! trees and are re-exported below.

use std::io::{self, Read, Write};
use std::time::Duration;

pub use crate::platform_imp::ipc::connect::{
    connect_unix, open_pipe_with_retry, probe_accepts_connections,
};

/// Combined synchronous transport surface (read + write + send).
pub trait SyncStream: Read + Write + Send {}
impl<T: Read + Write + Send> SyncStream for T {}

/// A boxed synchronous transport, usable with the daemon's sync frame
/// codec. Unix: an AF_UNIX socket. Windows: a named pipe tunneled
/// through a tokio runtime worker.
pub type BoxedSyncStream = Box<dyn SyncStream>;

/// Combined asynchronous transport surface, matching the generic
/// bounds of the daemon's async frame codec.
pub trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AsyncStream for T {}

/// A boxed async transport usable by the daemon's async frame codec:
/// the Windows named-pipe client on Windows, a `tokio` UnixStream
/// wrapper elsewhere.
pub type BoxedAsyncStream = Box<dyn AsyncStream>;

/// Windows `ERROR_PIPE_BUSY` (231) — the "all pipe instances are busy"
/// code. Meaningful only on the Windows transport; kept here so the
/// classification site and the retry loop share one constant.
pub const ERROR_PIPE_BUSY: i32 = 231;

/// How many times a client re-dials a busy named pipe. Bounded so a
/// single client call cannot spin forever against a saturated listener
/// pool, while still riding out a transient pool-wide admission spike.
pub const PIPE_BUSY_RETRY_LIMIT: u32 = 8;

/// Outcome of [`open_pipe_with_retry`]: the connected stream plus how
/// many busy-pipe retries the open consumed (surfaced to the daemon as
/// `ipc_busy_retries` telemetry).
pub struct PipeOpen {
    /// The connected async transport.
    pub stream: BoxedAsyncStream,
    /// Busy-pipe retries consumed before the open succeeded.
    pub busy_retries: u32,
}

/// Backoff between busy-pipe retries: exponential with a tiny
/// deterministic jitter so synchronized cargo workers do not re-dial in
/// lockstep, while keeping this retry policy testable and
/// dependency-free.
pub fn busy_pipe_retry_delay(attempt: u32) -> Duration {
    let base_ms = (2_u64.saturating_mul(1_u64 << attempt.min(5))).min(64);
    let jitter_ms = (u64::from(attempt) * 17 + u64::from(std::process::id())) % 4;
    Duration::from_millis(base_ms + jitter_ms)
}

/// Timeout error wording shared by every Windows-transport operation,
/// matching the historical message exactly:
/// `"{operation} timed out after {}ms"`.
pub fn pipe_timeout_error(operation: &str, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{operation} timed out after {}ms", timeout.as_millis()),
    )
}

/// Run `f` on a named worker thread and await its result with a
/// wall-clock deadline, mirroring the historical Windows transport:
/// the named-pipe client is async, so the work runs inside its own
/// tokio runtime on the worker, and the caller never blocks beyond
/// `timeout` even if the worker wedges.
pub fn run_in_pipe_worker<T, F>(operation: &'static str, timeout: Duration, f: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("soldr-daemon-client".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(pipe_timeout_error(operation, timeout))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(format!(
            "{operation} worker exited without a result"
        ))),
    }
}
