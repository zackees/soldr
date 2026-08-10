//! Deadline-bounded connect + frame-length-cap helpers for the sync IPC
//! clients (issue #590, cluster B).
//!
//! `interprocess` local sockets expose no portable connect or read
//! timeout, and none of the sync clients under this module capped the
//! 4-byte length prefix before allocating. That left two ways for a
//! bound-but-unaccepting, crashed, or hostile peer to wedge or exhaust the
//! Python-facing client:
//!
//! - **Connect wedge:** `Stream::connect` is a blocking syscall; a socket
//!   that exists but whose server never completes the accept parks the
//!   caller indefinitely. [`connect_with_timeout`] runs the blocking
//!   connect on a helper thread and bounds it (mirrors the proven
//!   `broker::backend_lifecycle::probe` idiom).
//! - **Huge-alloc / unbounded read:** a bogus length prefix (e.g.
//!   `0xFFFF_FFFF`) drove a multi-GiB `vec![0u8; len]` followed by a read
//!   for bytes that never arrive. [`check_frame_len`] rejects any length
//!   over [`MAX_FRAME_BYTES`] before allocating, matching what the broker
//!   framing layer already enforces.

use crate::broker::protocol::framing::MAX_FRAME_BYTES;
use crate::client::paths;
use interprocess::local_socket::Stream;
use std::io::{self, Read};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Poll interval for the nonblocking deadline reads. Short enough that a
/// prompt daemon reply is picked up with negligible latency, long enough
/// that a stalled peer doesn't spin the CPU.
const NONBLOCKING_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Default read timeout for a request/response RPC round-trip. A stalled
/// or crashed-mid-reply daemon must not wedge the Python-facing client
/// forever (issue #590, cluster B1). Override with
/// `RUNNING_PROCESS_CLIENT_RPC_TIMEOUT_MS` (milliseconds).
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT_ENV: &str = "RUNNING_PROCESS_CLIENT_RPC_TIMEOUT_MS";

/// Deadline for a single RPC response read, measured from now.
pub(crate) fn rpc_read_deadline() -> Instant {
    let timeout = std::env::var(RPC_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_RPC_TIMEOUT);
    Instant::now() + timeout
}

fn wait_for_io(deadline: Instant) -> io::Result<()> {
    let now = Instant::now();
    if now >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "daemon response read timed out; the daemon accepted the \
             connection but never sent a complete reply",
        ));
    }
    thread::sleep((deadline - now).min(NONBLOCKING_POLL_INTERVAL));
    Ok(())
}

/// Read exactly `buf.len()` bytes from a **nonblocking** reader, bounded by
/// `deadline`. `Ok(0)` is treated as "no data available yet" and retried
/// against the deadline rather than as EOF: on a Windows `PIPE_NOWAIT`
/// handle a read with no data ready returns `Ok(0)`, not `WouldBlock`, so
/// this mirrors the proven `broker::backend_lifecycle::probe` idiom. A
/// genuinely closed peer therefore surfaces as a `TimedOut` at the
/// deadline rather than a spurious early EOF.
pub(crate) fn read_exact_with_deadline<R: Read>(
    reader: &mut R,
    mut buf: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buf.is_empty() {
        match reader.read(buf) {
            Ok(0) => wait_for_io(deadline)?,
            Ok(read) => {
                let tmp = buf;
                buf = &mut tmp[read..];
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Write all of `buf` to a **nonblocking** writer, bounded by `deadline`.
/// Needed because `try_clone` shares the underlying `O_NONBLOCK` file
/// description on Unix, so once the read handle is nonblocking the write
/// handle is too — a blocking `write_all` would spuriously fail.
pub(crate) fn write_all_with_deadline<W: io::Write>(
    writer: &mut W,
    mut buf: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => wait_for_io(deadline)?,
            Ok(written) => buf = &buf[written..],
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

/// Read one `u32`-big-endian length-prefixed frame from a nonblocking
/// reader with the [`check_frame_len`] cap and a hard `deadline`. This is
/// the bounded replacement for the raw `read_exact` framing the sync
/// clients used on request/response paths (issue #590, cluster B1).
pub(crate) fn read_frame_with_deadline<R: Read>(
    reader: &mut R,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    read_exact_with_deadline(reader, &mut len_buf, deadline)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    check_frame_len(len)?;
    let mut buf = vec![0u8; len];
    if len > 0 {
        read_exact_with_deadline(reader, &mut buf, deadline)?;
    }
    Ok(buf)
}

/// Default connect timeout for the sync IPC clients. Generous enough not
/// to trip on a briefly-busy daemon, but bounded so a permanently
/// non-accepting socket can't wedge the caller. Override with
/// `RUNNING_PROCESS_CLIENT_CONNECT_TIMEOUT_MS` (milliseconds).
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT_ENV: &str = "RUNNING_PROCESS_CLIENT_CONNECT_TIMEOUT_MS";

fn connect_timeout() -> Duration {
    std::env::var(CONNECT_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
}

/// Connect to `socket_path` on a helper thread, bounded by
/// [`connect_timeout`]. On timeout the helper thread retains ownership of
/// (and eventually drops) the abandoned connection attempt — the same
/// leak-on-timeout pattern the broker probe path uses — so a wedged
/// listener never wedges the caller.
pub(crate) fn connect_with_timeout(socket_path: &str) -> io::Result<Stream> {
    let path = socket_path.to_string();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("rp-client-connect".to_string())
        .spawn(move || {
            use interprocess::local_socket::traits::Stream as _;
            let result = match paths::make_socket_name(&path) {
                Ok(name) => Stream::connect(name),
                Err(err) => Err(err),
            };
            // Receiver gone means the caller timed out; drop the stream here.
            let _ = tx.send(result);
        })?;

    match rx.recv_timeout(connect_timeout()) {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "daemon socket connect timed out after {:?}: the socket \
                 exists but the server never accepted the connection \
                 (path {socket_path})",
                connect_timeout()
            ),
        )),
    }
}

/// Reject a frame length larger than [`MAX_FRAME_BYTES`] before it is used
/// to allocate a receive buffer, so a corrupt/hostile/desynced peer cannot
/// trigger a multi-GiB allocation followed by an unbounded read for bytes
/// that never arrive.
pub(crate) fn check_frame_len(len: usize) -> io::Result<()> {
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("daemon frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_frame_len_accepts_within_cap() {
        assert!(check_frame_len(0).is_ok());
        assert!(check_frame_len(MAX_FRAME_BYTES).is_ok());
    }

    #[test]
    fn check_frame_len_rejects_over_cap() {
        let err = check_frame_len(MAX_FRAME_BYTES + 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn check_frame_len_rejects_bogus_u32_max() {
        // A desynced peer sending 0xFFFF_FFFF must be rejected before the
        // ~4 GiB allocation, not after.
        assert!(check_frame_len(u32::MAX as usize).is_err());
    }

    #[test]
    fn connect_with_timeout_errors_on_missing_socket() {
        // A path that does not exist should fail promptly (connection
        // refused / not found), never hang.
        let bogus = if cfg!(windows) {
            r"\\.\pipe\running-process-nonexistent-test-endpoint"
        } else {
            "/tmp/running-process-nonexistent-test-endpoint.sock"
        };
        assert!(connect_with_timeout(bogus).is_err());
    }

    /// A reader that is permanently `WouldBlock` — models a daemon that
    /// accepted the connection but never sent a reply.
    struct NeverReady;
    impl Read for NeverReady {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "never ready"))
        }
    }

    #[test]
    fn read_exact_with_deadline_times_out_instead_of_hanging() {
        let mut reader = NeverReady;
        let mut buf = [0u8; 4];
        let start = Instant::now();
        let err = read_exact_with_deadline(
            &mut reader,
            &mut buf,
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // Bounded — must not hang.
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn read_frame_with_deadline_times_out_on_silent_peer() {
        let mut reader = NeverReady;
        let start = Instant::now();
        let err =
            read_frame_with_deadline(&mut reader, Instant::now() + Duration::from_millis(100))
                .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn read_frame_with_deadline_reads_a_complete_frame() {
        let payload = b"hello-daemon";
        let mut framed = Vec::new();
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(payload);
        let mut cursor = io::Cursor::new(framed);
        let got =
            read_frame_with_deadline(&mut cursor, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn read_frame_with_deadline_rejects_oversized_length_prefix() {
        // A peer supplying 0xFFFF_FFFF as the length must be rejected before
        // the ~4 GiB allocation.
        let mut framed = vec![0xFF_u8; 4];
        framed.extend_from_slice(b"body");
        let mut cursor = io::Cursor::new(framed);
        let err = read_frame_with_deadline(&mut cursor, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
