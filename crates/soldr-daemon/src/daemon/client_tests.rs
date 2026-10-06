//! Unit coverage split from `client.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[test]
fn shutdown_compat_starts_with_the_immediately_previous_protocol() {
    assert_eq!(
        SHUTDOWN_COMPAT_PROTOCOL_VERSIONS.first().copied(),
        Some(crate::daemon::protocol::PROTOCOL_VERSION - 1)
    );
}

#[test]
fn reply_timeout_defaults_to_30_min() {
    // Unset / empty / non-numeric / zero all fall back to the generous
    // default so a legitimate slow release compile is never cut off.
    let default = Duration::from_secs(DEFAULT_REPLY_TIMEOUT_SECS);
    assert_eq!(parse_reply_timeout(None), default);
    assert_eq!(parse_reply_timeout(Some("")), default);
    assert_eq!(parse_reply_timeout(Some("nope")), default);
    assert_eq!(parse_reply_timeout(Some("0")), default);
}

#[test]
fn reply_timeout_env_override_fails_fast() {
    // #1364: an operator can opt into a short fail-fast budget.
    assert_eq!(parse_reply_timeout(Some("30")), Duration::from_secs(30));
    assert_eq!(parse_reply_timeout(Some("  5 ")), Duration::from_secs(5));
}

// ---- soldr#2844 follow-up: the missing-ack line must be sayable -------------

/// The diagnostic names the request kind and the reason.
///
/// soldr#2844 shipped this through `tracing::warn!`. The wrapper process
/// installs no subscriber -- only the daemon does -- so the warning was
/// emitted into nothing, which the integration test could not catch because it
/// asserts on the *call's* result and a silent warning changes neither.
#[test]
fn the_missing_ack_line_names_the_request_and_the_reason() {
    let request = Request::RecordTargetTouch {
        path: "/work/target".to_string(),
        unix_seconds: 1_700_000_000,
    };
    let line = missing_ack_message(&request, "no ack within 200ms");

    // Prefixed like every other client-side line, so it is greppable with them.
    assert!(line.starts_with("soldr: "), "{line}");
    assert!(line.contains("RecordTargetTouch"), "{line}");
    assert!(line.contains("no ack within 200ms"), "{line}");
    assert!(line.contains("delivery is unconfirmed"), "{line}");
    assert!(line.contains("soldr#2785"), "{line}");
    // One line: a multi-line diagnostic interleaves with concurrent compiler
    // output on the wrapper hot path.
    assert!(!line.contains('\n'), "{line}");
}

#[test]
fn the_missing_ack_line_does_not_carry_the_target_path() {
    let request = Request::RecordTargetTouch {
        path: "/some/very/long/workspace/target/directory".to_string(),
        unix_seconds: 1_700_000_000,
    };
    let line = missing_ack_message(&request, "reset");
    assert!(
        !line.contains("/some/very/long/workspace"),
        "the path is what the reader already knows and what makes it long: {line}"
    );
}

#[test]
fn a_cook_touch_is_named_distinctly() {
    let line = missing_ack_message(&Request::CookTouch { sha256: [0u8; 32] }, "reset");
    assert!(line.contains("CookTouch"), "{line}");
}

// ---- soldr#2955: a missing ack stays best-effort ----------------------------

/// A peer that takes the frame and answers nothing.
///
/// Reads report EOF, which is what a daemon that accepted the connection and
/// then went away looks like from the client side: no drop logged, no upsert
/// failure, no ack.
struct SilentPeer {
    written: usize,
}

impl Read for SilentPeer {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for SilentPeer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// soldr#2558 bounded the ack wait rather than requiring it, and soldr#2785
/// stopped discarding its outcome. Those two must not collide: observing a
/// missing ack must not quietly promote it to a failure, or every pre-#2558
/// daemon becomes a hard error on the wrapper hot path.
#[test]
fn a_peer_that_never_acks_is_still_a_successful_submit() {
    let mut peer = SilentPeer { written: 0 };

    let result = write_awaiting_receipt_ack(
        &mut peer,
        &Request::RecordTargetTouch {
            path: "/some/workspace/target".to_string(),
            unix_seconds: 1_700_000_000,
        },
        Duration::from_millis(50),
    );

    assert!(
        matches!(result, Ok(ReceiptAck::Unconfirmed(_))),
        "a missing ack must be reported as unconfirmed, not become an error: {result:?}"
    );
    assert!(
        peer.written > 0,
        "the request frame should still have been written to the peer"
    );
}

struct ScriptedControlStream {
    replies: Cursor<Vec<u8>>,
    writes: Arc<Mutex<Vec<u8>>>,
    dropped: Arc<AtomicBool>,
}

impl Read for ScriptedControlStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.replies.read(buf)
    }
}

impl Write for ScriptedControlStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes
            .lock()
            .expect("writes lock")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for ScriptedControlStream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn scripted_resident_peer() -> (BoxedControlStream, Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>) {
    let mut replies = Vec::new();
    write_frame_sync(
        &mut replies,
        &Response::ResidentCapacityAcquired { permits: 2 },
    )
    .expect("encode acquired reply");
    write_frame_sync(&mut replies, &Response::Ack).expect("encode release ack");
    let writes = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(AtomicBool::new(false));
    (
        Box::new(ScriptedControlStream {
            replies: Cursor::new(replies),
            writes: writes.clone(),
            dropped: dropped.clone(),
        }),
        writes,
        dropped,
    )
}

#[test]
fn resident_capacity_lease_retains_stream_until_explicit_finish() {
    let (stream, writes, dropped) = scripted_resident_peer();
    let lease = acquire_resident_capacity_on_stream(stream, 2).expect("acquire lease");
    assert!(!dropped.load(Ordering::SeqCst));

    lease.finish().expect("release lease");
    assert!(dropped.load(Ordering::SeqCst));

    let mut sent = Cursor::new(writes.lock().expect("writes lock").clone());
    assert!(matches!(
        read_frame_sync::<_, Request>(&mut sent).expect("acquire request"),
        Request::AcquireResidentCapacity { permits: 2 }
    ));
    assert!(matches!(
        read_frame_sync::<_, Request>(&mut sent).expect("release request"),
        Request::ReleaseResidentCapacity
    ));
}

#[test]
fn dropping_resident_capacity_lease_disconnects_without_a_release_frame() {
    let (stream, writes, dropped) = scripted_resident_peer();
    let lease = acquire_resident_capacity_on_stream(stream, 2).expect("acquire lease");
    drop(lease);
    assert!(dropped.load(Ordering::SeqCst));

    let mut sent = Cursor::new(writes.lock().expect("writes lock").clone());
    assert!(matches!(
        read_frame_sync::<_, Request>(&mut sent).expect("acquire request"),
        Request::AcquireResidentCapacity { permits: 2 }
    ));
    assert!(read_frame_sync::<_, Request>(&mut sent).is_err());
}

// ---- soldr#3558: an IPC failure says which stage it failed in -----------

/// A control stream whose halves answer or fail on demand, so each stage of
/// a round trip can be provoked without a live daemon.
struct ScriptedIo {
    reply: Vec<u8>,
    write_error: Option<std::io::Error>,
    read_error: Option<std::io::Error>,
}

impl ScriptedIo {
    fn new() -> Self {
        ScriptedIo {
            reply: Vec::new(),
            write_error: None,
            read_error: None,
        }
    }

    fn reply(mut self, bytes: Vec<u8>) -> Self {
        self.reply = bytes;
        self
    }

    fn fail_write(mut self, error: std::io::Error) -> Self {
        self.write_error = Some(error);
        self
    }

    fn fail_read(mut self, error: std::io::Error) -> Self {
        self.read_error = Some(error);
        self
    }
}

impl Read for ScriptedIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(error) = self.read_error.take() {
            return Err(error);
        }
        let n = self.reply.len().min(buf.len());
        buf[..n].copy_from_slice(&self.reply[..n]);
        self.reply.drain(..n);
        Ok(n)
    }
}

impl Write for ScriptedIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(error) = self.write_error.take() {
            return Err(error);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A reply whose header is well-formed but whose body is not decodable.
fn malformed_reply() -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&4u32.to_le_bytes());
    frame.extend_from_slice(&crate::daemon::protocol::PROTOCOL_VERSION.to_le_bytes());
    frame.extend_from_slice(&[0xFF; 4]);
    frame
}

fn deadline_of(error: &ClientError) -> Option<Duration> {
    match error {
        ClientError::Ipc { deadline, .. } => *deadline,
        _ => None,
    }
}

/// RED before soldr#3558: the same `WouldBlock` came back as a bare
/// `Io(..)`, so a failing `cook_record` could not say whether the request
/// left or the reply never arrived.
#[test]
fn a_send_failure_carries_the_request_send_stage_and_its_deadline() {
    let deadline = Duration::from_millis(750);
    let mut stream =
        ScriptedIo::new().fail_write(std::io::Error::from(std::io::ErrorKind::WouldBlock));

    let error = round_trip(&mut stream, &Request::Status, None, deadline)
        .expect_err("a peer that refuses the frame must fail");

    assert_eq!(error.ipc_stage(), Some(IpcStage::RequestSend));
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::WouldBlock));
    assert_eq!(deadline_of(&error), Some(deadline));
}

/// The signature the issue actually reports: the request went out and the
/// reply did not come back inside the budget.
#[test]
fn an_expired_reply_deadline_carries_the_reply_read_stage_and_the_deadline_that_expired() {
    let deadline = Duration::from_secs(2);
    let mut stream =
        ScriptedIo::new().fail_read(std::io::Error::from(std::io::ErrorKind::WouldBlock));

    let error = round_trip(&mut stream, &Request::Status, None, deadline)
        .expect_err("a peer that never answers must fail");

    assert_eq!(error.ipc_stage(), Some(IpcStage::ReplyRead));
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::WouldBlock));
    assert_eq!(deadline_of(&error), Some(deadline));
}

/// A hang-up during the reply is the reply-read stage too: the frame never
/// arrived, so there is nothing to decode.
#[test]
fn a_hung_up_reply_is_the_reply_read_stage() {
    let deadline = Duration::from_secs(2);
    let mut stream = ScriptedIo::new();

    let error = round_trip(&mut stream, &Request::Status, None, deadline)
        .expect_err("an empty stream must fail");

    assert_eq!(error.ipc_stage(), Some(IpcStage::ReplyRead));
    assert_eq!(
        error.io_kind(),
        Some(std::io::ErrorKind::UnexpectedEof),
        "{error:?}"
    );
}

#[test]
fn a_malformed_reply_carries_the_response_decode_stage_without_a_deadline() {
    let mut stream = ScriptedIo::new().reply(malformed_reply());

    let error = round_trip(&mut stream, &Request::Status, None, Duration::from_secs(2))
        .expect_err("an undecodable body must fail");

    assert_eq!(error.ipc_stage(), Some(IpcStage::ResponseDecode));
    // Decode runs on bytes already read; no timeout governs it.
    assert_eq!(deadline_of(&error), None);
}

/// This is the line a failing test's `expect(..)` prints, so it has to name
/// the stage and the budget in one glance.
#[test]
fn the_rendered_failure_names_the_stage_and_the_deadline() {
    let mut stream =
        ScriptedIo::new().fail_read(std::io::Error::from(std::io::ErrorKind::WouldBlock));

    let error = round_trip(&mut stream, &Request::Status, None, Duration::from_secs(2))
        .expect_err("a peer that never answers must fail");

    let rendered = format!("{error:?}");
    assert!(rendered.contains("ReplyRead"), "{rendered}");
    assert!(rendered.contains("WouldBlock"), "{rendered}");
    assert!(rendered.contains("2s"), "{rendered}");
}

/// Kind-based predicates (readiness waits, settle loops) must keep seeing
/// the io kind underneath the tag, or staged failures silently stop
/// matching them.
#[test]
fn the_stage_tag_never_hides_the_io_kind() {
    let error = at_stage(
        IpcStage::ReplyRead,
        Some(Duration::from_secs(2)),
        ClientError::from(std::io::Error::from(std::io::ErrorKind::TimedOut)),
    );

    assert_eq!(error.ipc_stage(), Some(IpcStage::ReplyRead));
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::TimedOut));
    assert!(!matches!(error, ClientError::Io(_)), "{error:?}");
}

/// `NotRunning`, `Protocol` and `VersionMismatch` already state what went
/// wrong; wrapping them would hide them from every caller that matches on
/// them, which is a behaviour change the diagnostic must not smuggle in.
#[test]
fn a_condition_that_already_names_itself_is_never_tagged() {
    let conditions = [
        ClientError::NotRunning,
        ClientError::Protocol("cook_index upsert failed".to_string()),
        ClientError::VersionMismatch("protocol version mismatch: peer=1".to_string()),
    ];
    for condition in conditions {
        let tagged = at_stage(IpcStage::ReplyRead, Some(Duration::from_secs(2)), condition);
        assert_eq!(tagged.ipc_stage(), None, "{tagged:?}");
    }
}

/// A daemon that never started must stay `NotRunning`, not become a staged
/// I/O failure: the "daemon not ready" answer and the "deadline expired"
/// answer have to stay separable (soldr#3558).
#[test]
fn a_missing_endpoint_reports_not_running_without_a_stage() {
    let dir = tempfile::tempdir().expect("temp dir");

    let error = submit_request(&dir.path().join("absent.sock"), &Request::Status)
        .expect_err("an absent endpoint must fail");

    assert!(matches!(error, ClientError::NotRunning), "{error:?}");
    assert_eq!(error.ipc_stage(), None);
}

/// A dial that fails for an I/O reason — here, a regular file where a
/// socket path's parent should be — is the connect stage, with the errno
/// that distinguishes it from `NotRunning`.
///
/// Gated at runtime rather than with `#[cfg(unix)]`: host-platform `cfg`
/// is banned outside `crates/soldr-platform` (#2493). Windows dials a
/// named pipe instead of AF_UNIX, so its absent endpoint answers
/// `NotRunning`, which the test above already covers.
#[test]
fn a_dial_that_cannot_succeed_carries_the_connect_stage() {
    if matches!(
        crate::platform::host::facts::os(),
        crate::platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let not_a_dir = dir.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"").expect("write blocker file");

    let error = submit_request(&not_a_dir.join("control.sock"), &Request::Status)
        .expect_err("connecting through a regular file must fail");

    assert_eq!(error.ipc_stage(), Some(IpcStage::Connect));
    // AF_UNIX connect is unbounded, so the tag reports no deadline.
    assert_eq!(deadline_of(&error), None);
    assert!(error.io_kind().is_some(), "{error:?}");
}
