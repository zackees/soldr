//! Zccache build-session orchestration and zccache subprocess helpers.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::fetch::ZccacheSource;
use crate::{
    current_soldr_binary, fetch_active_zccache, non_empty_env_path, ZccacheSourceArg,
    RUSTC_WRAPPER_OVERRIDE_ENV_VAR,
};

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
                let sccache_dir = crate::cache_lib::sccache_dir(paths);
                std::fs::create_dir_all(&sccache_dir)?;
                cargo.env("SCCACHE_DIR", sccache_dir);
            }
            cargo.env("RUSTC_WRAPPER", wrapper);
            cargo.env_remove(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
            cargo.env_remove(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR);
            Ok(None)
        }
        RustcWrapperMode::Disabled => {
            cargo.env_remove("RUSTC_WRAPPER");
            cargo.env_remove(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
            cargo.env_remove(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR);
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
        ZccacheSourceArg::Managed => fetch_active_zccache(paths).await?,
        ZccacheSourceArg::System => crate::fetch::resolve_system_zccache(paths)?,
    };

    // Source-aware diagnostic (issue #420). The old "soldr: using
    // managed zccache 1.8.1" line printed even when the pinned install
    // won resolution, which sent three perf-cluster runs chasing the
    // wrong binary. Print exactly one line that names the actual
    // source, runtime dir, and version of the binary we just resolved.
    let source = if matches!(zccache_source, ZccacheSourceArg::System) {
        ZccacheSource::None // `--zccache=system` doesn't go through the precedence chain.
    } else {
        crate::fetch::classify_zccache_source(paths, &fetch.binary_path)
    };
    let runtime_dir = fetch
        .binary_path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unknown>".into());
    match source {
        ZccacheSource::Pinned => eprintln!(
            "soldr: zccache source: pinned ({runtime_dir}) version={}",
            fetch.version
        ),
        ZccacheSource::Local => eprintln!(
            "soldr: zccache source: local ({runtime_dir}) version={}",
            fetch.version
        ),
        ZccacheSource::Managed => {
            if fetch.cached {
                eprintln!(
                    "soldr: zccache source: managed ({runtime_dir}) version={} (cached)",
                    fetch.version
                );
            } else {
                eprintln!(
                    "soldr: zccache source: managed ({runtime_dir}) version={} (downloaded)",
                    fetch.version
                );
            }
        }
        ZccacheSource::None => {
            if matches!(zccache_source, ZccacheSourceArg::System) {
                eprintln!(
                    "soldr: zccache source: system ({runtime_dir}) version={}",
                    fetch.version
                );
            } else {
                eprintln!(
                    "soldr: zccache source: unrecognized ({runtime_dir}) version={} \
                     — likely SOLDR_TEST_ZCCACHE_BIN override",
                    fetch.version
                );
            }
        }
    }

    // When the resolved zccache CLI binary differs from the one a
    // previous soldr invocation started the daemon with, the live
    // daemon is stale relative to what we just resolved. This matters
    // most for `SOLDR_ZCCACHE_LOCAL_DIR` debugging (issue #365): the
    // user expects `cargo` invocations to actually run their freshly
    // built daemon, but `zccache start` is a no-op when any daemon
    // is already alive, so a managed daemon from a previous build
    // would keep handling requests. Evict it explicitly here.
    evict_zccache_daemon_if_binary_changed(&fetch.binary_path, &zccache_dir)?;

    start_zccache_with_recovery(&fetch.binary_path, &zccache_dir)?;

    let session_log_path = crate::cache_lib::session_log_path(&zccache_dir);
    let session_log_path_arg = session_log_path.display().to_string();
    let journal_path = crate::cache_lib::session_journal_path(&zccache_dir);
    let journal_path_arg = journal_path.display().to_string();
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
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
        crate::cache_lib::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to parse zccache session id from output: {}",
                session_json.stdout.trim()
            ))
        })?;

    cargo.env("RUSTC_WRAPPER", current_soldr_binary()?);
    cargo.env(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR, &fetch.binary_path);
    cargo.env(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, &zccache_dir);
    cargo.env(
        crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
        &zccache_dir,
    );
    cargo.env(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR, &session_id);

    // Phase 3: tell the soldr-daemon (if running) that this session is
    // linked to a zccache daemon. The PID field is informational —
    // shutdown invokes the global `zccache stop` rather than targeting
    // a specific PID — so we record the session-id hash modulo u32 as a
    // distinct, non-zero token. Fire-and-forget; nothing about the
    // cargo build depends on it.
    let zccache_token = session_id
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
        .max(1);
    crate::daemon::client::link_zccache(paths, zccache_token);

    // Parent-cache (Tier L1.x, issue #352): seed ZCCACHE_PATH_REMAP=auto so
    // multiple worktrees of the same repo share zccache hits. Honor any
    // user-supplied ZCCACHE_PATH_REMAP, and the SOLDR_PATH_REMAP=off
    // escape hatch.
    let user_zccache = std::env::var(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR).ok();
    let soldr_override = std::env::var(crate::cache_lib::SOLDR_PATH_REMAP_ENV_VAR).ok();
    if let Some(value) = resolve_path_remap_env(user_zccache.as_deref(), soldr_override.as_deref())
    {
        cargo.env(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR, value);
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
    let zccache_dir = normalize_path_for_compare(&crate::cache_lib::zccache_dir(paths))?;
    let inherited_soldr_managed_dir =
        non_empty_env_path(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?;
    if let Some(explicit) = non_empty_env_path(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR) {
        let explicit = normalize_path_for_compare(&explicit)?;
        if explicit != zccache_dir && inherited_soldr_managed_dir.as_ref() != Some(&explicit) {
            return Err(SoldrError::Other(format!(
                "{} is managed by soldr for managed zccache builds. Unset it, set SOLDR_CACHE_DIR to choose soldr's cache root, or set SOLDR_RUSTC_WRAPPER to use a custom wrapper.",
                crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR
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
            crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
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

/// Name of the marker file inside the zccache cache dir that records
/// which CLI binary path soldr last started a daemon with. When the
/// resolved binary path changes between runs (e.g. user toggled
/// `SOLDR_ZCCACHE_LOCAL_DIR`, bumped `MANAGED_ZCCACHE_VERSION`, or
/// rebuilt zccache locally so its content-hashed dir name changed),
/// soldr evicts the live daemon before starting so the next
/// `zccache start` actually spawns the new binary. Issue #365.
pub(crate) const ZCCACHE_LAST_CLI_BINARY_SENTINEL: &str = "soldr-last-cli-binary";

/// Pure decision: should soldr evict the running zccache daemon
/// before starting a fresh one?
///
/// `current` is the absolute path of the CLI binary the current
/// invocation just resolved. `previous` is the contents of the
/// sentinel file from the last invocation (already trimmed), or
/// `None` if the sentinel is absent or unreadable.
///
/// First run (no sentinel) returns `false` — nothing to evict.
/// Path change returns `true`. Same path returns `false`.
pub(crate) fn should_evict_zccache_daemon(current: &str, previous: Option<&str>) -> bool {
    match previous {
        None => false,
        Some(prev) if prev == current => false,
        Some(_) => true,
    }
}

/// If a previous invocation recorded a different CLI binary path,
/// run `zccache stop` to evict the stale daemon. Best-effort: any
/// I/O failure is logged but not propagated, because the start path
/// below has its own stale-daemon recovery.
pub(crate) fn evict_zccache_daemon_if_binary_changed(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<(), SoldrError> {
    let sentinel = cache_dir.join(ZCCACHE_LAST_CLI_BINARY_SENTINEL);
    let resolved = binary.display().to_string();
    let prev = std::fs::read_to_string(&sentinel)
        .ok()
        .map(|s| s.trim().to_string());

    if should_evict_zccache_daemon(&resolved, prev.as_deref()) {
        eprintln!(
            "soldr: zccache CLI binary changed since last build; stopping stale daemon to force a fresh spawn (issue #365)",
        );
        if let Err(err) = run_zccache_command_raw_in_cache_dir(binary, &["stop"], cache_dir) {
            // Stop failures shouldn't block the build — the start
            // path below has its own stale-daemon recovery.
            eprintln!("soldr: zccache stop reported {err}; continuing");
        }
    }

    // Record the current resolution so future invocations can detect
    // the next change. Best-effort write — failure here is non-fatal.
    if let Err(err) = std::fs::write(&sentinel, &resolved) {
        eprintln!(
            "soldr: failed to record current zccache CLI binary at {}: {err}",
            sentinel.display()
        );
    }
    Ok(())
}

/// Soldr-side escape hatch for the RUST_LOG value that gets injected into
/// `zccache start`. Power users can set this to something like
/// `info,zccache_artifact=debug` when they need a specific level on the
/// daemon without the daemon inheriting (and being narrowed by) the
/// parent's RUST_LOG.
pub(crate) const SOLDR_DAEMON_RUST_LOG_ENV_VAR: &str = "SOLDR_DAEMON_RUST_LOG";

/// Compute the `RUST_LOG` value soldr should set when invoking
/// `zccache start`. The daemon spawned by `zccache start` inherits the
/// invocation's env; if `RUST_LOG` narrows the filter to a subset of
/// `zccache_*` modules (e.g. `zccache_daemon=info`), INFO logs from
/// sibling crates (`zccache_artifact`, `zccache_fscache`, etc.) silently
/// vanish from the daemon spawn log. To keep cross-crate observability
/// soldr forces a bare `info` directive at daemon spawn unless the user
/// asks for something specific via `SOLDR_DAEMON_RUST_LOG`.
///
/// Returns the value soldr should pass through as `RUST_LOG` on the
/// `zccache start` invocation. See issue #416.
pub(crate) fn effective_daemon_rust_log(soldr_override: Option<&str>) -> String {
    match soldr_override {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => "info".to_string(),
    }
}

fn run_zccache_start_command(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<std::process::Output, SoldrError> {
    let rust_log = effective_daemon_rust_log(
        std::env::var(SOLDR_DAEMON_RUST_LOG_ENV_VAR).ok().as_deref(),
    );
    run_zccache_command_raw_with_env(
        binary,
        &["start"],
        &[
            (
                crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
                cache_dir.as_os_str(),
            ),
            ("RUST_LOG", std::ffi::OsStr::new(rust_log.as_str())),
        ],
    )
}

pub(crate) fn start_zccache_with_recovery(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
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

pub(crate) fn run_zccache_command_raw_in_cache_dir(
    binary: &std::path::Path,
    args: &[&str],
    cache_dir: &std::path::Path,
) -> Result<std::process::Output, SoldrError> {
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

    // ---------------------------------------------------------------
    // Stale-daemon eviction when the resolved CLI binary changes
    // between soldr invocations (issue #365).
    // ---------------------------------------------------------------

    #[test]
    fn evict_decision_skips_first_run_with_no_sentinel() {
        // Nothing recorded yet — no daemon to evict.
        assert!(!should_evict_zccache_daemon(
            "/path/zccache-1.8.1/zccache.exe",
            None
        ));
    }

    #[test]
    fn evict_decision_skips_when_path_unchanged() {
        let current = "/path/zccache-1.8.1/zccache.exe";
        assert!(!should_evict_zccache_daemon(current, Some(current)));
    }

    #[test]
    fn evict_decision_triggers_when_local_dir_overrides_managed() {
        // The user just exported SOLDR_ZCCACHE_LOCAL_DIR but a stale
        // managed daemon is still alive. Issue #365 acceptance: the
        // next soldr invocation must evict.
        let previous = "/home/u/.soldr/bin/zccache-1.8.1/zccache.exe";
        let current = "/home/u/.soldr/bin/zccache-local-219d33e77197/zccache.exe";
        assert!(should_evict_zccache_daemon(current, Some(previous)));
    }

    #[test]
    fn evict_decision_triggers_when_local_dir_rebuilt() {
        // User rebuilt zccache; content hash changed; the resolved
        // CLI path is a different `zccache-local-<hash>` directory.
        let previous = "/home/u/.soldr/bin/zccache-local-aaaaaaaaaaaa/zccache.exe";
        let current = "/home/u/.soldr/bin/zccache-local-bbbbbbbbbbbb/zccache.exe";
        assert!(should_evict_zccache_daemon(current, Some(previous)));
    }

    #[test]
    fn evict_decision_triggers_when_local_dir_reverts_to_managed() {
        // User unset SOLDR_ZCCACHE_LOCAL_DIR — switching back to the
        // managed path must also evict the stale local daemon.
        let previous = "/home/u/.soldr/bin/zccache-local-219d33e77197/zccache.exe";
        let current = "/home/u/.soldr/bin/zccache-1.8.1/zccache.exe";
        assert!(should_evict_zccache_daemon(current, Some(previous)));
    }

    // ---------------------------------------------------------------
    // RUST_LOG injection on daemon spawn (issue #416). The daemon
    // inherits the env of the `zccache start` invocation; without a
    // soldr-side override, narrow RUST_LOG values in the parent (CI
    // configs, shell exports) silently filter out sibling-crate INFO
    // logs from the daemon spawn log. Soldr forces a non-narrowing
    // directive unless the user explicitly opts in via
    // SOLDR_DAEMON_RUST_LOG.
    // ---------------------------------------------------------------

    #[test]
    fn daemon_rust_log_defaults_to_info_when_override_unset() {
        assert_eq!(effective_daemon_rust_log(None), "info");
    }

    #[test]
    fn daemon_rust_log_defaults_to_info_when_override_is_blank() {
        // Empty / whitespace-only override is treated as unset so accidental
        // `export SOLDR_DAEMON_RUST_LOG=` doesn't re-introduce the narrow
        // filter the env var exists to defeat.
        assert_eq!(effective_daemon_rust_log(Some("")), "info");
        assert_eq!(effective_daemon_rust_log(Some("   ")), "info");
    }

    #[test]
    fn daemon_rust_log_honors_explicit_override() {
        assert_eq!(effective_daemon_rust_log(Some("debug")), "debug");
        assert_eq!(
            effective_daemon_rust_log(Some("info,zccache_artifact=debug")),
            "info,zccache_artifact=debug"
        );
    }

    #[test]
    fn daemon_rust_log_override_passes_through_narrow_directive() {
        // If the user explicitly asks for a single-target directive via
        // SOLDR_DAEMON_RUST_LOG, that's a conscious choice and soldr respects
        // it — the override exists specifically to give power users this knob.
        assert_eq!(
            effective_daemon_rust_log(Some("zccache_daemon=trace")),
            "zccache_daemon=trace"
        );
    }
}
