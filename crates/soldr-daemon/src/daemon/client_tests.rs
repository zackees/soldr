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
