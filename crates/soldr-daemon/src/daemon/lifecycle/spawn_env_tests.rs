//! Unit coverage for the daemon spawn allowlist in `spawn_env.rs`. Pure:
//! drives `filter_forwarded_env` / `forwarded_env_name` without touching the
//! process environment.

use super::*;
use std::ffi::{OsStr, OsString};

fn forwarded(name: &str) -> bool {
    forwarded_env_name(OsStr::new(name))
}

#[test]
fn cache_size_names_are_forwarded() {
    // soldr#3503: the daemon reads these, so they must cross the scrub.
    assert!(forwarded(crate::zccache_embedded::CACHE_SIZE_BYTES_ENV));
    assert!(forwarded(crate::zccache_embedded::CACHE_SIZE_PERCENT_ENV));
}

#[test]
fn zccache_disable_is_not_forwarded() {
    assert!(!forwarded("ZCCACHE_DISABLE"));
}

#[test]
fn every_allowlist_entry_is_forwarded_in_any_casing() {
    for name in FORWARDED_ZCCACHE_ENV {
        assert!(forwarded(name), "{name} must be forwarded");
        assert!(
            forwarded(&name.to_ascii_lowercase()),
            "{name} must be accepted in lowercase"
        );
    }
}

#[test]
fn unrelated_names_are_not_forwarded() {
    assert!(!forwarded("PATH"));
    assert!(!forwarded("ZCCACHE_SOMETHING_ELSE"));
}

#[test]
fn filter_keeps_only_allowlisted_pairs() {
    let vars = [
        (crate::zccache_embedded::CACHE_SIZE_BYTES_ENV, "1"),
        ("ZCCACHE_DISABLE", "1"),
        ("PATH", "/bin"),
    ]
    .map(|(k, v)| (OsString::from(k), OsString::from(v)));
    let kept = filter_forwarded_env(vars);
    assert_eq!(
        kept,
        vec![(
            OsString::from(crate::zccache_embedded::CACHE_SIZE_BYTES_ENV),
            OsString::from("1")
        )]
    );
}
