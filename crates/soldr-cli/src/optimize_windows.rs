//! Windows-specific UAC self-relaunch wrapper for `soldr optimize`.
//! The actual `Add-MpPreference` / `Remove-MpPreference` plumbing now
//! lives in `crate::defender` so both the optimize CLI and
//! `soldr load --auto-defender-exclude` can share it. This module only
//! owns the UAC re-launch helpers, which are specific to the optimize
//! CLI flow.
//!
//! The UAC mechanics themselves live in the platform crate's Windows
//! host tree (`platform::host::user::{relaunch_elevated, …}`); this
//! module keeps the soldr-side constants and the re-export surface.

pub(crate) use crate::defender::{
    apply_exclusions, current_exclusion_list, is_admin, SOLDR_TEST_ASSUME_ADMIN_ENV,
    SOLDR_TEST_DEFENDER_EXISTING_ENV, SOLDR_TEST_DEFENDER_LOG_ENV,
};

/// Environment variable injected by the parent soldr process when it
/// re-launches itself elevated. The helper subprocess writes its JSON
/// status to this path so the parent can read and report it.
pub(crate) const SOLDR_OPTIMIZE_HELPER_OUTPUT_ENV: &str = "SOLDR_OPTIMIZE_HELPER_OUTPUT";

/// Sentinel flag the parent passes to the elevated helper to make it
/// skip its own UAC self-relaunch loop.
pub(crate) const ELEVATED_HELPER_FLAG: &str = "--as-elevated-helper";

/// Relaunch the current soldr binary elevated via UAC. Returns the
/// child's exit code if the relaunch succeeded, or an error explaining
/// why elevation wasn't possible.
pub(crate) fn relaunch_elevated(
    powershell: &std::path::Path,
    args: &[String],
    helper_output_path: &std::path::Path,
) -> Result<i32, String> {
    crate::platform::host::user::relaunch_elevated(
        powershell,
        args,
        helper_output_path,
        SOLDR_OPTIMIZE_HELPER_OUTPUT_ENV,
    )
}
