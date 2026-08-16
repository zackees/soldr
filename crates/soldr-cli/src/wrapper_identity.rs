//! Soldr-owned effective-wrapper identity mirror (soldr#2545).
//!
//! Cargo fingerprints the `RUSTC_WRAPPER` executable path; changing it
//! between invocations silently invalidates every otherwise-warm artifact
//! and turns a warm build into a full-workspace recompile with no error.
//! The observed failure was two Soldr generations driving one shared
//! `target/`: each wrote a different versioned shim path and the "hang"
//! before tests was cargo rebuilding the world, twice.
//!
//! `SOLDR_RUSTC_WRAPPER` stays the public policy *input* and is untouched.
//! This module owns a private exact mirror of the *resolved* value: every
//! boundary where Soldr sets or clears `RUSTC_WRAPPER` on a child does it
//! through these helpers so the pair can never drift within Soldr-owned
//! lineage, and the wrapper re-entry asserts the inherited pair still
//! matches before any broker/daemon contact. Caller-owned wrappers are
//! deliberately not mirrored: Soldr asserts only what Soldr owns.

use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::core::SoldrError;

/// Private exact mirror of the effective `RUSTC_WRAPPER` Soldr resolved.
pub const EFFECTIVE_WRAPPER_ENV: &str = "SOLDR_EFFECTIVE_RUSTC_WRAPPER";

/// Which Soldr path produced the effective wrapper value.
pub const EFFECTIVE_WRAPPER_ORIGIN_ENV: &str = "SOLDR_EFFECTIVE_RUSTC_WRAPPER_ORIGIN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperOrigin {
    /// The managed compiler-named multicall shim (the normal cached path).
    SoldrManaged,
    /// A caller-supplied `SOLDR_RUSTC_WRAPPER` override Soldr applied.
    CustomOverride,
    /// The build-from-source cache shim.
    SourceBuild,
    /// Soldr explicitly cleared the wrapper (caching disabled).
    Disabled,
}

impl WrapperOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SoldrManaged => "soldr-managed",
            Self::CustomOverride => "custom-override",
            Self::SourceBuild => "source-build",
            Self::Disabled => "disabled",
        }
    }
}

/// Set `RUSTC_WRAPPER` and its private mirror from the same bytes.
///
/// The single `OsStr` source is the point: no `String` round trip, so
/// non-UTF-8 paths mirror byte-for-byte on Unix.
pub fn set_owned_rustc_wrapper(command: &mut Command, wrapper: &OsStr, origin: WrapperOrigin) {
    command.env("RUSTC_WRAPPER", wrapper);
    command.env(EFFECTIVE_WRAPPER_ENV, wrapper);
    command.env(EFFECTIVE_WRAPPER_ORIGIN_ENV, origin.as_str());
}

/// Remove `RUSTC_WRAPPER` and the mirror together, recording why.
pub fn remove_owned_rustc_wrapper(command: &mut Command) {
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove(EFFECTIVE_WRAPPER_ENV);
    command.env(
        EFFECTIVE_WRAPPER_ORIGIN_ENV,
        WrapperOrigin::Disabled.as_str(),
    );
}

/// Fail closed when Soldr-owned wrapper state drifted (soldr#2545).
///
/// Reads the *process* environment: called at re-entry boundaries where the
/// current process inherited a Soldr-owned lineage. A mirror with no (or a
/// different) `RUSTC_WRAPPER` means something between the owning Soldr and
/// this process mutated exactly the pair this invariant exists to protect —
/// continuing would hand cargo a different wrapper identity and silently
/// recompile the world. Caller-owned or unmirrored environments pass.
pub fn assert_inherited_wrapper_coherent(boundary: &str) -> Result<(), SoldrError> {
    let Some(mirror) = std::env::var_os(EFFECTIVE_WRAPPER_ENV) else {
        return Ok(());
    };
    let origin = std::env::var(EFFECTIVE_WRAPPER_ORIGIN_ENV)
        .unwrap_or_else(|_| "<origin missing>".to_string());
    if origin == WrapperOrigin::Disabled.as_str() {
        // A disabled origin leaves no mirror; seeing one anyway is drift.
        return Err(drift_error(boundary, &origin, Some(&mirror), None));
    }
    match std::env::var_os("RUSTC_WRAPPER") {
        Some(actual) if actual == mirror => Ok(()),
        actual => Err(drift_error(
            boundary,
            &origin,
            Some(&mirror),
            actual.as_deref(),
        )),
    }
}

fn drift_error(
    boundary: &str,
    origin: &str,
    mirror: Option<&OsStr>,
    actual: Option<&OsStr>,
) -> SoldrError {
    let show = |value: Option<&OsStr>| {
        value
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unset>".to_string())
    };
    SoldrError::Other(format!(
        "soldr-owned RUSTC_WRAPPER identity drifted (observed at {boundary}): \
         {EFFECTIVE_WRAPPER_ENV}={} ({EFFECTIVE_WRAPPER_ORIGIN_ENV}={origin}) but \
         RUSTC_WRAPPER={}. Cargo fingerprints the wrapper path, so continuing \
         would silently invalidate every warm artifact and recompile the \
         workspace (soldr#2545). Fix whatever mutated one of the pair without \
         the other; Soldr never does.",
        show(mirror),
        show(actual),
    ))
}

/// Owned-state view for callers that only need to report identity.
pub fn inherited_identity() -> Option<(OsString, String)> {
    let mirror = std::env::var_os(EFFECTIVE_WRAPPER_ENV)?;
    let origin = std::env::var(EFFECTIVE_WRAPPER_ORIGIN_ENV).ok()?;
    Some((mirror, origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_pairs(command: &Command) -> Vec<(String, Option<OsString>)> {
        command
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(OsString::from)))
            .collect()
    }

    #[test]
    fn set_updates_wrapper_and_mirror_byte_for_byte() {
        let mut command = Command::new("true");
        let path = OsString::from("/root/.soldr/v9.9.9/shims/rustc");
        set_owned_rustc_wrapper(&mut command, &path, WrapperOrigin::SoldrManaged);
        let envs = env_pairs(&command);
        let get = |name: &str| {
            envs.iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| v.clone())
        };
        assert_eq!(get("RUSTC_WRAPPER"), Some(path.clone()));
        assert_eq!(get(EFFECTIVE_WRAPPER_ENV), Some(path));
        assert_eq!(
            get(EFFECTIVE_WRAPPER_ORIGIN_ENV),
            Some(OsString::from("soldr-managed"))
        );
    }

    #[test]
    fn remove_clears_both_and_records_disabled() {
        let mut command = Command::new("true");
        set_owned_rustc_wrapper(
            &mut command,
            OsStr::new("/x/rustc"),
            WrapperOrigin::SoldrManaged,
        );
        remove_owned_rustc_wrapper(&mut command);
        let envs = env_pairs(&command);
        let get = |name: &str| {
            envs.iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("RUSTC_WRAPPER"), Some(None), "removed, not just unset");
        assert_eq!(get(EFFECTIVE_WRAPPER_ENV), Some(None));
        assert_eq!(
            get(EFFECTIVE_WRAPPER_ORIGIN_ENV).flatten(),
            Some(OsString::from("disabled"))
        );
    }

    // The process-env assertion is covered by the integration test
    // (cli_wrapper_identity.rs), which owns real child environments;
    // mutating this process's env here would race sibling tests.
}
