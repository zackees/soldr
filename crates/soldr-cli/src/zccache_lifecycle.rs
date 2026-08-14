//! Shared zccache subprocess helpers and output classifiers.
//!
//! Historically this module owned the managed-zccache daemon/session state
//! machine (`ZccacheLifecycle`). soldr#1368 moved rustc compile caching into
//! the soldr-daemon embedded zccache service, so what remains here is the
//! build-session carrier struct plus the subprocess plumbing still used by
//! the rust-plan save/restore path and `soldr cache report`.

use crate::core::{suppress_windows_console_window, SoldrError};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub(crate) const ZCCACHE_DAEMON_NAMESPACE_ENV_VAR: &str = "ZCCACHE_DAEMON_NAMESPACE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZccachePrivateEnv {
    pub(crate) key: String,
    pub(crate) value: String,
}

impl ZccachePrivateEnv {
    pub(crate) fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheBuildSession {
    pub(crate) cache_dir: PathBuf,
    pub(crate) cache_dir_env: bool,
    pub(crate) session_id: String,
    pub(crate) session_log_path: PathBuf,
    pub(crate) journal_path: PathBuf,
    pub(crate) session_stats_path: PathBuf,
}

pub(crate) struct CommandOutput {
    pub(crate) stdout: String,
}

pub(crate) fn run_zccache_command_strings_in_cache_dir_with_daemon_name(
    binary: &Path,
    args: &[String],
    cache_dir: &Path,
    cache_dir_env: bool,
    daemon_name: Option<&str>,
) -> Result<CommandOutput, SoldrError> {
    let envs = cache_dir_envs(cache_dir, cache_dir_env, daemon_name);
    let output = run_zccache_command_raw_strings_with_env(binary, args, &envs, cache_dir_env)?;
    if !output.status.success() {
        return Err(SoldrError::Other(zccache_command_failure_message_strings(
            args, &output,
        )));
    }

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn run_zccache_command_raw_strings_with_env(
    binary: &Path,
    args: &[String],
    envs: &[(&str, &OsStr)],
    cache_dir_env: bool,
) -> Result<Output, SoldrError> {
    let mut command = Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    apply_zccache_env_removals(&mut command, cache_dir_env);
    suppress_windows_console_window(&mut command);
    Ok(command.output()?)
}

fn cache_dir_envs<'a>(
    cache_dir: &'a Path,
    cache_dir_env: bool,
    daemon_name: Option<&'a str>,
) -> Vec<(&'static str, &'a OsStr)> {
    let mut envs = Vec::new();
    if cache_dir_env {
        envs.push((
            crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        ));
    }
    if let Some(daemon_name) = daemon_name {
        envs.push((ZCCACHE_DAEMON_NAMESPACE_ENV_VAR, OsStr::new(daemon_name)));
    }
    envs
}

fn apply_zccache_env_removals(command: &mut Command, cache_dir_env: bool) {
    if cache_dir_env {
        return;
    }
    command.env_remove(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR);
    command.env_remove(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
    command.env_remove(ZCCACHE_DAEMON_NAMESPACE_ENV_VAR);
}

pub(crate) const ZCCACHE_ANALYZE_NOTE_LIMIT: usize = 1000;

pub(crate) fn zccache_output_snippet(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut compact = String::new();
    let mut previous_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !previous_was_space {
                compact.push(' ');
                previous_was_space = true;
            }
        } else {
            compact.push(ch);
            previous_was_space = false;
        }
    }

    let mut chars = compact.chars();
    let mut snippet: String = chars.by_ref().take(ZCCACHE_ANALYZE_NOTE_LIMIT).collect();
    if chars.next().is_some() {
        snippet.push_str("...");
    }
    Some(snippet)
}

/// Returns `true` iff `stderr` contains the literal substring
/// `unknown session:` somewhere in its bytes. Tolerates non-UTF-8 input.
pub(crate) fn stderr_indicates_unknown_session(stderr: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"unknown session:";
    if stderr.len() < NEEDLE.len() {
        return false;
    }
    stderr.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

fn zccache_command_failure_message_strings(args: &[String], output: &Output) -> String {
    format!(
        "zccache {} failed: {}",
        args.join(" "),
        command_stderr(output)
    )
}

pub(crate) fn command_stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_snippet_omits_empty_output() {
        assert_eq!(zccache_output_snippet(b""), None);
        assert_eq!(zccache_output_snippet(b" \n\t "), None);
    }

    #[test]
    fn output_snippet_compacts_whitespace() {
        assert_eq!(
            zccache_output_snippet(b"  first line\n\nsecond\tline  ").as_deref(),
            Some("first line second line")
        );
    }

    #[test]
    fn output_snippet_truncates_long_output() {
        let output = "x".repeat(ZCCACHE_ANALYZE_NOTE_LIMIT + 10);
        let snippet = zccache_output_snippet(output.as_bytes()).unwrap();
        assert_eq!(snippet.len(), ZCCACHE_ANALYZE_NOTE_LIMIT + 3);
        assert!(snippet.ends_with("..."));
    }

    #[test]
    fn unknown_session_detector_matches_exact_zccache_line() {
        let stderr = b"zccache error: unknown session: abc-123\n";
        assert!(stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_matches_substring_not_line_shape() {
        let stderr = b"prelude blah blah unknown session: 0000 trailing\n";
        assert!(stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_does_not_match_other_session_text() {
        let stderr = b"zccache info: session started\nzccache info: session ok\n";
        assert!(!stderr_indicates_unknown_session(stderr));
    }
}
