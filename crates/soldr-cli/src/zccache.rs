//! Zccache build-session orchestration and zccache subprocess helpers.
//! Extracted from `main.rs` as part of issue #339.

use crate::{
    current_soldr_binary, fetch_managed_zccache, non_empty_env_path, RUSTC_WRAPPER_OVERRIDE_ENV_VAR,
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

pub(crate) async fn prepare_rustc_wrapper(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
) -> Result<Option<ZccacheBuildSession>, SoldrError> {
    match rustc_wrapper_mode() {
        RustcWrapperMode::ManagedZccache => prepare_zccache_build(cargo, paths).await.map(Some),
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
) -> Result<ZccacheBuildSession, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;
    let fetch = fetch_managed_zccache(paths).await?;
    if fetch.cached {
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

fn start_zccache_with_recovery(
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
