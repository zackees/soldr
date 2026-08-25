//! Unit coverage split from `client.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

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
