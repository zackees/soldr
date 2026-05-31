//! Windows-specific UAC self-relaunch wrapper for `soldr optimize`.
//! The actual `Add-MpPreference` / `Remove-MpPreference` plumbing now
//! lives in `crate::defender` so both the optimize CLI and
//! `soldr load --auto-defender-exclude` can share it. This module only
//! owns the UAC re-launch helpers, which are specific to the optimize
//! CLI flow.

#[cfg(target_os = "windows")]
use std::path::Path;
#[cfg(target_os = "windows")]
use std::process::Command;

pub(crate) use crate::defender::{
    apply_exclusions, current_exclusion_list, is_admin, SOLDR_TEST_ASSUME_ADMIN_ENV,
    SOLDR_TEST_DEFENDER_EXISTING_ENV, SOLDR_TEST_DEFENDER_LOG_ENV,
};

/// Environment variable injected by the parent soldr process when it
/// re-launches itself elevated. The helper subprocess writes its JSON
/// status to this path so the parent can read and report it.
#[cfg(target_os = "windows")]
pub(crate) const SOLDR_OPTIMIZE_HELPER_OUTPUT_ENV: &str = "SOLDR_OPTIMIZE_HELPER_OUTPUT";

/// Sentinel flag the parent passes to the elevated helper to make it
/// skip its own UAC self-relaunch loop.
#[cfg(target_os = "windows")]
pub(crate) const ELEVATED_HELPER_FLAG: &str = "--as-elevated-helper";

/// Relaunch the current soldr binary elevated via UAC. Returns the
/// child's exit code if the relaunch succeeded, or an error explaining
/// why elevation wasn't possible.
#[cfg(target_os = "windows")]
pub(crate) fn relaunch_elevated(
    powershell: &Path,
    args: &[String],
    helper_output_path: &Path,
) -> Result<i32, String> {
    let current =
        std::env::current_exe().map_err(|e| format!("failed to resolve current binary: {e}"))?;
    let arg_list = build_powershell_arg_list(args);
    // Why: `-Wait` blocks the parent until the elevated child exits, and
    // `-PassThru` lets us capture its exit code.
    let exe_quoted = ps_quote(&current.display().to_string());
    let output_env_quoted = ps_quote(&helper_output_path.display().to_string());
    let script = format!(
        "$env:{env_name}={out_lit}; $p = Start-Process -FilePath {exe} -ArgumentList {args} -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        env_name = SOLDR_OPTIMIZE_HELPER_OUTPUT_ENV,
        out_lit = output_env_quoted,
        exe = exe_quoted,
        args = arg_list,
    );
    let status = Command::new(powershell)
        .args(["-NoProfile", "-Command", &script])
        .status()
        .map_err(|e| format!("failed to relaunch elevated: {e}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(target_os = "windows")]
fn build_powershell_arg_list(args: &[String]) -> String {
    if args.is_empty() {
        // Why: PowerShell rejects empty Start-Process ArgumentList; pass
        // a single empty string so the elevated process gets argc == 1.
        return "@(' ')".to_string();
    }
    let parts: Vec<String> = args.iter().map(|a| ps_quote(a)).collect();
    format!("@({})", parts.join(","))
}

#[cfg(target_os = "windows")]
fn ps_quote(value: &str) -> String {
    let escaped = value.replace('\'', "''");
    format!("'{}'", escaped)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("a'b"), "'a''b'");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn build_powershell_arg_list_emits_array_literal() {
        let args = vec!["optimize".to_string(), "--scope".to_string()];
        assert_eq!(build_powershell_arg_list(&args), "@('optimize','--scope')");
    }
}
