//! Diagnostics for small external tool invocations (soldr#3389).
//!
//! Owner directive: "We should always handle stdout, not just swallow it,
//! and stderr as well." Every small probe that goes through this module
//! (`rustc -V`, `rustc -vV`, `llvm-objcopy`, a Dylint PATH component
//! `--version`, …) follows one contract, on success and on failure alike:
//!
//! 1. **Always forward.** Non-empty captured stderr is forwarded to Soldr's
//!    own stderr, every line prefixed with the tool context
//!    (`rustc -vV: <line>`).
//! 2. **Always log.** Every run appends one JSON record to
//!    `<soldr root>/logs/small-tools.jsonl` (listed by `soldr logs paths`):
//!    argv, exit status, duration, and the captured stdout and stderr, each
//!    bounded to [`LOG_STREAM_BYTES`].
//! 3. **Failures carry the cause.** A spawn error, timeout, or non-zero exit
//!    becomes an error (or warning) that includes a trimmed stderr excerpt of
//!    at most [`STDERR_EXCERPT_BYTES`]; a bare exit status is not a cause.
//!
//! Route new small-tool call sites through this module rather than
//! hand-rolling forwarding or logging beside them.

use super::SoldrPaths;
use super::{command_output_with_timeout, command_output_with_timeout_duration, SoldrError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Maximum number of stderr bytes carried into an error or warning.
pub const STDERR_EXCERPT_BYTES: usize = 500;

/// Maximum number of bytes of each stream recorded in the persistent log.
pub const LOG_STREAM_BYTES: usize = 64 * 1024;

/// Trimmed, UTF-8-lossy excerpt of the first [`STDERR_EXCERPT_BYTES`] of a
/// byte stream. Returns `"<empty>"` when nothing was written.
pub fn bytes_excerpt(bytes: &[u8]) -> String {
    let end = bytes.len().min(STDERR_EXCERPT_BYTES);
    let text = String::from_utf8_lossy(&bytes[..end]);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "<empty>".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Trimmed excerpt of a finished child's stderr.
pub fn stderr_excerpt(output: &Output) -> String {
    bytes_excerpt(&output.stderr)
}

/// One-line description of a failed small-tool run: exit status plus the
/// stderr excerpt (and a stdout excerpt when stderr is empty, since some
/// tools report errors on stdout).
pub fn describe_tool_failure(context: &str, output: &Output) -> String {
    let stderr = stderr_excerpt(output);
    if stderr == "<empty>" && !output.stdout.is_empty() {
        format!(
            "{context} exited with {} (stderr: <empty>; stdout: {})",
            output.status,
            bytes_excerpt(&output.stdout)
        )
    } else {
        format!("{context} exited with {} (stderr: {stderr})", output.status)
    }
}

/// Where the forwarded stderr and the persistent record go. Production uses
/// [`ToolSinks::process`]; tests inject a buffer and a temp log path.
pub struct ToolSinks<'a> {
    pub stderr: &'a mut dyn Write,
    pub log_path: Option<PathBuf>,
}

impl ToolSinks<'_> {
    /// Soldr's own stderr plus the default persistent log under the soldr
    /// root (`SOLDR_CACHE_DIR` aware).
    pub fn process(stderr: &mut dyn Write) -> ToolSinks<'_> {
        ToolSinks {
            stderr,
            log_path: default_log_path(),
        }
    }
}

/// `<soldr root>/logs/small-tools.jsonl`, or `None` if no root resolves.
pub fn default_log_path() -> Option<PathBuf> {
    SoldrPaths::new().ok().map(|paths| paths.small_tool_log())
}

/// Run a small tool with both streams captured, forwarding and logging per
/// the module contract. `Err` only for a spawn failure or timeout; a
/// non-zero exit is returned as `Ok(output)` for callers that interpret the
/// status themselves. `timeout = None` uses Soldr's configurable inactivity
/// budget; `Some(d)` is a hard wall-clock bound for protocol probes.
pub fn capture_small_tool(
    command: &mut Command,
    context: &str,
    timeout: Option<Duration>,
) -> Result<Output, SoldrError> {
    let mut stderr = std::io::stderr();
    capture_small_tool_with_sinks(command, context, timeout, ToolSinks::process(&mut stderr))
}

/// [`capture_small_tool`] with explicit sinks.
pub fn capture_small_tool_with_sinks(
    command: &mut Command,
    context: &str,
    timeout: Option<Duration>,
    sinks: ToolSinks<'_>,
) -> Result<Output, SoldrError> {
    let argv = command_argv(command);
    let started = Instant::now();
    let result = match timeout {
        Some(limit) => command_output_with_timeout_duration(command, context, limit),
        None => command_output_with_timeout(command, context),
    };
    let duration = started.elapsed();
    if let Ok(output) = &result {
        forward_stderr(sinks.stderr, context, &output.stderr);
    }
    if let Some(log_path) = &sinks.log_path {
        append_log_record(log_path, context, &argv, duration, &result);
    }
    result
}

/// Run a small tool; a spawn error, timeout, or non-zero exit becomes a
/// `SoldrError` naming the tool and carrying its stderr excerpt.
pub fn run_small_tool(command: &mut Command, context: &str) -> Result<Output, SoldrError> {
    let mut stderr = std::io::stderr();
    run_small_tool_with_sinks(command, context, None, ToolSinks::process(&mut stderr))
}

/// [`run_small_tool`] with an explicit timeout and sinks.
pub fn run_small_tool_with_sinks(
    command: &mut Command,
    context: &str,
    timeout: Option<Duration>,
    sinks: ToolSinks<'_>,
) -> Result<Output, SoldrError> {
    let output = capture_small_tool_with_sinks(command, context, timeout, sinks)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(SoldrError::Other(describe_tool_failure(context, &output)))
    }
}

/// First line of `<rustc> -V` (e.g. `rustc 1.98.1 (…)`). The one
/// implementation shared by `soldr cook` and the cargo front door's cook
/// hydrate pre-flight (soldr#3381).
pub fn rustc_version_line(rustc: &Path) -> Result<String, SoldrError> {
    let mut stderr = std::io::stderr();
    rustc_version_line_with_sinks(rustc, ToolSinks::process(&mut stderr))
}

/// [`rustc_version_line`] with explicit sinks.
pub fn rustc_version_line_with_sinks(
    rustc: &Path,
    sinks: ToolSinks<'_>,
) -> Result<String, SoldrError> {
    let context = format!("{} -V", rustc.display());
    let mut command = Command::new(rustc);
    command.arg("-V");
    let output = run_small_tool_with_sinks(&mut command, &context, None, sinks)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "{context} printed no version line (stderr: {})",
                stderr_excerpt(&output)
            ))
        })
}

fn command_argv(command: &Command) -> Vec<String> {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn forward_stderr(sink: &mut dyn Write, context: &str, stderr: &[u8]) {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        // A closed stderr must not turn a successful probe into a failure.
        let _ = writeln!(sink, "{context}: {line}");
    }
}

fn bounded_lossy(bytes: &[u8]) -> String {
    let end = bytes.len().min(LOG_STREAM_BYTES);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn append_log_record(
    log_path: &Path,
    context: &str,
    argv: &[String],
    duration: Duration,
    result: &Result<Output, SoldrError>,
) {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let mut record = serde_json::json!({
        "unix_ms": unix_ms,
        "context": context,
        "argv": argv,
        "duration_ms": duration.as_millis() as u64,
    });
    match result {
        Ok(output) => {
            record["exit_code"] = serde_json::json!(output.status.code());
            record["status"] = serde_json::json!(output.status.to_string());
            record["stdout"] = serde_json::json!(bounded_lossy(&output.stdout));
            record["stderr"] = serde_json::json!(bounded_lossy(&output.stderr));
        }
        Err(error) => {
            record["error"] = serde_json::json!(error.to_string());
        }
    }
    // The log is a diagnostic sink: failing to write it must not fail the
    // probe, but it must not vanish silently either.
    if let Err(error) = write_log_line(log_path, &record) {
        eprintln!(
            "soldr: warning: could not append small-tool log {}: {error}",
            log_path.display()
        );
    }
}

fn write_log_line(log_path: &Path, record: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    line.push(b'\n');
    file.write_all(&line)
}

/// Test support: write an executable fake tool that prints `stdout` and
/// `stderr` (single lines, no shell metacharacters) and exits `code`.
/// Returns the path to invoke. Shared by soldr-core and soldr-cli tests.
#[doc(hidden)]
pub fn write_fake_tool(dir: &Path, name: &str, stdout: &str, stderr: &str, code: i32) -> PathBuf {
    let windows =
        crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows;
    if windows {
        let path = dir.join(format!("{name}.bat"));
        let mut body = String::from("@echo off\r\n");
        if !stdout.is_empty() {
            body.push_str(&format!("echo {stdout}\r\n"));
        }
        if !stderr.is_empty() {
            body.push_str(&format!("echo {stderr} 1>&2\r\n"));
        }
        body.push_str(&format!("exit /b {code}\r\n"));
        std::fs::write(&path, body).expect("write fake tool");
        path
    } else {
        let path = dir.join(name);
        let mut body = String::from("#!/bin/sh\n");
        if !stdout.is_empty() {
            body.push_str(&format!("echo '{stdout}'\n"));
        }
        if !stderr.is_empty() {
            body.push_str(&format!("echo '{stderr}' 1>&2\n"));
        }
        body.push_str(&format!("exit {code}\n"));
        std::fs::write(&path, body).expect("write fake tool");
        // `PermissionsExt::set_mode` is unix-only; go through chmod instead
        // of a cfg'd import (platform boundary, soldr#2493).
        let mut chmod = Command::new("chmod");
        chmod.arg("755").arg(&path);
        let output = command_output_with_timeout(&mut chmod, "chmod 755").expect("chmod");
        assert!(output.status.success(), "chmod 755 {}", path.display());
        path
    }
}

#[cfg(test)]
#[path = "tool_output_tests.rs"]
mod tests;
