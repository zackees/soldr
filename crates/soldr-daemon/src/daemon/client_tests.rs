//! Unit coverage split from `client.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A test-only peer that accepts a frame but closes before acknowledging it.
///
/// This lives in the daemon client's unit-test binary because the connector
/// seam is a process-global `OnceLock`. Installing it in the consolidated
/// daemon integration target made plain workspace tests route sibling tests
/// to this silent peer instead of their real daemon (soldr#2955).
struct SilentPeer {
    written: Arc<AtomicUsize>,
}

struct SilentStream {
    written: Arc<AtomicUsize>,
}

impl Read for SilentStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(0)
    }
}

impl Write for SilentStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.fetch_add(buf.len(), Ordering::SeqCst);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ControlConnector for SilentPeer {
    fn connect(
        &self,
        _endpoint_marker: &std::path::Path,
        _timeout: Duration,
    ) -> std::io::Result<BoxedControlStream> {
        Ok(Box::new(SilentStream {
            written: Arc::clone(&self.written),
        }))
    }
}

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

/// A missing receipt acknowledgement is diagnostic-only on the wrapper hot
/// path: old daemons and a peer that disappears after accepting the frame must
/// not turn a target touch into a hard compiler failure.
#[test]
fn a_peer_that_never_acks_is_still_a_successful_submit() {
    let written = Arc::new(AtomicUsize::new(0));
    install_control_connector(Arc::new(SilentPeer {
        written: Arc::clone(&written),
    }))
    .expect("the daemon client unit-test process owns this connector seam");

    let result = submit_fire_and_forget(
        std::path::Path::new("endpoint-marker-unused-by-the-override"),
        &Request::RecordTargetTouch {
            path: "/some/workspace/target".to_string(),
            unix_seconds: 1_700_000_000,
        },
    );

    assert!(
        result.is_ok(),
        "a missing ack must stay best-effort, not become an error: {result:?}"
    );
    assert!(
        written.load(Ordering::SeqCst) > 0,
        "the request frame should still have been written to the peer"
    );
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
