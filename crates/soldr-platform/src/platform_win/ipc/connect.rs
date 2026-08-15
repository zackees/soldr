//! Windows client transport: tokio named-pipe client with the
//! `ERROR_PIPE_BUSY` retry policy.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::platform::ipc::connect::{
    busy_pipe_retry_delay, BoxedSyncStream, PipeOpen, ERROR_PIPE_BUSY, PIPE_BUSY_RETRY_LIMIT,
    PIPE_NOT_FOUND_RETRY_LIMIT,
};

/// Connect an interprocess local socket through the platform boundary.
pub async fn connect_local_socket(
    name: interprocess::local_socket::Name<'_>,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    use interprocess::local_socket::tokio::prelude::*;
    interprocess::local_socket::tokio::Stream::connect(name).await
}

/// Open the named-pipe client at `path`, retrying `ERROR_PIPE_BUSY`
/// with the shared exponential backoff. A busy pipe is listener-pool
/// backpressure, not evidence that the daemon died, so the retry loop
/// lives inside this one client call and callers never mistake it for
/// daemon failure.
///
/// `ERROR_FILE_NOT_FOUND` is also retried, but under the much smaller
/// [`PIPE_NOT_FOUND_RETRY_LIMIT`]: a healthy server's pipe name is
/// transiently absent between one client's disconnect and the next
/// `CreateNamedPipe`, and failing that instant as "NotRunning" was the
/// Windows-only `daemon registry query: NotRunning` failure the msvc
/// target-run lanes kept reporting against a demonstrably live daemon.
pub async fn open_pipe_with_retry(path: &Path) -> io::Result<PipeOpen> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let mut not_found_attempts: u32 = 0;
    for attempt in 0..PIPE_BUSY_RETRY_LIMIT {
        match ClientOptions::new().open(path) {
            Ok(stream) => {
                return Ok(PipeOpen {
                    stream: Box::new(stream),
                    busy_retries: attempt,
                })
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && attempt + 1 < PIPE_BUSY_RETRY_LIMIT =>
            {
                tokio::time::sleep(busy_pipe_retry_delay(attempt)).await;
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && not_found_attempts + 1 < PIPE_NOT_FOUND_RETRY_LIMIT
                    && attempt + 1 < PIPE_BUSY_RETRY_LIMIT =>
            {
                not_found_attempts += 1;
                tokio::time::sleep(busy_pipe_retry_delay(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop either returned a client or its last IO error")
}

/// Unsupported on Windows: the Windows transport runs the whole request
/// on a tokio worker thread (see [`run_in_pipe_worker`]), so there is
/// no sync stream to hand back. Present so the facade surface is
/// uniform; callers reach it only when the host is not Windows.
pub fn connect_unix(_path: &Path, _read_timeout: Duration, _write_timeout: Duration) -> io::Result<BoxedSyncStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sync Unix-stream connect is not supported on Windows",
    ))
}

/// Windows named pipes have no filesystem entry to probe: a connect
/// only succeeds while a broker is actively listening under the pipe
/// name, and a readiness probe would require the full framed
/// handshake. The caller treats Windows admission as unknown
/// (`false`) and falls back to the timeout-bounded spawn path.
pub fn probe_accepts_connections(_endpoint: &str) -> bool {
    false
}
