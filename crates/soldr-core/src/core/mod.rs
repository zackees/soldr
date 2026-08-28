//! Shared foundational types for soldr.
//!
//! The `core` module owns target-triple resolution, `~/.soldr/`
//! layout (`SoldrPaths` / `SoldrConfig`), `SoldrError`, and the
//! runtime toolchain-discovery helpers. It is split across
//! sub-modules but the external API (`crate::core::*` /
//! `soldr_cli::core::*`) is preserved via re-exports below — every
//! identifier consumers reach for must show up in this `mod.rs`.

use std::ffi::OsStr;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use wait_timeout::ChildExt;

/// soldr#941/#2336 — single-source-of-truth for the canonical 9-target list.
/// Mirror of `[workspace.metadata.soldr].targets` in the root
/// `Cargo.toml`; a parity test enforces byte-equality.
pub mod canonical_targets;
pub mod cpu_topology;
pub mod env_flag;
pub mod git;
pub mod installer_watchdog;
/// soldr#1761 — soldr-owned compile concurrency limit, resolved once
/// and shared by the admission queue and the compile semaphore.
pub mod jobs;
mod paths;
pub mod quiet;
mod target_triple;
mod temp;
mod toolchain_manifest;
mod toolchain_resolve;
pub mod wire;

pub use canonical_targets::{canonical_targets, is_canonical, CANONICAL_TARGETS};
pub use env_flag::{flag, flag_value, foreign_flag, foreign_flag_value, is_off_value};
pub use installer_watchdog::{
    installer_safety_timeout, installer_stall_timeout, run_installer_command,
    InstallerWatchdogConfig, DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS,
    DEFAULT_INSTALLER_STALL_TIMEOUT_SECS, INSTALLER_SAFETY_TIMEOUT_ENV_VAR,
    INSTALLER_STALL_TIMEOUT_ENV_VAR,
};
pub use paths::{
    resolve_cargo_home, resolve_rustup_home, AutoGcConfig, CookConfig, GcConfig, InstallConfig,
    PinsConfig, SoldrConfig, SoldrConfigLoadError, SoldrPaths, MANAGED_SHIM_VERSION,
    SOLDR_CACHE_DIR_ENV_VAR,
};
pub use target_triple::{Arch, Env, Os, TargetTriple};
pub use temp::{
    ensure_temp_root, ensure_temp_root_for, replace_file_with_dir, temp_root, temp_root_for,
    SOLDR_TMPDIR_ENV_VAR,
};
pub use toolchain_manifest::{
    read_rust_toolchain_manifest, PluginSpec, RustToolchainManifest, SoldrCookManifest,
    SoldrManifestSection,
};
pub use toolchain_resolve::{
    apply_implicit_toolchain_homes, find_rust_toolchain_manifest, probe_toolchain_binary,
    suppress_windows_console_window,
};

pub const CARGO_HOME_ENV_VAR: &str = "CARGO_HOME";
pub const RUSTUP_HOME_ENV_VAR: &str = "RUSTUP_HOME";
pub(crate) const RUSTUP_TOOLCHAIN_ENV_VAR: &str = "RUSTUP_TOOLCHAIN";
pub const COMMAND_OUTPUT_TIMEOUT_ENV_VAR: &str = "SOLDR_COMMAND_OUTPUT_TIMEOUT_SECS";
pub const DEFAULT_COMMAND_OUTPUT_TIMEOUT_SECS: u64 = 60;
const KILLED_COMMAND_OUTPUT_REAP_TIMEOUT_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SoldrError {
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("no home directory found")]
    NoHomeDir,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("archive error: {0}")]
    Archive(String),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Home-dir resolution + small env helpers
// ---------------------------------------------------------------------------

/// Public accessor for the soldr home directory used by the `gc`
/// allowlist default (`~/dev`). Returns `Err` when no home dir can be
/// resolved.
pub fn user_home_dir() -> Result<PathBuf, SoldrError> {
    home_dir()
}

/// Expand `~` and `~/...` strings to absolute paths under the user's
/// home directory. Other inputs pass through unchanged.
pub fn expand_user_home(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~") {
        if let Ok(home) = home_dir() {
            let trimmed = rest.trim_start_matches(['/', '\\']);
            if trimmed.is_empty() {
                return home;
            }
            return home.join(trimmed);
        }
    }
    PathBuf::from(input)
}

pub(crate) fn soldr_root_from_env_var(
    value: Option<&OsStr>,
) -> Option<Result<PathBuf, SoldrError>> {
    non_empty_env_path(value).map(Ok)
}

pub(crate) fn non_empty_env_path(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub fn home_dir() -> Result<PathBuf, SoldrError> {
    crate::platform::host::dirs::home().ok_or(SoldrError::NoHomeDir)
}

pub fn command_output_with_timeout(
    command: &mut Command,
    context: &str,
) -> Result<Output, SoldrError> {
    command_output_with_timeout_inner(command, context, command_output_timeout(), true)
}

/// Run a command through soldr's sanctioned output-capture containment with a
/// caller-selected timeout. This is for small host probes whose timeout is a
/// protocol property rather than user-configurable build policy.
pub fn command_output_with_timeout_duration(
    command: &mut Command,
    context: &str,
    timeout: Duration,
) -> Result<Output, SoldrError> {
    command_output_with_timeout_inner(command, context, timeout, false)
}

fn command_output_with_timeout_inner(
    command: &mut Command,
    context: &str,
    timeout: Duration,
    suggest_timeout_override: bool,
) -> Result<Output, SoldrError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("failed to invoke {context}: {err}")))?;
    let stdout_reader = child.stdout.take().map(read_pipe_async);
    let stderr_reader = child.stderr.take().map(read_pipe_async);
    let status = match child
        .wait_timeout(timeout)
        .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
    {
        Some(status) => status,
        None => {
            let kill_result = child.kill();
            let reap_result =
                child.wait_timeout(Duration::from_secs(KILLED_COMMAND_OUTPUT_REAP_TIMEOUT_SECS));
            let timeout_secs = timeout.as_secs();
            let mut message = format!("{context} timed out after {timeout_secs} seconds");
            if suggest_timeout_override {
                message.push_str(&format!(
                    " (set {COMMAND_OUTPUT_TIMEOUT_ENV_VAR} to override)"
                ));
            }
            match kill_result {
                Ok(()) => message.push_str("; killed child process"),
                Err(err) => message.push_str(&format!("; kill failed: {err}")),
            }
            match reap_result {
                Ok(Some(_)) => {}
                Ok(None) => message.push_str(&format!(
                    "; process did not exit within {KILLED_COMMAND_OUTPUT_REAP_TIMEOUT_SECS} seconds after kill"
                )),
                Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
            }
            return Err(SoldrError::Other(message));
        }
    };

    let stdout = join_pipe_reader(stdout_reader, context, "stdout")?;
    let stderr = join_pipe_reader(stderr_reader, context, "stderr")?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe_async<R>(mut pipe: R) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_pipe_reader(
    reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    context: &str,
    pipe_name: &str,
) -> Result<Vec<u8>, SoldrError> {
    match reader {
        Some(handle) => handle
            .join()
            .map_err(|_| SoldrError::Other(format!("{pipe_name} reader panicked for {context}")))?
            .map_err(|err| {
                SoldrError::Other(format!("failed to read {pipe_name} from {context}: {err}"))
            }),
        None => Ok(Vec::new()),
    }
}

pub fn command_output_timeout() -> Duration {
    std::env::var(COMMAND_OUTPUT_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| command_output_timeout_from_str(&value))
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_COMMAND_OUTPUT_TIMEOUT_SECS))
}

fn command_output_timeout_from_str(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn command_output_timeout_parses_positive_values_only() {
        assert_eq!(
            command_output_timeout_from_str("7"),
            Some(Duration::from_secs(7))
        );
        assert_eq!(command_output_timeout_from_str("0"), None);
        assert_eq!(command_output_timeout_from_str("not-a-number"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_timeout_drains_pipe_filling_child_then_kills_and_reaps_it() {
        let temp = tempfile::tempdir().expect("temporary command directory");
        let pid_file = temp.path().join("child.pid");
        let script = format!(
            "echo $$ > '{}'; dd if=/dev/zero bs=1024 count=256 2>/dev/null; while :; do :; done",
            pid_file.display()
        );
        let started = std::time::Instant::now();
        let error = command_output_with_timeout_duration(
            Command::new("sh").args(["-c", &script]),
            "pipe-filling test child",
            Duration::from_secs(1),
        )
        .expect_err("pipe-filling child must time out");

        assert!(
            started.elapsed() < Duration::from_secs(3),
            "pipe filling must not prevent the bounded timeout"
        );
        assert!(error.to_string().contains("timed out after 1 seconds"));
        let pid = std::fs::read_to_string(&pid_file)
            .expect("child must record its pid")
            .trim()
            .to_owned();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the timed-out child must be reaped before returning"
        );
    }
}
