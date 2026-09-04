//! Child-cargo backtrace policy tests (`apply_library_backtrace_policy`).
//!
//! Split out of the sibling `tests.rs` for the same reason
//! `scrub_pool_tests.rs` was: that file is already past the 1,500-line
//! ceiling, so the ratchet refuses to let it grow.

use super::*;
use std::ffi::{OsStr, OsString};

fn command_env_override(
    command: &std::process::Command,
    key: &'static str,
) -> Option<Option<OsString>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value.map(OsString::from))
}

#[test]
fn child_cargo_quiets_library_backtraces_when_only_rust_backtrace_is_set() {
    let mut command = std::process::Command::new("cargo");
    apply_library_backtrace_policy(&mut command, Some(OsStr::new("1")), None);
    assert_eq!(
        command_env_override(&command, "RUST_LIB_BACKTRACE"),
        Some(Some(OsString::from("0")))
    );
    assert_eq!(command_env_override(&command, "RUST_BACKTRACE"), None);

    let mut command = std::process::Command::new("cargo");
    apply_library_backtrace_policy(&mut command, Some(OsStr::new("full")), None);
    assert_eq!(
        command_env_override(&command, "RUST_LIB_BACKTRACE"),
        Some(Some(OsString::from("0")))
    );
}

#[test]
fn child_cargo_leaves_library_backtraces_alone_otherwise() {
    // Nothing set: anyhow captures nothing already; do not add noise.
    let mut command = std::process::Command::new("cargo");
    apply_library_backtrace_policy(&mut command, None, None);
    assert_eq!(command_env_override(&command, "RUST_LIB_BACKTRACE"), None);

    // RUST_BACKTRACE=0 is "off"; same.
    let mut command = std::process::Command::new("cargo");
    apply_library_backtrace_policy(&mut command, Some(OsStr::new("0")), None);
    assert_eq!(command_env_override(&command, "RUST_LIB_BACKTRACE"), None);

    // An explicit caller choice wins, whatever it is.
    for explicit in ["1", "0", "full"] {
        let mut command = std::process::Command::new("cargo");
        apply_library_backtrace_policy(
            &mut command,
            Some(OsStr::new("1")),
            Some(OsStr::new(explicit)),
        );
        assert_eq!(command_env_override(&command, "RUST_LIB_BACKTRACE"), None);
    }
}
