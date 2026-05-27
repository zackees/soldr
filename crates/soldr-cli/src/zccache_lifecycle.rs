//! Shared zccache daemon/session lifecycle helpers.
//!
//! This module owns the state transitions and output classifiers used by the
//! cargo front door, `soldr session`, `soldr cache`, wrapper retry handling,
//! and soldr-daemon linked shutdown.

use crate::core::{suppress_windows_console_window, SoldrError};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZccacheLifecycleState {
    Ready,
    DaemonStarted,
    SessionActive,
    SessionEnded,
    Stopped,
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheBuildSession {
    pub(crate) binary_path: PathBuf,
    pub(crate) cache_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) session_log_path: PathBuf,
    pub(crate) journal_path: PathBuf,
    pub(crate) session_stats_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheSessionStartOptions {
    pub(crate) id: Option<String>,
    pub(crate) session_log_path: PathBuf,
    pub(crate) journal_path: PathBuf,
    pub(crate) session_stats_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheSessionEndOutcome {
    pub(crate) stdout: String,
    pub(crate) already_ended: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheStopOutcome {
    pub(crate) stopped: bool,
    pub(crate) already_stopped: bool,
    pub(crate) unsupported_no_depgraph_save: bool,
    pub(crate) failure: Option<String>,
}

impl ZccacheStopOutcome {
    pub(crate) fn stopped() -> Self {
        Self {
            stopped: true,
            already_stopped: false,
            unsupported_no_depgraph_save: false,
            failure: None,
        }
    }

    fn already_stopped() -> Self {
        Self {
            stopped: false,
            already_stopped: true,
            unsupported_no_depgraph_save: false,
            failure: None,
        }
    }

    fn failed(message: String) -> Self {
        Self {
            stopped: false,
            already_stopped: false,
            unsupported_no_depgraph_save: false,
            failure: Some(message),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheFlushOutcome {
    pub(crate) flushed: bool,
    pub(crate) stats: Option<serde_json::Value>,
    pub(crate) json_unsupported: bool,
    pub(crate) subcommand_unsupported: bool,
    pub(crate) already_stopped: bool,
    pub(crate) invalid_json: Option<String>,
    pub(crate) failure: Option<String>,
}

impl ZccacheFlushOutcome {
    fn no_op() -> Self {
        Self {
            flushed: false,
            stats: None,
            json_unsupported: false,
            subcommand_unsupported: false,
            already_stopped: false,
            invalid_json: None,
            failure: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ZccacheDaemonExitPollResult {
    Exited,
    TimedOut,
    PollFailed(String),
}

#[derive(Debug, Clone)]
pub(crate) struct ZccacheLifecycle {
    binary_path: PathBuf,
    cache_dir: PathBuf,
    state: ZccacheLifecycleState,
}

impl ZccacheLifecycle {
    pub(crate) fn new(binary_path: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            cache_dir: cache_dir.into(),
            state: ZccacheLifecycleState::Ready,
        }
    }

    pub(crate) fn from_session(session: &ZccacheBuildSession) -> Self {
        Self {
            binary_path: session.binary_path.clone(),
            cache_dir: session.cache_dir.clone(),
            state: ZccacheLifecycleState::SessionActive,
        }
    }

    pub(crate) fn binary_path(&self) -> &Path {
        &self.binary_path
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub(crate) fn state(&self) -> ZccacheLifecycleState {
        self.state
    }

    pub(crate) fn start_with_recovery(&mut self) -> Result<(), SoldrError> {
        start_zccache_with_recovery(&self.binary_path, &self.cache_dir)?;
        self.state = ZccacheLifecycleState::DaemonStarted;
        Ok(())
    }

    pub(crate) fn start_session(
        &mut self,
        options: ZccacheSessionStartOptions,
    ) -> Result<ZccacheBuildSession, SoldrError> {
        self.start_with_recovery()?;

        let log_arg = options.session_log_path.display().to_string();
        let journal_arg = options.journal_path.display().to_string();
        let mut args: Vec<&str> = vec![
            "session-start",
            "--stats",
            "--log",
            &log_arg,
            "--journal",
            &journal_arg,
        ];
        if let Some(id) = options.id.as_deref() {
            args.push("--id");
            args.push(id);
        }

        let session_json = self.run(&args)?;
        let session_id = crate::cache_lib::parse_zccache_session_id(&session_json.stdout)
            .ok_or_else(|| {
                SoldrError::Other(format!(
                    "failed to parse zccache session id from output: {}",
                    session_json.stdout.trim()
                ))
            })?;

        self.state = ZccacheLifecycleState::SessionActive;
        Ok(ZccacheBuildSession {
            binary_path: self.binary_path.clone(),
            cache_dir: self.cache_dir.clone(),
            session_id,
            session_log_path: options.session_log_path,
            journal_path: options.journal_path,
            session_stats_path: options.session_stats_path,
        })
    }

    pub(crate) fn end_session_json(
        &mut self,
        session_id: &str,
    ) -> Result<ZccacheSessionEndOutcome, SoldrError> {
        let output = self.run_raw(&["session-end", session_id, "--json"])?;
        if output.status.success() {
            self.state = ZccacheLifecycleState::SessionEnded;
            return Ok(ZccacheSessionEndOutcome {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                already_ended: false,
            });
        }
        if zccache_session_already_ended(&output) {
            self.state = ZccacheLifecycleState::SessionEnded;
            return Ok(ZccacheSessionEndOutcome {
                stdout: String::new(),
                already_ended: true,
            });
        }
        Err(SoldrError::Other(zccache_command_failure_message(
            &["session-end", session_id, "--json"],
            &output,
        )))
    }

    pub(crate) fn stop(
        &mut self,
        no_depgraph_save: bool,
    ) -> Result<ZccacheStopOutcome, SoldrError> {
        let mut args: Vec<&str> = vec!["stop"];
        if no_depgraph_save {
            args.push("--no-depgraph-save");
        }
        let output = self.run_raw(&args)?;
        if output.status.success() {
            self.state = ZccacheLifecycleState::Stopped;
            return Ok(ZccacheStopOutcome::stopped());
        }
        if no_depgraph_save && zccache_flag_unsupported(&output, "--no-depgraph-save") {
            let retry = self.run_raw(&["stop"])?;
            if retry.status.success() {
                self.state = ZccacheLifecycleState::Stopped;
                return Ok(ZccacheStopOutcome {
                    unsupported_no_depgraph_save: true,
                    ..ZccacheStopOutcome::stopped()
                });
            }
            if zccache_daemon_already_stopped(&retry) {
                self.state = ZccacheLifecycleState::Stopped;
                return Ok(ZccacheStopOutcome {
                    unsupported_no_depgraph_save: true,
                    ..ZccacheStopOutcome::already_stopped()
                });
            }
            return Ok(ZccacheStopOutcome {
                unsupported_no_depgraph_save: true,
                ..ZccacheStopOutcome::failed(command_stderr(&retry))
            });
        }
        if zccache_daemon_already_stopped(&output) {
            self.state = ZccacheLifecycleState::Stopped;
            return Ok(ZccacheStopOutcome::already_stopped());
        }
        Ok(ZccacheStopOutcome::failed(command_stderr(&output)))
    }

    pub(crate) fn stop_best_effort_with_process_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<(), SoldrError> {
        let mut child = command_with_cache_dir(&self.binary_path, &["stop"], &self.cache_dir);
        child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = child.spawn()?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    self.state = ZccacheLifecycleState::Stopped;
                    return Ok(());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(err) => return Err(SoldrError::from(err)),
            }
        }
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.state = ZccacheLifecycleState::Stopped;
        Ok(())
    }

    pub(crate) fn poll_daemon_exit(&self, timeout: Duration) -> ZccacheDaemonExitPollResult {
        poll_zccache_daemon_exit(&self.binary_path, &self.cache_dir, timeout)
    }

    pub(crate) fn flush(&mut self) -> Result<ZccacheFlushOutcome, SoldrError> {
        let result = self.run_raw(&["flush", "--json"])?;
        if result.status.success() {
            let stdout = String::from_utf8_lossy(&result.stdout);
            let trimmed = stdout.trim();
            let mut outcome = ZccacheFlushOutcome {
                flushed: true,
                ..ZccacheFlushOutcome::no_op()
            };
            if !trimmed.is_empty() {
                outcome.stats = serde_json::from_str(trimmed).ok();
                if outcome.stats.is_none() {
                    outcome.invalid_json = Some(
                        zccache_output_snippet(trimmed.as_bytes())
                            .unwrap_or_else(|| "<empty>".into()),
                    );
                }
            }
            return Ok(outcome);
        }

        if zccache_flag_unsupported(&result, "--json") {
            let retry = self.run_raw(&["flush"])?;
            if retry.status.success() {
                return Ok(ZccacheFlushOutcome {
                    flushed: true,
                    json_unsupported: true,
                    ..ZccacheFlushOutcome::no_op()
                });
            }
            if zccache_subcommand_unsupported(&retry, "flush") {
                return Ok(ZccacheFlushOutcome {
                    json_unsupported: true,
                    subcommand_unsupported: true,
                    ..ZccacheFlushOutcome::no_op()
                });
            }
            if zccache_daemon_already_stopped(&retry) {
                return Ok(ZccacheFlushOutcome {
                    json_unsupported: true,
                    already_stopped: true,
                    ..ZccacheFlushOutcome::no_op()
                });
            }
            return Ok(ZccacheFlushOutcome {
                json_unsupported: true,
                failure: Some(command_stderr(&retry)),
                ..ZccacheFlushOutcome::no_op()
            });
        }

        if zccache_subcommand_unsupported(&result, "flush") {
            return Ok(ZccacheFlushOutcome {
                subcommand_unsupported: true,
                ..ZccacheFlushOutcome::no_op()
            });
        }
        if zccache_daemon_already_stopped(&result) {
            return Ok(ZccacheFlushOutcome {
                already_stopped: true,
                ..ZccacheFlushOutcome::no_op()
            });
        }
        Ok(ZccacheFlushOutcome {
            failure: Some(command_stderr(&result)),
            ..ZccacheFlushOutcome::no_op()
        })
    }

    pub(crate) fn run(&self, args: &[&str]) -> Result<CommandOutput, SoldrError> {
        run_zccache_command_in_cache_dir(&self.binary_path, args, &self.cache_dir)
    }

    pub(crate) fn run_raw(&self, args: &[&str]) -> Result<Output, SoldrError> {
        run_zccache_command_raw_in_cache_dir(&self.binary_path, args, &self.cache_dir)
    }
}

pub(crate) struct CommandOutput {
    pub(crate) stdout: String,
}

pub(crate) fn run_zccache_command_in_cache_dir(
    binary: &Path,
    args: &[&str],
    cache_dir: &Path,
) -> Result<CommandOutput, SoldrError> {
    run_zccache_command_with_env(
        binary,
        args,
        &[(
            crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

pub(crate) fn run_zccache_command_strings_in_cache_dir(
    binary: &Path,
    args: &[String],
    cache_dir: &Path,
) -> Result<CommandOutput, SoldrError> {
    let output = run_zccache_command_raw_strings_with_env(
        binary,
        args,
        &[(
            crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )?;
    if !output.status.success() {
        return Err(SoldrError::Other(zccache_command_failure_message_strings(
            args, &output,
        )));
    }

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

pub(crate) fn run_zccache_command_raw_in_cache_dir(
    binary: &Path,
    args: &[&str],
    cache_dir: &Path,
) -> Result<Output, SoldrError> {
    run_zccache_command_raw_with_env(
        binary,
        args,
        &[(
            crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

fn run_zccache_command_with_env(
    binary: &Path,
    args: &[&str],
    envs: &[(&str, &OsStr)],
) -> Result<CommandOutput, SoldrError> {
    let output = run_zccache_command_raw_with_env(binary, args, envs)?;
    if !output.status.success() {
        return Err(SoldrError::Other(zccache_command_failure_message(
            args, &output,
        )));
    }

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn run_zccache_command_raw_with_env(
    binary: &Path,
    args: &[&str],
    envs: &[(&str, &OsStr)],
) -> Result<Output, SoldrError> {
    let mut command = command_with_env(binary, args, envs);
    Ok(command.output()?)
}

fn run_zccache_command_raw_strings_with_env(
    binary: &Path,
    args: &[String],
    envs: &[(&str, &OsStr)],
) -> Result<Output, SoldrError> {
    let mut command = Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    suppress_windows_console_window(&mut command);
    Ok(command.output()?)
}

fn command_with_cache_dir(binary: &Path, args: &[&str], cache_dir: &Path) -> Command {
    command_with_env(
        binary,
        args,
        &[(
            crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

fn command_with_env(binary: &Path, args: &[&str], envs: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    suppress_windows_console_window(&mut command);
    command
}

/// Soldr-side escape hatch for the RUST_LOG value that gets injected into
/// `zccache start`.
pub(crate) const SOLDR_DAEMON_RUST_LOG_ENV_VAR: &str = "SOLDR_DAEMON_RUST_LOG";

pub(crate) fn effective_daemon_rust_log(soldr_override: Option<&str>) -> String {
    match soldr_override {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => "info".to_string(),
    }
}

fn run_zccache_start_command(binary: &Path, cache_dir: &Path) -> Result<Output, SoldrError> {
    let rust_log =
        effective_daemon_rust_log(std::env::var(SOLDR_DAEMON_RUST_LOG_ENV_VAR).ok().as_deref());
    run_zccache_command_raw_with_env(
        binary,
        &["start"],
        &[
            (
                crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
                cache_dir.as_os_str(),
            ),
            ("RUST_LOG", OsStr::new(rust_log.as_str())),
        ],
    )
}

pub(crate) fn start_zccache_with_recovery(
    binary: &Path,
    cache_dir: &Path,
) -> Result<(), SoldrError> {
    let start = run_zccache_start_command(binary, cache_dir)?;
    if start.status.success() {
        return Ok(());
    }

    let initial_stderr = command_stderr(&start);
    if !is_stale_zccache_daemon_start_failure(&initial_stderr) {
        return Err(SoldrError::Other(zccache_command_failure_message(
            &["start"],
            &start,
        )));
    }

    eprintln!(
        "soldr: zccache start reported an unresponsive daemon; stopping stale state and retrying"
    );
    let stop_diagnostic = match run_zccache_command_raw_in_cache_dir(binary, &["stop"], cache_dir) {
        Ok(stop) if stop.status.success() => None,
        Ok(stop) => Some(zccache_command_failure_message(&["stop"], &stop)),
        Err(err) => Some(format!("failed to invoke zccache stop: {err}")),
    };

    match run_zccache_start_command(binary, cache_dir) {
        Ok(retry) if retry.status.success() => Ok(()),
        Ok(retry) => {
            let mut message = format!(
                "zccache start failed after stale daemon recovery retry: {}",
                command_stderr(&retry)
            );
            message.push_str(&format!(
                "\ninitial zccache start failure: {}",
                initial_stderr
            ));
            if let Some(stop_diagnostic) = stop_diagnostic {
                message.push_str(&format!("\nzccache stop diagnostic: {stop_diagnostic}"));
            }
            Err(SoldrError::Other(message))
        }
        Err(err) => {
            let mut message =
                format!("failed to invoke zccache start during stale daemon recovery retry: {err}");
            message.push_str(&format!(
                "\ninitial zccache start failure: {}",
                initial_stderr
            ));
            if let Some(stop_diagnostic) = stop_diagnostic {
                message.push_str(&format!("\nzccache stop diagnostic: {stop_diagnostic}"));
            }
            Err(SoldrError::Other(message))
        }
    }
}

fn is_stale_zccache_daemon_start_failure(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("not accepting connections")
        || (stderr.contains("daemon process") && stderr.contains("exists"))
}

pub(crate) fn poll_zccache_daemon_exit(
    binary: &Path,
    cache_dir: &Path,
    timeout: Duration,
) -> ZccacheDaemonExitPollResult {
    let deadline = Instant::now() + timeout;
    let poll_interval = Duration::from_millis(100);
    loop {
        match run_zccache_command_raw_in_cache_dir(binary, &["status"], cache_dir) {
            Ok(output) => {
                if zccache_daemon_already_stopped(&output) {
                    return ZccacheDaemonExitPollResult::Exited;
                }
            }
            Err(err) => return ZccacheDaemonExitPollResult::PollFailed(err.to_string()),
        }
        if Instant::now() >= deadline {
            return ZccacheDaemonExitPollResult::TimedOut;
        }
        std::thread::sleep(poll_interval);
    }
}

pub(crate) fn zccache_session_already_ended(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("session not found")
        || stderr.contains("no such session")
        || stderr.contains("already ended")
        || stderr.contains("unknown session")
}

pub(crate) fn zccache_json_flag_unsupported(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("unexpected argument")
        || stderr.contains("unrecognized option")
        || stderr.contains("found argument")
}

pub(crate) fn zccache_flag_unsupported(output: &Output, flag: &str) -> bool {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    combined.contains("unexpected argument") && combined.contains(flag.trim_start_matches('-'))
        || combined.contains(&format!("unknown flag: {flag}"))
        || combined.contains(&format!("unrecognized option {flag}"))
}

pub(crate) fn zccache_subcommand_unsupported(output: &Output, subcommand: &str) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let needles = [
        "unrecognized subcommand",
        "unrecognized command",
        "error: subcommand",
        "invalid value for",
    ];
    let combined = format!("{stderr}\n{stdout}");
    needles.iter().any(|n| combined.contains(n)) && combined.contains(subcommand)
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

pub(crate) fn zccache_daemon_already_stopped(output: &Output) -> bool {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    )
    .to_ascii_lowercase();
    combined.contains("daemon not running")
        || combined.contains("no daemon to stop")
        || combined.contains("no daemon")
        || combined.contains("not running")
        || combined.contains("connection refused")
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

pub(crate) fn zccache_command_failure_message(args: &[&str], output: &Output) -> String {
    format!(
        "zccache {} failed: {}",
        args.join(" "),
        command_stderr(output)
    )
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
    use std::process::Output;

    fn synthetic_output(stderr: &str, exit: i32) -> Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw((exit & 0xFF) << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(exit as u32)
        };
        Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    crate::timed_test!(lifecycle_state_transitions_are_explicit, {
        let lifecycle = ZccacheLifecycle::new("zccache", "cache");
        assert_eq!(lifecycle.state(), ZccacheLifecycleState::Ready);
        assert_eq!(lifecycle.binary_path(), Path::new("zccache"));
        assert_eq!(lifecycle.cache_dir(), Path::new("cache"));
    });

    crate::timed_test!(session_already_ended_detects_common_states, {
        for needle in [
            "session not found",
            "no such session",
            "already ended",
            "unknown session abc",
        ] {
            assert!(
                zccache_session_already_ended(&synthetic_output(needle, 1)),
                "expected {needle:?} to indicate ended session"
            );
        }
        assert!(!zccache_session_already_ended(&synthetic_output(
            "permission denied",
            1
        )));
    });

    crate::timed_test!(daemon_already_stopped_detects_common_states, {
        for needle in [
            "daemon not running",
            "No daemon to stop",
            "no daemon",
            "connection refused",
        ] {
            assert!(
                zccache_daemon_already_stopped(&synthetic_output(needle, 1)),
                "expected {needle:?} to indicate daemon already stopped"
            );
        }
    });

    crate::timed_test!(flag_unsupported_detects_clap_phrasing, {
        let out = synthetic_output("error: unexpected argument '--no-depgraph-save'", 2);
        assert!(zccache_flag_unsupported(&out, "--no-depgraph-save"));
        let unrelated = synthetic_output("error: permission denied", 2);
        assert!(!zccache_flag_unsupported(&unrelated, "--no-depgraph-save"));
    });

    crate::timed_test!(output_snippet_omits_empty_output, {
        assert_eq!(zccache_output_snippet(b""), None);
        assert_eq!(zccache_output_snippet(b" \n\t "), None);
    });

    crate::timed_test!(output_snippet_compacts_whitespace, {
        assert_eq!(
            zccache_output_snippet(b"  first line\n\nsecond\tline  ").as_deref(),
            Some("first line second line")
        );
    });

    crate::timed_test!(output_snippet_truncates_long_output, {
        let output = "x".repeat(ZCCACHE_ANALYZE_NOTE_LIMIT + 10);
        let snippet = zccache_output_snippet(output.as_bytes()).unwrap();
        assert_eq!(snippet.len(), ZCCACHE_ANALYZE_NOTE_LIMIT + 3);
        assert!(snippet.ends_with("..."));
    });

    crate::timed_test!(unknown_session_detector_matches_exact_zccache_line, {
        let stderr = b"zccache error: unknown session: abc-123\n";
        assert!(stderr_indicates_unknown_session(stderr));
    });

    crate::timed_test!(unknown_session_detector_matches_substring_not_line_shape, {
        let stderr = b"prelude blah blah unknown session: 0000 trailing\n";
        assert!(stderr_indicates_unknown_session(stderr));
    });

    crate::timed_test!(
        unknown_session_detector_does_not_match_other_session_text,
        {
            let stderr = b"zccache info: session started\nzccache info: session ok\n";
            assert!(!stderr_indicates_unknown_session(stderr));
        }
    );
}
