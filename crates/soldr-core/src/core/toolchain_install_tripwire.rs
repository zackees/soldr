//! Test-only tripwire that turns a Rust toolchain download into a named error
//! (soldr#3195).
//!
//! Two tests downloaded toolchains on every CI gate. Each faked every tool
//! except `rustup`, pinned a channel the runner did not have, and so the real
//! rustup installed it: 1.6 GB and ~30 s per gate for one of them. Nothing
//! failed, because a download is time and bandwidth rather than a red test, and
//! a dev box that already had the channel never noticed.
//!
//! Every soldr entry point that can install a toolchain, component or target,
//! or bootstrap rustup itself, calls [`forbid_toolchain_install_tripwire`]
//! first. The nextest wrapper (`.github/scripts/nextest_timeout_wrapper.py`)
//! sets [`FORBID_TOOLCHAIN_INSTALL_ENV_VAR`] for every test process, so such a
//! path now fails the test with this diagnostic instead of downloading. A test
//! that exercises an install on purpose supplies a fake rustup through
//! [`TEST_RUSTUP_BIN_ENV_VAR`], which never downloads, and is allowed through.
//! Same shape as `SOLDR_TEST_FORBID_SOURCE_BUILD`. Never set outside tests.

use super::SoldrError;
use std::ffi::OsStr;

/// Set truthy for test processes; see the module docs.
pub const FORBID_TOOLCHAIN_INSTALL_ENV_VAR: &str = "SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL";

/// The test-only fake rustup override `rustup_binary()` honours.
pub const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";

/// Whether an install through the real rustup is forbidden: the tripwire is on
/// and no fake rustup is configured. Pure, so it can be tested without
/// mutating the process environment.
pub fn toolchain_install_forbidden(tripwire: Option<&str>, fake_rustup: Option<&OsStr>) -> bool {
    tripwire.is_some_and(super::flag_value) && fake_rustup.is_none_or(OsStr::is_empty)
}

/// Refuse `action` when it would download a toolchain under test.
pub fn forbid_toolchain_install_tripwire(action: &str) -> Result<(), SoldrError> {
    let tripwire = std::env::var(FORBID_TOOLCHAIN_INSTALL_ENV_VAR).ok();
    let fake_rustup = std::env::var_os(TEST_RUSTUP_BIN_ENV_VAR);
    if toolchain_install_forbidden(tripwire.as_deref(), fake_rustup.as_deref()) {
        return Err(SoldrError::Other(format!(
            "test tripwire: `{action}` would download a Rust toolchain, but \
             {FORBID_TOOLCHAIN_INSTALL_ENV_VAR} is set; give the test a fake rustup \
             through {TEST_RUSTUP_BIN_ENV_VAR} (soldr#3195)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_or_falsy_tripwire_never_forbids() {
        for tripwire in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
        ] {
            assert!(!toolchain_install_forbidden(tripwire, None), "{tripwire:?}");
        }
    }

    #[test]
    fn an_armed_tripwire_forbids_the_real_rustup() {
        for tripwire in ["1", "true", "yes", "on"] {
            assert!(
                toolchain_install_forbidden(Some(tripwire), None),
                "{tripwire}"
            );
            assert!(
                toolchain_install_forbidden(Some(tripwire), Some(OsStr::new(""))),
                "an empty override is not a fake rustup"
            );
        }
    }

    #[test]
    fn a_fake_rustup_is_allowed_through_an_armed_tripwire() {
        assert!(!toolchain_install_forbidden(
            Some("1"),
            Some(OsStr::new("/tmp/fake/rustup"))
        ));
    }
}
