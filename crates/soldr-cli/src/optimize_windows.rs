//! Windows-specific Defender exclusion logic for `soldr optimize`. The
//! Rust side handles detection, decision, and UAC self-relaunch; the
//! actual `Add-MpPreference` / `Remove-MpPreference` calls are issued
//! through PowerShell.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::optimize::{ActionStatus, ExclusionAction, PathAction};

/// Environment variable injected by the parent soldr process when it
/// re-launches itself elevated. The helper subprocess writes its JSON
/// status to this path so the parent can read and report it.
pub(crate) const SOLDR_OPTIMIZE_HELPER_OUTPUT_ENV: &str = "SOLDR_OPTIMIZE_HELPER_OUTPUT";

/// Sentinel flag the parent passes to the elevated helper to make it
/// skip its own UAC self-relaunch loop.
pub(crate) const ELEVATED_HELPER_FLAG: &str = "--as-elevated-helper";

/// Test seam: when set, the helper that would normally call
/// `Add-MpPreference` / `Remove-MpPreference` is replaced with a
/// no-op that records what it would have done into the file pointed
/// to by this env var (one TSV line per call:
/// `op\tpath\n`). Used by the integration tests.
pub(crate) const SOLDR_TEST_DEFENDER_LOG_ENV: &str = "SOLDR_TEST_DEFENDER_LOG";

/// Test seam: when set, treats the current process as if it is
/// running with administrator privileges regardless of the real
/// token. Bypasses the UAC self-relaunch path in tests.
pub(crate) const SOLDR_TEST_ASSUME_ADMIN_ENV: &str = "SOLDR_TEST_ASSUME_ADMIN";

/// Test seam: when set, returns the contents of this file as the
/// "current Defender exclusion list" (one path per line) instead of
/// running `Get-MpPreference`.
pub(crate) const SOLDR_TEST_DEFENDER_EXISTING_ENV: &str = "SOLDR_TEST_DEFENDER_EXISTING";

/// Check whether the current process holds an administrator token.
/// On Windows we try a registry read of `HKLM\SECURITY` — only admin
/// processes can open this key. A read failure means non-admin.
#[cfg(target_os = "windows")]
pub(crate) fn is_admin() -> bool {
    if std::env::var_os(SOLDR_TEST_ASSUME_ADMIN_ENV).is_some() {
        return true;
    }
    // `reg query HKLM\SECURITY` errors with access denied for non-admin.
    Command::new("reg")
        .args(["query", r"HKLM\SECURITY"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn is_admin() -> bool {
    false
}

/// Apply the given exclusion plan via PowerShell. Returns the per-path
/// outcomes for the JSON / human-readable output. The caller is
/// responsible for deciding whether elevation is needed before this is
/// invoked.
pub(crate) fn apply_exclusions(
    powershell: &Path,
    plan: &[PathAction],
    existing_exclusions: &[String],
) -> Vec<PathAction> {
    let test_log_path = std::env::var_os(SOLDR_TEST_DEFENDER_LOG_ENV).map(PathBuf::from);
    plan.iter()
        .map(|action| {
            let mut out = action.clone();
            let already_excluded = exclusion_list_contains(existing_exclusions, &action.path);
            match action.action {
                ExclusionAction::Add => {
                    if already_excluded {
                        out.status = ActionStatus::AlreadyApplied;
                        out.detail = Some("already on Defender exclusion list".into());
                        return out;
                    }
                    match run_defender_command(
                        powershell,
                        "Add-MpPreference",
                        &action.path,
                        test_log_path.as_deref(),
                    ) {
                        Ok(()) => {
                            out.status = ActionStatus::Applied;
                        }
                        Err(err) => {
                            out.status = ActionStatus::Failed;
                            out.detail = Some(err);
                        }
                    }
                }
                ExclusionAction::Remove => {
                    if !already_excluded {
                        out.status = ActionStatus::Skipped;
                        out.detail = Some("not present in Defender exclusion list".into());
                        return out;
                    }
                    match run_defender_command(
                        powershell,
                        "Remove-MpPreference",
                        &action.path,
                        test_log_path.as_deref(),
                    ) {
                        Ok(()) => {
                            out.status = ActionStatus::Applied;
                        }
                        Err(err) => {
                            out.status = ActionStatus::Failed;
                            out.detail = Some(err);
                        }
                    }
                }
            }
            out
        })
        .collect()
}

fn exclusion_list_contains(list: &[String], path: &str) -> bool {
    let needle = normalize_path(path);
    list.iter().any(|p| normalize_path(p) == needle)
}

fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

fn run_defender_command(
    powershell: &Path,
    cmdlet: &str,
    path: &str,
    test_log: Option<&Path>,
) -> Result<(), String> {
    if let Some(log) = test_log {
        let line = format!("{cmdlet}\t{path}\n");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| format!("failed to open test log: {e}"))?
            .write_all_str(&line)?;
        return Ok(());
    }
    let escaped = path.replace('\'', "''");
    let script = format!("{cmdlet} -ExclusionPath '{escaped}'");
    let output = Command::new(powershell)
        .args(["-NoProfile", "-Command", &script])
        .output()
        .map_err(|e| format!("failed to invoke PowerShell: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{cmdlet} exited with {}: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ))
    }
}

trait WriteAllStr {
    fn write_all_str(&mut self, s: &str) -> Result<(), String>;
}

impl WriteAllStr for std::fs::File {
    fn write_all_str(&mut self, s: &str) -> Result<(), String> {
        use std::io::Write;
        self.write_all(s.as_bytes())
            .map_err(|e| format!("failed to write defender test log: {e}"))
    }
}

/// Read the current Defender exclusion list. Requires admin in production;
/// tests can shim the result via `SOLDR_TEST_DEFENDER_EXISTING`.
pub(crate) fn current_exclusion_list(powershell: &Path) -> Vec<String> {
    if let Some(path) = std::env::var_os(SOLDR_TEST_DEFENDER_EXISTING_ENV) {
        return std::fs::read_to_string(&path)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }
    let output = Command::new(powershell)
        .args([
            "-NoProfile",
            "-Command",
            "(Get-MpPreference).ExclusionPath | ForEach-Object { $_ }",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

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
    // Use `-Wait` so the parent blocks until the elevated child exits,
    // and `-PassThru` so we can capture the child's exit code.
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

#[cfg(not(target_os = "windows"))]
pub(crate) fn relaunch_elevated(
    _powershell: &Path,
    _args: &[String],
    _helper_output_path: &Path,
) -> Result<i32, String> {
    Err("UAC self-relaunch is only supported on Windows".into())
}

fn build_powershell_arg_list(args: &[String]) -> String {
    if args.is_empty() {
        // PowerShell rejects empty Start-Process ArgumentList; pass a
        // single empty string so the elevated process gets argc == 1.
        return "@(' ')".to_string();
    }
    let parts: Vec<String> = args.iter().map(|a| ps_quote(a)).collect();
    format!("@({})", parts.join(","))
}

fn ps_quote(value: &str) -> String {
    let escaped = value.replace('\'', "''");
    format!("'{}'", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusion_list_contains_normalizes_separators_and_case() {
        let list = vec!["C:\\Users\\You\\.soldr\\Cache".to_string()];
        assert!(exclusion_list_contains(&list, "c:/users/you/.soldr/cache"));
    }

    #[test]
    fn ps_quote_doubles_single_quotes() {
        assert_eq!(ps_quote("a'b"), "'a''b'");
    }

    #[test]
    fn build_powershell_arg_list_emits_array_literal() {
        let args = vec!["optimize".to_string(), "--scope".to_string()];
        assert_eq!(build_powershell_arg_list(&args), "@('optimize','--scope')");
    }
}
