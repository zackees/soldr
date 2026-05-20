//! Zccache build-session orchestration and zccache subprocess helpers.
//! Extracted from `main.rs` as part of issue #339.

use crate::{
    current_soldr_binary, fetch_managed_zccache, non_empty_env_path, ZccacheSourceArg,
    RUSTC_WRAPPER_OVERRIDE_ENV_VAR,
};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};

pub(crate) struct ZccacheBuildSession {
    pub(crate) binary_path: std::path::PathBuf,
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) session_id: String,
    pub(crate) session_log_path: std::path::PathBuf,
    pub(crate) journal_path: std::path::PathBuf,
    pub(crate) session_stats_path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RustcWrapperMode {
    ManagedZccache,
    Custom(std::ffi::OsString),
    Disabled,
}

pub(crate) fn rustc_wrapper_mode_from_env_var(value: Option<&std::ffi::OsStr>) -> RustcWrapperMode {
    match value.and_then(std::ffi::OsStr::to_str) {
        None => value
            .map(|value| RustcWrapperMode::Custom(value.to_os_string()))
            .unwrap_or(RustcWrapperMode::ManagedZccache),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
                RustcWrapperMode::Disabled
            } else {
                RustcWrapperMode::Custom(trimmed.into())
            }
        }
    }
}

pub(crate) fn rustc_wrapper_mode() -> RustcWrapperMode {
    rustc_wrapper_mode_from_env_var(std::env::var_os(RUSTC_WRAPPER_OVERRIDE_ENV_VAR).as_deref())
}

/// Decide what value (if any) soldr should set for `ZCCACHE_PATH_REMAP` on
/// the spawned child cargo. Returns `Some("auto")` if soldr should inject
/// the default parent-cache remap, or `None` if no injection is required
/// (either the user already set it, or the soldr-side escape hatch
/// `SOLDR_PATH_REMAP=off` is active).
///
/// Issue #352 (Tier L1.x).
pub(crate) fn resolve_path_remap_env(
    user_zccache: Option<&str>,
    soldr_override: Option<&str>,
) -> Option<&'static str> {
    // Rule 1: if the user already exported ZCCACHE_PATH_REMAP, never
    // overwrite. zccache itself decides what to do with their value
    // (including the empty string).
    if user_zccache.is_some() {
        return None;
    }

    // Rule 2: SOLDR_PATH_REMAP=off (case-insensitive) suppresses the
    // injection. Anything else, or unset, falls through to auto.
    if let Some(value) = soldr_override {
        if value.trim().eq_ignore_ascii_case("off") {
            return None;
        }
    }

    Some("auto")
}

pub(crate) async fn prepare_rustc_wrapper(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
    zccache_source: ZccacheSourceArg,
) -> Result<Option<ZccacheBuildSession>, SoldrError> {
    match rustc_wrapper_mode() {
        RustcWrapperMode::ManagedZccache => prepare_zccache_build(cargo, paths, zccache_source)
            .await
            .map(Some),
        RustcWrapperMode::Custom(wrapper) => {
            if is_sccache_wrapper(&wrapper) && std::env::var_os("SCCACHE_DIR").is_none() {
                let sccache_dir = soldr_cache::sccache_dir(paths);
                std::fs::create_dir_all(&sccache_dir)?;
                cargo.env("SCCACHE_DIR", sccache_dir);
            }
            cargo.env("RUSTC_WRAPPER", wrapper);
            cargo.env_remove(soldr_cache::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
            cargo.env_remove(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR);
            Ok(None)
        }
        RustcWrapperMode::Disabled => {
            cargo.env_remove("RUSTC_WRAPPER");
            cargo.env_remove(soldr_cache::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
            cargo.env_remove(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR);
            Ok(None)
        }
    }
}

pub(crate) fn is_sccache_wrapper(wrapper: &std::ffi::OsStr) -> bool {
    std::path::Path::new(wrapper)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"))
}

async fn prepare_zccache_build(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
    zccache_source: ZccacheSourceArg,
) -> Result<ZccacheBuildSession, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;
    let fetch = match zccache_source {
        ZccacheSourceArg::Managed => {
            let fetched = fetch_managed_zccache(paths).await?;
            if fetched.cached {
                eprintln!(
                    "soldr: using managed zccache {}",
                    soldr_fetch::MANAGED_ZCCACHE_VERSION
                );
            } else {
                eprintln!(
                    "soldr: fetched managed zccache {}",
                    soldr_fetch::MANAGED_ZCCACHE_VERSION
                );
            }
            fetched
        }
        ZccacheSourceArg::System => soldr_fetch::resolve_system_zccache(paths)?,
    };

    start_zccache_with_recovery(&fetch.binary_path, &zccache_dir)?;

    let session_log_path = soldr_cache::session_log_path(&zccache_dir);
    let session_log_path_arg = session_log_path.display().to_string();
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_path_arg = journal_path.display().to_string();
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);
    let session_json = run_zccache_command_in_cache_dir(
        &fetch.binary_path,
        &[
            "session-start",
            "--stats",
            "--log",
            &session_log_path_arg,
            "--journal",
            &journal_path_arg,
        ],
        &zccache_dir,
    )?;
    let session_id =
        soldr_cache::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to parse zccache session id from output: {}",
                session_json.stdout.trim()
            ))
        })?;

    cargo.env("RUSTC_WRAPPER", current_soldr_binary()?);
    cargo.env(soldr_cache::ZCCACHE_BINARY_ENV_VAR, &fetch.binary_path);
    cargo.env(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR, &zccache_dir);
    cargo.env(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR, &zccache_dir);
    cargo.env(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR, &session_id);

    // Parent-cache (Tier L1.x, issue #352): seed ZCCACHE_PATH_REMAP=auto so
    // multiple worktrees of the same repo share zccache hits. Honor any
    // user-supplied ZCCACHE_PATH_REMAP, and the SOLDR_PATH_REMAP=off
    // escape hatch.
    let user_zccache = std::env::var(soldr_cache::ZCCACHE_PATH_REMAP_ENV_VAR).ok();
    let soldr_override = std::env::var(soldr_cache::SOLDR_PATH_REMAP_ENV_VAR).ok();
    if let Some(value) = resolve_path_remap_env(user_zccache.as_deref(), soldr_override.as_deref())
    {
        cargo.env(soldr_cache::ZCCACHE_PATH_REMAP_ENV_VAR, value);
    }

    Ok(ZccacheBuildSession {
        binary_path: fetch.binary_path,
        cache_dir: zccache_dir,
        session_id,
        session_log_path,
        journal_path,
        session_stats_path,
    })
}

pub(crate) fn finish_zccache_build(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let output = run_zccache_command_raw_in_cache_dir(
        &session.binary_path,
        &["session-end", &session.session_id, "--json"],
        &session.cache_dir,
    )?;
    if session.session_log_path.exists() {
        eprintln!(
            "soldr: zccache session log: {}",
            session.session_log_path.display()
        );
    }
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stats_json = stdout.trim();
        if !stats_json.is_empty() {
            write_zccache_session_stats_json(session, stats_json)?;
            let stats = parse_zccache_session_stats_json(stats_json)?;
            print_zccache_session_stats(&stats, &session.session_stats_path);
        }
        return Ok(());
    }

    if zccache_json_flag_unsupported(&output) {
        eprintln!(
            "soldr: zccache JSON session summary unavailable; falling back to text session-end"
        );
        finish_zccache_build_text_fallback(session)?;
        return Ok(());
    }

    Err(SoldrError::Other(zccache_command_failure_message(
        &["session-end", &session.session_id, "--json"],
        &output,
    )))
}

fn finish_zccache_build_text_fallback(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let output = run_zccache_command_in_cache_dir(
        &session.binary_path,
        &["session-end", &session.session_id],
        &session.cache_dir,
    )?;
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        eprintln!("soldr: zccache session summary");
        eprintln!("{stdout}");
    }
    Ok(())
}

fn write_zccache_session_stats_json(
    session: &ZccacheBuildSession,
    stats_json: &str,
) -> Result<(), SoldrError> {
    if let Some(parent) = session.session_stats_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&session.session_stats_path, stats_json)?;
    Ok(())
}

fn parse_zccache_session_stats_json(stats_json: &str) -> Result<serde_json::Value, SoldrError> {
    serde_json::from_str(stats_json).map_err(|err| {
        SoldrError::Other(format!(
            "failed to parse zccache JSON session summary: {err}"
        ))
    })
}

fn print_zccache_session_stats(stats: &serde_json::Value, stats_path: &std::path::Path) {
    eprintln!("soldr: zccache session summary");
    eprintln!("  stats file: {}", stats_path.display());
    match stats.get("status").and_then(serde_json::Value::as_str) {
        Some("ok") => {
            let hits = json_u64(stats, "hits").unwrap_or(0);
            let misses = json_u64(stats, "misses").unwrap_or(0);
            let non_cacheable = json_u64(stats, "non_cacheable").unwrap_or(0);
            let errors = json_u64(stats, "errors").unwrap_or(0);
            let compilations = json_u64(stats, "compilations").unwrap_or(hits + misses);
            eprintln!(
                "  compilations: {compilations}; hits: {hits}; misses: {misses}; non-cacheable: {non_cacheable}; errors: {errors}"
            );
            if let Some(hit_rate) = json_f64(stats, "hit_rate") {
                eprintln!("  hit rate: {:.1}%", hit_rate * 100.0);
            } else {
                eprintln!("  hit rate: n/a");
            }
            let unique_sources = json_u64(stats, "unique_sources").unwrap_or(0);
            let bytes_read = json_u64(stats, "bytes_read").unwrap_or(0);
            let bytes_written = json_u64(stats, "bytes_written").unwrap_or(0);
            eprintln!(
                "  unique sources: {unique_sources}; bytes read: {bytes_read}; bytes written: {bytes_written}"
            );
            let time_saved_ms = json_u64(stats, "time_saved_ms").unwrap_or(0);
            let duration_ms = json_u64(stats, "duration_ms").unwrap_or(0);
            eprintln!("  time saved: {time_saved_ms} ms; duration: {duration_ms} ms");
        }
        Some("unavailable") => {
            let reason = stats
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            eprintln!("  status: unavailable ({reason})");
        }
        Some("error") => {
            let error = stats
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            eprintln!("  status: error ({error})");
        }
        Some(status) => eprintln!("  status: {status}"),
        None => eprintln!("  status: unknown"),
    }
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn json_f64(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn zccache_json_flag_unsupported(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("unexpected argument")
        || stderr.contains("unrecognized option")
        || stderr.contains("found argument")
}

pub(crate) struct CommandOutput {
    pub(crate) stdout: String,
}

pub(crate) fn managed_zccache_cache_dir(
    paths: &SoldrPaths,
) -> Result<std::path::PathBuf, SoldrError> {
    let zccache_dir = normalize_path_for_compare(&soldr_cache::zccache_dir(paths))?;
    let inherited_soldr_managed_dir =
        non_empty_env_path(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?;
    if let Some(explicit) = non_empty_env_path(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR) {
        let explicit = normalize_path_for_compare(&explicit)?;
        if explicit != zccache_dir && inherited_soldr_managed_dir.as_ref() != Some(&explicit) {
            return Err(SoldrError::Other(format!(
                "{} is managed by soldr for managed zccache builds. Unset it, set SOLDR_CACHE_DIR to choose soldr's cache root, or set SOLDR_RUSTC_WRAPPER to use a custom wrapper.",
                soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR
            )));
        }
    }
    Ok(zccache_dir)
}

pub(crate) fn normalize_path_for_compare(
    path: &std::path::Path,
) -> Result<std::path::PathBuf, SoldrError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(crate) fn run_zccache_command_in_cache_dir(
    binary: &std::path::Path,
    args: &[&str],
    cache_dir: &std::path::Path,
) -> Result<CommandOutput, SoldrError> {
    run_zccache_command_with_env(
        binary,
        args,
        &[(
            soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

pub(crate) fn run_zccache_command_strings_in_cache_dir(
    binary: &std::path::Path,
    args: &[String],
    cache_dir: &std::path::Path,
) -> Result<CommandOutput, SoldrError> {
    let output = run_zccache_command_raw_strings_with_env(
        binary,
        args,
        &[(
            soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR,
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

pub(crate) fn start_zccache_with_recovery(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<(), SoldrError> {
    let start = run_zccache_command_raw_in_cache_dir(binary, &["start"], cache_dir)?;
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

    match run_zccache_command_raw_in_cache_dir(binary, &["start"], cache_dir) {
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

pub(crate) fn run_zccache_command_raw_in_cache_dir(
    binary: &std::path::Path,
    args: &[&str],
    cache_dir: &std::path::Path,
) -> Result<std::process::Output, SoldrError> {
    run_zccache_command_raw_with_env(
        binary,
        args,
        &[(
            soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

fn run_zccache_command_with_env(
    binary: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
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
    binary: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<std::process::Output, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    suppress_windows_console_window(&mut command);
    Ok(command.output()?)
}

fn run_zccache_command_raw_strings_with_env(
    binary: &std::path::Path,
    args: &[String],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<std::process::Output, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    suppress_windows_console_window(&mut command);
    Ok(command.output()?)
}

fn zccache_command_failure_message(args: &[&str], output: &std::process::Output) -> String {
    format!(
        "zccache {} failed: {}",
        args.join(" "),
        command_stderr(output)
    )
}

fn zccache_command_failure_message_strings(
    args: &[String],
    output: &std::process::Output,
) -> String {
    format!(
        "zccache {} failed: {}",
        args.join(" "),
        command_stderr(output)
    )
}

pub(crate) fn command_stderr(output: &std::process::Output) -> String {
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
    use std::ffi::OsStr;

    #[test]
    fn rustc_wrapper_override_defaults_to_managed_zccache() {
        assert_eq!(
            rustc_wrapper_mode_from_env_var(None),
            RustcWrapperMode::ManagedZccache
        );
    }

    #[test]
    fn rustc_wrapper_override_disables_wrapper_for_empty_or_none() {
        for value in ["", " ", "none", "NONE"] {
            assert_eq!(
                rustc_wrapper_mode_from_env_var(Some(OsStr::new(value))),
                RustcWrapperMode::Disabled,
                "expected {value:?} to disable wrapper injection"
            );
        }
    }

    #[test]
    fn rustc_wrapper_override_uses_custom_wrapper_name() {
        assert_eq!(
            rustc_wrapper_mode_from_env_var(Some(OsStr::new("sccache"))),
            RustcWrapperMode::Custom("sccache".into())
        );
    }

    #[test]
    fn sccache_wrapper_detection_accepts_binary_names_and_paths() {
        assert!(is_sccache_wrapper(OsStr::new("sccache")));
        assert!(is_sccache_wrapper(OsStr::new("sccache.exe")));
        assert!(is_sccache_wrapper(OsStr::new("/tmp/tools/sccache")));
        assert!(!is_sccache_wrapper(OsStr::new("zccache")));
        assert!(!is_sccache_wrapper(OsStr::new("sccache-proxy")));
    }

    // Parent-cache L1.x env injection (issue #352). The decision function
    // takes the inherited values of `ZCCACHE_PATH_REMAP` (set by the user)
    // and `SOLDR_PATH_REMAP` (soldr-side escape hatch) and decides whether
    // soldr should inject `ZCCACHE_PATH_REMAP=auto` onto the spawned cargo
    // child. None means do not inject; Some(value) means inject that value.
    //
    // Rules:
    //   1. If the user already set ZCCACHE_PATH_REMAP, do not override.
    //   2. Otherwise read SOLDR_PATH_REMAP (default `auto`). `off`
    //      (case-insensitive) suppresses the injection. Anything else, or
    //      unset, injects `auto`.

    #[test]
    fn path_remap_injects_auto_when_nothing_set() {
        assert_eq!(resolve_path_remap_env(None, None), Some("auto"));
    }

    #[test]
    fn path_remap_skips_when_soldr_override_is_off() {
        assert_eq!(resolve_path_remap_env(None, Some("off")), None);
    }

    #[test]
    fn path_remap_skips_when_soldr_override_is_off_case_insensitive() {
        assert_eq!(resolve_path_remap_env(None, Some("OFF")), None);
        assert_eq!(resolve_path_remap_env(None, Some("Off")), None);
        assert_eq!(resolve_path_remap_env(None, Some(" off ")), None);
    }

    #[test]
    fn path_remap_injects_auto_when_soldr_override_is_auto() {
        assert_eq!(resolve_path_remap_env(None, Some("auto")), Some("auto"));
        assert_eq!(resolve_path_remap_env(None, Some("AUTO")), Some("auto"));
    }

    #[test]
    fn path_remap_preserves_user_value_when_zccache_already_set_to_non_auto() {
        assert_eq!(resolve_path_remap_env(Some("disabled"), None), None);
        assert_eq!(resolve_path_remap_env(Some("disabled"), Some("auto")), None);
        assert_eq!(resolve_path_remap_env(Some(""), None), None);
    }

    #[test]
    fn path_remap_preserves_user_value_when_zccache_already_auto() {
        // User explicitly set `auto` — soldr must not double-inject. The
        // decision function returns None because the env is already correct
        // in the inherited environment.
        assert_eq!(resolve_path_remap_env(Some("auto"), None), None);
        assert_eq!(resolve_path_remap_env(Some("auto"), Some("off")), None);
    }
}
