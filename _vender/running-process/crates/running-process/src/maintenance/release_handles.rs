//! `running-process maintenance release-handles --path <PATH>`.
//!
//! Phase 1 of #228 (issue #230). The goal is to make
//! `rm -rf <PATH>` reliable on Windows even when a daemon process is
//! holding handles inside `<PATH>` — see soldr#710.
//!
//! ## Per-platform behaviour
//!
//! - **POSIX**: no-op. Linux/macOS use delete-on-close semantics, so
//!   `rm -rf` always succeeds; the subcommand exits 0 with an
//!   informational message.
//! - **Windows**: scaffold-only in Phase 1. The full handler depends
//!   on the manifest registry that ships in Phase 2 (#231). Until
//!   then the subcommand returns a successful "no manifests to scan
//!   yet" result so downstream callers (clud-pr, soldr cleanup) can
//!   start wiring the call site without changing their exit-code
//!   handling later.
//!
//! ## Why ship the POSIX no-op now?
//!
//! Cross-platform tooling (soldr's clud-pr workflow, CI helpers) can
//! call the subcommand unconditionally and get the right behaviour on
//! every host. Without the no-op surface, every caller would need its
//! own `cfg(unix)` short-circuit.

use std::path::{Path, PathBuf};

/// Errors emitted by [`run_release_handles`].
#[derive(Debug, thiserror::Error)]
pub enum ReleaseHandlesError {
    /// The supplied `--path` argument was empty.
    #[error("--path must be non-empty")]
    EmptyPath,
}

/// Inputs used to authorize a future daemon-side `release-handles` request.
///
/// `requester_account_id` and `daemon_owner_account_id` are opaque OS account
/// identifiers: UID strings on POSIX and SID strings on Windows. The helper
/// keeps them opaque so tests can exercise the policy without needing a real
/// cross-user setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseHandlesAuthorization<'a> {
    /// UID/SID of the client asking the daemon to release handles.
    pub requester_account_id: &'a str,
    /// UID/SID that owns the daemon holding the target handles.
    pub daemon_owner_account_id: &'a str,
    /// Whether the requester can write to the requested target path.
    pub requester_can_write_target_path: bool,
}

/// Errors emitted by [`authorize_release_handles_request`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseHandlesAuthorizationError {
    /// The requester identity was empty.
    #[error("release-handles requester identity must be non-empty")]
    EmptyRequesterIdentity,
    /// The daemon owner identity was empty.
    #[error("release-handles daemon owner identity must be non-empty")]
    EmptyDaemonOwnerIdentity,
    /// The requester UID/SID did not match the daemon owner's UID/SID.
    #[error("release-handles requester identity does not match daemon owner")]
    OwnerMismatch,
    /// The requester did not have write access to the requested target path.
    #[error("release-handles requester lacks write access to target path")]
    TargetPathWriteDenied,
}

/// Authorize a daemon-side `release-handles` request.
///
/// The policy intentionally checks both ownership and target-path write access:
/// a requester must be the same OS account that owns the daemon and must be
/// able to write to the path it is asking the daemon to free.
pub fn authorize_release_handles_request(
    authorization: ReleaseHandlesAuthorization<'_>,
) -> Result<(), ReleaseHandlesAuthorizationError> {
    if authorization.requester_account_id.trim().is_empty() {
        return Err(ReleaseHandlesAuthorizationError::EmptyRequesterIdentity);
    }
    if authorization.daemon_owner_account_id.trim().is_empty() {
        return Err(ReleaseHandlesAuthorizationError::EmptyDaemonOwnerIdentity);
    }
    if authorization.requester_account_id != authorization.daemon_owner_account_id {
        return Err(ReleaseHandlesAuthorizationError::OwnerMismatch);
    }
    if !authorization.requester_can_write_target_path {
        return Err(ReleaseHandlesAuthorizationError::TargetPathWriteDenied);
    }

    Ok(())
}

/// Result of one `release-handles` invocation.
///
/// Stable across Phase 1 → Phase 2 — Phase 2 will populate the
/// `manifests_scanned` / `handles_released` counters that are zero in
/// the Phase 1 stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseHandlesOutcome {
    /// The path the caller asked us to free up.
    pub path: PathBuf,
    /// Informational human-readable message — printed verbatim by the
    /// CLI when `--json` is not set.
    pub message: String,
    /// Number of manifests we walked. Always 0 in Phase 1.
    pub manifests_scanned: u32,
    /// Number of handle-drop requests we issued. Always 0 in Phase 1.
    pub handles_released: u32,
    /// `true` when no further action is needed (POSIX always returns
    /// `true`; Windows-Phase-1 returns `true` because the manifest
    /// registry doesn't exist yet).
    pub already_clean: bool,
}

impl ReleaseHandlesOutcome {
    /// Render as a JSON object. Stable shape across phases — Phase 2
    /// adds counter values but never adds or removes top-level keys.
    pub fn to_json(&self) -> String {
        // Hand-roll the JSON to avoid pulling serde_json into a code
        // path that needs to be cross-platform clean. Field order is
        // chosen for grep-ability.
        format!(
            "{{\
\"path\":\"{path}\",\
\"manifests_scanned\":{manifests},\
\"handles_released\":{handles},\
\"already_clean\":{clean},\
\"message\":\"{message}\"\
}}",
            path = json_escape(&self.path.to_string_lossy()),
            manifests = self.manifests_scanned,
            handles = self.handles_released,
            clean = self.already_clean,
            message = json_escape(&self.message),
        )
    }
}

/// Run the `release-handles` subcommand. Cross-platform entrypoint
/// called by `runpm maintenance release-handles`.
pub fn run_release_handles(path: &Path) -> Result<ReleaseHandlesOutcome, ReleaseHandlesError> {
    let path_str = path.to_string_lossy();
    if path_str.trim().is_empty() {
        return Err(ReleaseHandlesError::EmptyPath);
    }

    #[cfg(unix)]
    {
        Ok(ReleaseHandlesOutcome {
            path: path.to_path_buf(),
            message: format!(
                "POSIX delete-on-close semantics make this a no-op; proceed with `rm -rf {path_str}`"
            ),
            manifests_scanned: 0,
            handles_released: 0,
            already_clean: true,
        })
    }

    #[cfg(windows)]
    {
        // Phase 1 stub. Phase 2 (#231) ships the manifest registry
        // under `%LOCALAPPDATA%\running-process\manifests\` and the
        // full handler will enumerate that directory + send
        // `MaintenanceRequest::ReleaseHandles { path_prefix }` over
        // each live daemon's pipe. For now we return a successful
        // "nothing to do" result so callers can wire the integration
        // unconditionally.
        Ok(ReleaseHandlesOutcome {
            path: path.to_path_buf(),
            message: format!(
                "Phase 2 manifest registry not yet shipped; no daemons to query for handles under \
                 {path_str}. Proceed with rm -rf and report soldr#710 reproductions if encountered."
            ),
            manifests_scanned: 0,
            handles_released: 0,
            already_clean: true,
        })
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_returns_error() {
        let err = run_release_handles(Path::new("")).unwrap_err();
        match err {
            ReleaseHandlesError::EmptyPath => {}
        }
    }

    #[test]
    fn non_empty_path_returns_ok() {
        let outcome = run_release_handles(Path::new("/tmp/example")).expect("ok");
        assert_eq!(outcome.path, PathBuf::from("/tmp/example"));
        assert!(outcome.already_clean);
        assert_eq!(outcome.manifests_scanned, 0);
        assert_eq!(outcome.handles_released, 0);
    }

    #[test]
    fn json_output_has_stable_keys() {
        let outcome = run_release_handles(Path::new("/tmp/example")).expect("ok");
        let json = outcome.to_json();
        assert!(json.contains("\"path\":"));
        assert!(json.contains("\"manifests_scanned\":"));
        assert!(json.contains("\"handles_released\":"));
        assert!(json.contains("\"already_clean\":"));
        assert!(json.contains("\"message\":"));
    }
}
