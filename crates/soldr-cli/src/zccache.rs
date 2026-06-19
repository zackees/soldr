//! Zccache build-session orchestration and zccache subprocess helpers.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{SoldrError, SoldrPaths};
use crate::fetch::{ZccacheRuntime, ZccacheRuntimeSource};
use crate::zccache_lifecycle::{
    zccache_command_failure_message, zccache_json_flag_unsupported, ZccacheLifecycle,
    ZccachePrivateEnv, ZccacheSessionStartOptions,
};
use crate::{
    current_soldr_binary, fetch_active_zccache_runtime, non_empty_env_path, ZccacheSourceArg,
    RUSTC_WRAPPER_OVERRIDE_ENV_VAR,
};
use std::ffi::OsStr;

pub(crate) use crate::zccache_lifecycle::{
    command_stderr, effective_daemon_rust_log, run_zccache_command_in_cache_dir,
    run_zccache_command_raw_in_cache_dir, run_zccache_command_raw_in_cache_dir_with_daemon_name,
    run_zccache_command_strings_in_cache_dir,
    run_zccache_command_strings_in_cache_dir_with_daemon_name, start_zccache_with_recovery,
    ZccacheBuildSession, SOLDR_DAEMON_RUST_LOG_ENV_VAR, ZCCACHE_DAEMON_NAMESPACE_ENV_VAR,
};

pub(crate) const SOLDR_CACHE_LIFECYCLE_ENV_VAR: &str = "SOLDR_CACHE_LIFECYCLE";
pub(crate) const SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR: &str =
    "SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS";
pub(crate) const SOLDR_ZCCACHE_PRIVATE_DAEMON_NAME_ENV_VAR: &str =
    "SOLDR_ZCCACHE_PRIVATE_DAEMON_NAME";
pub(crate) const SOLDR_ZCCACHE_SESSION_DIR_ENV_VAR: &str = "SOLDR_ZCCACHE_SESSION_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheLifecycle {
    Job,
    Command,
}

pub(crate) fn cache_lifecycle_from_env() -> Result<CacheLifecycle, SoldrError> {
    cache_lifecycle_from_env_value(std::env::var_os(SOLDR_CACHE_LIFECYCLE_ENV_VAR).as_deref())
}

pub(crate) fn cache_lifecycle_from_env_value(
    value: Option<&OsStr>,
) -> Result<CacheLifecycle, SoldrError> {
    let Some(value) = value else {
        return Ok(CacheLifecycle::Job);
    };
    let value = value.to_string_lossy();
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "" | "job" | "job-long" | "job-long-cache" | "default" => Ok(CacheLifecycle::Job),
        "command" | "command-lifetime" | "self-build" => Ok(CacheLifecycle::Command),
        _ => Err(SoldrError::Other(format!(
            "{SOLDR_CACHE_LIFECYCLE_ENV_VAR} must be 'job' or 'command' (got {value:?})"
        ))),
    }
}

pub(crate) fn command_lifetime_shutdown_timeout() -> Result<std::time::Duration, SoldrError> {
    Ok(std::time::Duration::from_secs(
        command_lifetime_shutdown_timeout_seconds()?,
    ))
}

fn command_lifetime_shutdown_timeout_seconds() -> Result<u64, SoldrError> {
    match std::env::var(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR) {
        Ok(raw) => parse_shutdown_timeout_seconds(&raw),
        Err(std::env::VarError::NotPresent) => Ok(30),
        Err(err) => Err(SoldrError::Other(format!(
            "{SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR} is not valid Unicode: {err}"
        ))),
    }
}

pub(crate) fn parse_shutdown_timeout_seconds(raw: &str) -> Result<u64, SoldrError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(30);
    }
    let seconds = trimmed.parse::<u64>().map_err(|err| {
        SoldrError::Other(format!(
            "{SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR} must be a positive integer number of seconds (got {raw:?}: {err})"
        ))
    })?;
    if seconds == 0 {
        return Err(SoldrError::Other(format!(
            "{SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR} must be greater than zero"
        )));
    }
    Ok(seconds)
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
    // (except that an empty/whitespace-only value is treated as unset).
    if let Some(value) = user_zccache {
        if !value.trim().is_empty() {
            return None;
        }
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

pub(crate) fn path_remap_auto_active(
    user_zccache: Option<&str>,
    soldr_override: Option<&str>,
) -> bool {
    match user_zccache
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => value.eq_ignore_ascii_case("auto"),
        None => resolve_path_remap_env(None, soldr_override).is_some(),
    }
}

pub(crate) fn resolve_worktree_root_env(
    user_worktree_root: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if user_worktree_root.is_some_and(|value| !value.is_empty()) {
        return None;
    }

    find_git_worktree_root(cwd).or_else(|| Some(cwd.to_path_buf()))
}

pub(crate) fn find_git_worktree_root(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    for candidate in cwd.ancestors() {
        let dot_git = candidate.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ZccacheChildEnv {
    pub(crate) path_remap: Option<&'static str>,
    pub(crate) worktree_root: Option<std::path::PathBuf>,
}

impl ZccacheChildEnv {
    pub(crate) fn from_current_process() -> Result<Self, SoldrError> {
        let user_zccache = std::env::var(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR).ok();
        let soldr_override = std::env::var(crate::cache_lib::SOLDR_PATH_REMAP_ENV_VAR).ok();
        let user_worktree_root = std::env::var_os(crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR);
        let cwd = std::env::current_dir()?;
        Ok(Self::from_inputs(
            user_zccache.as_deref(),
            soldr_override.as_deref(),
            user_worktree_root.as_deref(),
            &cwd,
        ))
    }

    pub(crate) fn from_inputs(
        user_zccache: Option<&str>,
        soldr_override: Option<&str>,
        user_worktree_root: Option<&std::ffi::OsStr>,
        cwd: &std::path::Path,
    ) -> Self {
        let path_remap = resolve_path_remap_env(user_zccache, soldr_override);
        let worktree_root = if path_remap_auto_active(user_zccache, soldr_override) {
            resolve_worktree_root_env(user_worktree_root, cwd)
        } else {
            None
        };
        Self {
            path_remap,
            worktree_root,
        }
    }

    pub(crate) fn apply_to_command(&self, cargo: &mut std::process::Command) {
        if let Some(value) = self.path_remap {
            cargo.env(crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR, value);
        }
        if let Some(root) = self.worktree_root.as_ref() {
            cargo.env(crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR, root);
        }
    }

    pub(crate) fn private_env_assignments(&self) -> Vec<ZccachePrivateEnv> {
        let mut entries = Vec::new();
        if let Some(value) = self.path_remap {
            entries.push(ZccachePrivateEnv::new(
                crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR,
                value,
            ));
        }
        if let Some(root) = self.worktree_root.as_ref() {
            entries.push(ZccachePrivateEnv::new(
                crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR,
                root.display().to_string(),
            ));
        }
        entries
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedZccacheWrapperPlan {
    pub(crate) session: ZccacheBuildSession,
    pub(crate) child_env: ZccacheChildEnv,
}

#[derive(Debug, Clone)]
pub(crate) enum RustcWrapperPlan {
    ManagedZccache(Box<ManagedZccacheWrapperPlan>),
    Custom {
        wrapper: std::ffi::OsString,
        sccache_dir: Option<std::path::PathBuf>,
    },
    Disabled,
}

impl RustcWrapperPlan {
    pub(crate) fn session(&self) -> Option<&ZccacheBuildSession> {
        match self {
            Self::ManagedZccache(plan) => Some(&plan.session),
            Self::Custom { .. } | Self::Disabled => None,
        }
    }

    pub(crate) fn apply_to_command(
        &self,
        cargo: &mut std::process::Command,
    ) -> Result<(), SoldrError> {
        match self {
            Self::ManagedZccache(plan) => {
                let session = &plan.session;
                cargo.env("RUSTC_WRAPPER", current_soldr_binary()?);
                cargo.env(
                    crate::cache_lib::ZCCACHE_BINARY_ENV_VAR,
                    &session.binary_path,
                );
                cargo.env(SOLDR_ZCCACHE_SESSION_DIR_ENV_VAR, &session.cache_dir);
                if session.cache_dir_env {
                    cargo.env(
                        crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
                        &session.cache_dir,
                    );
                    cargo.env(
                        crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
                        &session.cache_dir,
                    );
                } else {
                    cargo.env_remove(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR);
                    cargo.env_remove(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
                }
                cargo.env(
                    crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR,
                    &session.session_id,
                );
                if let Some(private) = session.private_daemon.as_ref() {
                    cargo.env(ZCCACHE_DAEMON_NAMESPACE_ENV_VAR, &private.daemon_name);
                } else {
                    cargo.env_remove(ZCCACHE_DAEMON_NAMESPACE_ENV_VAR);
                }
                plan.child_env.apply_to_command(cargo);
            }
            Self::Custom {
                wrapper,
                sccache_dir,
            } => {
                if let Some(sccache_dir) = sccache_dir {
                    cargo.env("SCCACHE_DIR", sccache_dir);
                }
                cargo.env("RUSTC_WRAPPER", wrapper);
                remove_managed_zccache_env(cargo);
            }
            Self::Disabled => {
                cargo.env_remove("RUSTC_WRAPPER");
                remove_managed_zccache_env(cargo);
            }
        }
        Ok(())
    }
}

fn remove_managed_zccache_env(cargo: &mut std::process::Command) {
    cargo.env_remove(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR);
    cargo.env_remove(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
    cargo.env_remove(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR);
}

pub(crate) async fn prepare_rustc_wrapper(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
    zccache_source: ZccacheSourceArg,
) -> Result<Option<ZccacheBuildSession>, SoldrError> {
    let plan = prepare_rustc_wrapper_plan(paths, zccache_source).await?;
    let session = plan.session().cloned();
    plan.apply_to_command(cargo)?;
    Ok(session)
}

pub(crate) async fn prepare_rustc_wrapper_plan(
    paths: &SoldrPaths,
    zccache_source: ZccacheSourceArg,
) -> Result<RustcWrapperPlan, SoldrError> {
    match rustc_wrapper_mode() {
        RustcWrapperMode::ManagedZccache => prepare_zccache_build(paths, zccache_source)
            .await
            .map(|plan| RustcWrapperPlan::ManagedZccache(Box::new(plan))),
        RustcWrapperMode::Custom(wrapper) => {
            let sccache_dir =
                if is_sccache_wrapper(&wrapper) && std::env::var_os("SCCACHE_DIR").is_none() {
                    let sccache_dir = crate::cache_lib::sccache_dir(paths);
                    std::fs::create_dir_all(&sccache_dir)?;
                    Some(sccache_dir)
                } else {
                    None
                };
            Ok(RustcWrapperPlan::Custom {
                wrapper,
                sccache_dir,
            })
        }
        RustcWrapperMode::Disabled => Ok(RustcWrapperPlan::Disabled),
    }
}

pub(crate) fn is_sccache_wrapper(wrapper: &std::ffi::OsStr) -> bool {
    std::path::Path::new(wrapper)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"))
}

async fn prepare_zccache_build(
    paths: &SoldrPaths,
    zccache_source: ZccacheSourceArg,
) -> Result<ManagedZccacheWrapperPlan, SoldrError> {
    let zccache_base_dir = managed_zccache_cache_dir(paths)?;
    let runtime = match zccache_source {
        ZccacheSourceArg::Managed => fetch_active_zccache_runtime(paths).await?,
        ZccacheSourceArg::System => {
            crate::fetch::ZccacheResolver::new(paths)?.materialize_system()?
        }
    };
    let fetch = runtime.to_fetch_result()?;

    // Source-aware diagnostic (issue #420). The old "soldr: using
    // managed zccache 1.8.1" line printed even when the pinned install
    // won resolution, which sent three perf-cluster runs chasing the
    // wrong binary. Print exactly one line that names the actual
    // source, runtime dir, and version of the binary we just resolved.
    print_zccache_runtime_diagnostic(&runtime);

    let child_env = ZccacheChildEnv::from_current_process()?;
    let inherited_soldr_managed_dir =
        non_empty_env_path(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?;
    let explicit_zccache_cache_dir =
        non_empty_env_path(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?
            .filter(|path| inherited_soldr_managed_dir.as_ref() != Some(path));
    let cache_dir_env = explicit_zccache_cache_dir.is_some();
    let zccache_dir = explicit_zccache_cache_dir.unwrap_or(zccache_base_dir);
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;

    // When the resolved zccache CLI binary differs from the one a
    // previous soldr invocation started the daemon with, the live
    // daemon is stale relative to what we just resolved. This matters
    // most for `SOLDR_ZCCACHE_LOCAL_DIR` debugging (issue #365): the
    // user expects `cargo` invocations to actually run their freshly
    // built daemon, but `zccache start` is a no-op when any daemon
    // is already alive, so a managed daemon from a previous build
    // would keep handling requests. Evict it explicitly here.
    evict_zccache_daemon_if_binary_changed_scoped(
        &fetch.binary_path,
        &zccache_dir,
        cache_dir_env,
        None,
    )?;

    // Refresh stale `runtime-binaries/` from a previous pinned zccache
    // version. The namespace key is `(soldr-binary-path,
    // zccache-binary-path)` and stays stable across `pip install -U
    // soldr` bumps that flip `MANAGED_ZCCACHE_VERSION` while keeping
    // the soldr binary at the same on-disk path; without this check
    // zccache's own "directory already populated -> skip copy"
    // short-circuit keeps the old daemon binary forever. Issue #714.
    refresh_runtime_binaries_if_version_changed_scoped(
        &fetch.binary_path,
        &zccache_dir,
        cache_dir_env,
        None,
        &runtime.version,
    )?;

    let session_log_path = crate::cache_lib::session_log_path(&zccache_dir);
    let journal_path = crate::cache_lib::session_journal_path(&zccache_dir);
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
    let mut lifecycle =
        ZccacheLifecycle::new(&fetch.binary_path, &zccache_dir).with_cache_dir_env(cache_dir_env);
    let session = lifecycle.start_session(ZccacheSessionStartOptions {
        id: None,
        session_log_path,
        journal_path,
        session_stats_path,
    })?;

    // Tell the soldr-daemon which concrete zccache runtime/cache/session
    // this build owns so daemon shutdown can stop only that scoped daemon.
    crate::daemon::client::link_zccache(
        paths,
        crate::daemon::protocol::ZccacheDaemonLink {
            binary_path: session.binary_path.display().to_string(),
            cache_dir: session.cache_dir.display().to_string(),
            session_id: Some(session.session_id.clone()),
            source: runtime.source.summary_source().as_str().to_string(),
            private_daemon: session.private_daemon.is_some(),
            daemon_name: session
                .private_daemon
                .as_ref()
                .map(|private| private.daemon_name.clone()),
            owner_pid: session
                .private_daemon
                .as_ref()
                .and_then(|private| private.owner_pid),
            private_env_keys: session
                .private_daemon
                .as_ref()
                .map(|private| private.private_env_keys.clone())
                .unwrap_or_default(),
        },
    );

    Ok(ManagedZccacheWrapperPlan {
        session,
        // Parent-cache (Tier L1.x, issue #352): seed ZCCACHE_PATH_REMAP=auto
        // and a logical worktree root so multiple worktrees of the same repo
        // share zccache hits through normalized keys. Honor user-supplied
        // ZCCACHE_* values, and the SOLDR_PATH_REMAP=off escape hatch.
        child_env,
    })
}

fn print_zccache_runtime_diagnostic(runtime: &ZccacheRuntime) {
    let runtime_dir = runtime.runtime_dir.display();
    match runtime.source {
        ZccacheRuntimeSource::Pinned => eprintln!(
            "soldr: zccache source: pinned ({runtime_dir}) version={}",
            runtime.version
        ),
        ZccacheRuntimeSource::Local => eprintln!(
            "soldr: zccache source: local ({runtime_dir}) version={}",
            runtime.version
        ),
        ZccacheRuntimeSource::ManagedCached => eprintln!(
            "soldr: zccache source: managed ({runtime_dir}) version={} (cached)",
            runtime.version
        ),
        ZccacheRuntimeSource::ManagedDownloaded => eprintln!(
            "soldr: zccache source: managed ({runtime_dir}) version={} (downloaded)",
            runtime.version
        ),
        ZccacheRuntimeSource::ManagedFallback => eprintln!(
            "soldr: zccache source: managed ({runtime_dir}) version={} (cargo-install fallback)",
            runtime.version
        ),
        ZccacheRuntimeSource::System => eprintln!(
            "soldr: zccache source: system ({runtime_dir}) version={}",
            runtime.version
        ),
        ZccacheRuntimeSource::TestOverride => eprintln!(
            "soldr: zccache source: test-override ({runtime_dir}) version={}",
            runtime.version
        ),
        ZccacheRuntimeSource::Missing => eprintln!(
            "soldr: zccache source: missing ({runtime_dir}) version={}",
            runtime.version
        ),
    }
}

pub(crate) fn finish_zccache_build(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let lifecycle = ZccacheLifecycle::from_session(session);
    let output = lifecycle.run_raw(&["session-end", &session.session_id, "--json"])?;
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
            print_wasted_depgraph_hits_if_any(&session.session_log_path);
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

pub(crate) fn stop_zccache_after_command(
    session: &ZccacheBuildSession,
    timeout: std::time::Duration,
) -> Result<(), SoldrError> {
    let mut lifecycle = ZccacheLifecycle::from_session(session);
    let output = lifecycle.stop(false)?;
    if output.stopped {
        eprintln!("soldr: command-lifetime cache: zccache stop requested");
    } else if output.already_stopped {
        eprintln!("soldr: command-lifetime cache: zccache daemon already stopped");
        return Ok(());
    } else if let Some(failure) = output.failure {
        return Err(SoldrError::Other(format!("zccache stop failed: {failure}")));
    }

    match lifecycle.poll_daemon_exit(timeout) {
        crate::zccache_lifecycle::ZccacheDaemonExitPollResult::Exited => {
            eprintln!("soldr: command-lifetime cache: zccache daemon exited");
            Ok(())
        }
        crate::zccache_lifecycle::ZccacheDaemonExitPollResult::TimedOut => {
            Err(SoldrError::Other(format!(
                "command-lifetime cache: zccache daemon did not exit within {}s",
                timeout.as_secs()
            )))
        }
        crate::zccache_lifecycle::ZccacheDaemonExitPollResult::PollFailed(err) => {
            Err(SoldrError::Other(format!(
                "command-lifetime cache: could not confirm zccache daemon exit: {err}"
            )))
        }
    }
}

fn finish_zccache_build_text_fallback(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let lifecycle = ZccacheLifecycle::from_session(session);
    let output = lifecycle.run(&["session-end", &session.session_id])?;
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

/// Count "depgraph said Hit, artifact store had no payload" events in
/// a zccache session log. Each `[DIAG] artifact_not_found:` line is a
/// hit-classified lookup that fell through to a forced miss — i.e. the
/// cache *thought* it had the unit, the lookup succeeded, then the
/// storage layer dropped it. Surfaces issue #704's "depgraph Hit +
/// artifact_not_found -> forced miss" pattern without grep'ing the log.
///
/// Pure parser over the log body — caller is responsible for reading
/// the file (so this stays testable without filesystem I/O).
pub(crate) fn count_wasted_depgraph_hits(log_body: &str) -> u64 {
    log_body
        .lines()
        .filter(|line| line.trim_start().starts_with("[DIAG] artifact_not_found:"))
        .count() as u64
}

/// Cap on how much of the session log we'll read to compute the wasted-
/// hit count. 16 MiB covers full debug sessions on dogfood builds with
/// substantial headroom; anything larger almost certainly has bigger
/// problems than a missing diagnostic line.
const SESSION_LOG_READ_CAP: u64 = 16 * 1024 * 1024;

/// Emit a `  wasted depgraph hits: N` summary line when the log shows
/// any artifact-not-found events. Silent when the log is missing,
/// unreadable, oversized, or shows zero wasted hits — this is purely
/// additive diagnostic surface and must not break existing output.
fn print_wasted_depgraph_hits_if_any(session_log_path: &std::path::Path) {
    let Ok(metadata) = std::fs::metadata(session_log_path) else {
        return;
    };
    if metadata.len() > SESSION_LOG_READ_CAP {
        return;
    }
    let Ok(body) = std::fs::read_to_string(session_log_path) else {
        return;
    };
    let wasted = count_wasted_depgraph_hits(&body);
    if wasted > 0 {
        eprintln!(
            "  wasted depgraph hits: {wasted} (depgraph said Hit but artifact store had \
             no payload; see issue #704)"
        );
    }
}

pub(crate) fn private_zccache_cache_dir(
    base_dir: &std::path::Path,
    daemon_name: &str,
) -> std::path::PathBuf {
    base_dir.join("private").join(daemon_name)
}

pub(crate) fn resolve_private_zccache_daemon_name(
    override_name: Option<&str>,
    soldr_binary: &std::path::Path,
    zccache_binary: &std::path::Path,
    base_cache_dir: &std::path::Path,
) -> String {
    if let Some(name) = override_name.and_then(sanitize_zccache_daemon_name) {
        return name;
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"soldr-private-zccache-v1\0");
    hash_path_component(&mut hasher, soldr_binary);
    let zccache_identity = zccache_binary_identity_path(zccache_binary, base_cache_dir);
    hash_path_component(&mut hasher, zccache_identity.as_ref());
    let digest = hasher.finalize();
    let hex = digest.to_hex();
    format!("soldr-dev-{}", &hex[..16])
}

fn zccache_binary_identity_path<'a>(
    zccache_binary: &'a std::path::Path,
    base_cache_dir: &'a std::path::Path,
) -> std::borrow::Cow<'a, std::path::Path> {
    if let Some(root) = soldr_root_from_zccache_base_dir(base_cache_dir) {
        if let Ok(relative) = zccache_binary.strip_prefix(&root) {
            return std::borrow::Cow::Owned(relative.to_path_buf());
        }
    }
    std::borrow::Cow::Borrowed(zccache_binary)
}

fn soldr_root_from_zccache_base_dir(
    base_cache_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if !path_file_name_eq(base_cache_dir, "zccache") {
        return None;
    }
    let cache_dir = base_cache_dir.parent()?;
    if !path_file_name_eq(cache_dir, "cache") {
        return None;
    }
    cache_dir.parent().map(std::path::Path::to_path_buf)
}

fn path_file_name_eq(path: &std::path::Path, expected: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(expected))
}

fn hash_path_component(hasher: &mut blake3::Hasher, path: &std::path::Path) {
    let value = path.display().to_string();
    let value = if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    };
    hasher.update(value.as_bytes());
    hasher.update(b"\0");
}

pub(crate) fn sanitize_zccache_daemon_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() <= 32 {
        return Some(sanitized);
    }
    let prefix: String = sanitized.chars().take(23).collect();
    let short = blake3::hash(trimmed.as_bytes()).to_hex();
    Some(format!("{prefix}-{}", &short[..8]))
}

pub(crate) fn managed_zccache_cache_dir(
    paths: &SoldrPaths,
) -> Result<std::path::PathBuf, SoldrError> {
    normalize_path_for_compare(&crate::cache_lib::zccache_dir(paths))
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
    evict_zccache_daemon_if_binary_changed_scoped(binary, cache_dir, true, None)
}

pub(crate) fn evict_zccache_daemon_if_binary_changed_scoped(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
    cache_dir_env: bool,
    daemon_name: Option<&str>,
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
        if let Err(err) = run_zccache_command_raw_in_cache_dir_with_daemon_name(
            binary,
            &["stop"],
            cache_dir,
            cache_dir_env,
            daemon_name,
        ) {
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

/// Name of the JSON manifest soldr writes alongside the
/// `runtime-binaries/` subdir that zccache populates inside a
/// private-daemon namespace. Records the zccache version that was
/// current the last time soldr (re)established the dir. The
/// namespace key is `(soldr-binary-path, zccache-binary-path)`,
/// which stays stable across `pip install -U soldr` bumps that flip
/// `MANAGED_ZCCACHE_VERSION` without moving the soldr binary on
/// disk. Without this marker, zccache's own "directory already
/// populated -> skip copy" short-circuit keeps the OLD daemon
/// binary in place forever. Issue #714.
pub(crate) const RUNTIME_BINARIES_MANIFEST_FILENAME: &str = "soldr-runtime-binaries.manifest.json";

/// Pure decision: should soldr refresh the namespace's
/// `runtime-binaries/` subdir before starting a fresh daemon?
///
/// - `current` is the zccache version soldr just resolved
///   (`ZccacheRuntime::version`).
/// - `previous` is the version recorded in the manifest from the
///   last refresh, or `None` if absent / unreadable.
/// - `runtime_binaries_exists` is whether `runtime-binaries/`
///   currently exists inside the namespace dir.
///
/// Returns `true` when the dir exists and either (a) the recorded
/// version differs from the current one, or (b) the manifest is
/// missing — we cannot prove the dir is current, so refresh
/// defensively. This handles the pre-fix migration: namespaces that
/// existed before this manifest was introduced all refresh exactly
/// once on the first post-fix soldr invocation.
///
/// Returns `false` when there is no `runtime-binaries/` dir to
/// refresh (zccache will populate it on first session-start) OR
/// when the recorded version matches.
pub(crate) fn should_refresh_runtime_binaries(
    current: &str,
    previous: Option<&str>,
    runtime_binaries_exists: bool,
) -> bool {
    if !runtime_binaries_exists {
        return false;
    }
    match previous {
        None => true,
        Some(prev) if prev == current => false,
        Some(_) => true,
    }
}

/// If the `runtime-binaries/` subdir under `cache_dir` was populated
/// by a previous soldr invocation pinned to a different zccache
/// version, stop the running daemon and delete the dir so the next
/// `zccache session-start` repopulates from the current
/// `~/.soldr/bin/zccache-<VERSION>/`. Always (re)writes the
/// manifest so future invocations can detect the next drift.
/// Best-effort: I/O failures log and continue. Issue #714.
pub(crate) fn refresh_runtime_binaries_if_version_changed_scoped(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
    cache_dir_env: bool,
    daemon_name: Option<&str>,
    current_version: &str,
) -> Result<(), SoldrError> {
    let manifest_path = cache_dir.join(RUNTIME_BINARIES_MANIFEST_FILENAME);
    let runtime_binaries_dir = cache_dir.join("runtime-binaries");
    let previous_version = read_runtime_binaries_manifest_version(&manifest_path);
    let exists = runtime_binaries_dir.exists();

    if should_refresh_runtime_binaries(current_version, previous_version.as_deref(), exists) {
        eprintln!(
            "soldr: refreshing stale zccache runtime-binaries (had={}, want={}) — issue #714",
            previous_version.as_deref().unwrap_or("<no manifest>"),
            current_version,
        );
        if let Err(err) = run_zccache_command_raw_in_cache_dir_with_daemon_name(
            binary,
            &["stop"],
            cache_dir,
            cache_dir_env,
            daemon_name,
        ) {
            // Daemon may already be dead; the subsequent file removal
            // is what actually unblocks the refresh.
            eprintln!(
                "soldr: zccache stop reported {err}; continuing with runtime-binaries refresh"
            );
        }
        if let Err(err) = std::fs::remove_dir_all(&runtime_binaries_dir) {
            eprintln!(
                "soldr: failed to remove stale runtime-binaries at {}: {err}; manifest not updated, will retry on next invocation",
                runtime_binaries_dir.display(),
            );
            // Bail before writing the manifest so next run retries.
            return Ok(());
        }
    }

    write_runtime_binaries_manifest(&manifest_path, current_version);
    Ok(())
}

fn read_runtime_binaries_manifest_version(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("zccache_version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn write_runtime_binaries_manifest(path: &std::path::Path, version: &str) {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = serde_json::json!({
        "zccache_version": version,
        "refreshed_at_unix_secs": now_unix,
    });
    let pretty =
        serde_json::to_string_pretty(&body).expect("constructed json value always serializes");
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!(
                "soldr: failed to create runtime-binaries manifest parent {}: {err}",
                parent.display(),
            );
            return;
        }
    }
    if let Err(err) = std::fs::write(path, pretty) {
        eprintln!(
            "soldr: failed to record runtime-binaries manifest at {}: {err}",
            path.display(),
        );
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

    #[test]
    fn count_wasted_depgraph_hits_returns_zero_when_no_artifact_not_found_lines() {
        let body = "\
[DIAG] depgraph_check: foo verdict=Hit reason=fast_key_match
[HIT] foo (reason: fast_key_match)
[DIAG] depgraph_check: bar verdict=Miss reason=key_mismatch
[MISS] bar (reason: key_mismatch)
";
        assert_eq!(count_wasted_depgraph_hits(body), 0);
    }

    #[test]
    fn count_wasted_depgraph_hits_counts_each_artifact_not_found_line() {
        let body = "\
[DIAG] depgraph_check: foo verdict=Hit reason=fast_key_match
[DIAG] artifact_not_found: key=aaaaaaaa
[MISS] foo (reason: fast_key_match)
[DIAG] depgraph_check: bar verdict=Hit reason=first_check_after_update
[DIAG] artifact_not_found: key=bbbbbbbb
[MISS] bar (reason: first_check_after_update)
";
        assert_eq!(count_wasted_depgraph_hits(body), 2);
    }

    #[test]
    fn count_wasted_depgraph_hits_tolerates_leading_whitespace() {
        // Some log frameworks indent diagnostic lines under a parent span.
        let body = "\
    [DIAG] artifact_not_found: key=aaaaaaaa
\t[DIAG] artifact_not_found: key=bbbbbbbb
[DIAG] artifact_not_found: key=cccccccc
";
        assert_eq!(count_wasted_depgraph_hits(body), 3);
    }

    #[test]
    fn count_wasted_depgraph_hits_ignores_unrelated_diag_lines() {
        let body = "\
[DIAG] depgraph_check: foo verdict=Hit
[DIAG] artifact_size: 12345
[DIAG] artifact_not_found_recovered: key=xxxx
[DIAG] artifact_not_foundish: key=yyyy
[INFO] artifact_not_found: this is not a DIAG line
";
        assert_eq!(count_wasted_depgraph_hits(body), 0);
    }

    #[test]
    fn count_wasted_depgraph_hits_handles_empty_log() {
        assert_eq!(count_wasted_depgraph_hits(""), 0);
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
    }

    #[test]
    fn path_remap_treats_empty_user_value_as_unset() {
        assert_eq!(resolve_path_remap_env(Some(""), None), Some("auto"));
        assert_eq!(resolve_path_remap_env(Some("   "), None), Some("auto"));
        assert_eq!(resolve_path_remap_env(Some(""), Some("off")), None);
    }

    #[test]
    fn path_remap_preserves_user_value_when_zccache_already_auto() {
        // User explicitly set `auto` — soldr must not double-inject. The
        // decision function returns None because the env is already correct
        // in the inherited environment.
        assert_eq!(resolve_path_remap_env(Some("auto"), None), None);
        assert_eq!(resolve_path_remap_env(Some("auto"), Some("off")), None);
    }

    #[test]
    fn path_remap_auto_active_tracks_child_state() {
        assert!(path_remap_auto_active(None, None));
        assert!(path_remap_auto_active(Some(""), None));
        assert!(path_remap_auto_active(Some("auto"), Some("off")));
        assert!(!path_remap_auto_active(None, Some("off")));
        assert!(!path_remap_auto_active(Some("disabled"), None));
    }

    #[test]
    fn worktree_root_env_uses_git_root_by_default() {
        let temp = unique_test_dir("worktree-root-git");
        let root = temp.join("repo");
        let nested = root.join("crates").join("demo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_git_worktree_root(&nested), Some(root.clone()));
        assert_eq!(resolve_worktree_root_env(None, &nested), Some(root));
    }

    #[test]
    fn worktree_root_env_falls_back_to_cwd_without_git_root() {
        let temp = unique_test_dir("worktree-root-cwd");
        let cwd = temp.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(find_git_worktree_root(&cwd), None);
        assert_eq!(resolve_worktree_root_env(None, &cwd), Some(cwd));
    }

    #[test]
    fn worktree_root_env_preserves_user_value() {
        let cwd = std::path::Path::new("/repo");
        assert_eq!(
            resolve_worktree_root_env(Some(OsStr::new("/custom/root")), cwd),
            None
        );
    }

    #[test]
    fn child_env_private_assignments_match_generated_zccache_env() {
        let env = ZccacheChildEnv {
            path_remap: Some("auto"),
            worktree_root: Some(std::path::PathBuf::from("/repo")),
        };
        let entries = env.private_env_assignments();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR);
        assert_eq!(entries[0].value, "auto");
        assert_eq!(
            entries[1].key,
            crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR
        );
        assert_eq!(entries[1].value, "/repo");
    }

    #[test]
    fn private_daemon_name_is_stable_sanitized_and_short() {
        let generated = resolve_private_zccache_daemon_name(
            None,
            std::path::Path::new("/repo/target/debug/soldr"),
            std::path::Path::new("/repo/target/debug/zccache"),
            std::path::Path::new("/tmp/cache/zccache"),
        );
        assert!(generated.starts_with("soldr-dev-"));
        assert!(generated.len() <= 32);
        assert_eq!(
            generated,
            resolve_private_zccache_daemon_name(
                None,
                std::path::Path::new("/repo/target/debug/soldr"),
                std::path::Path::new("/repo/target/debug/zccache"),
                std::path::Path::new("/tmp/cache/zccache"),
            )
        );
        assert_eq!(
            generated,
            resolve_private_zccache_daemon_name(
                None,
                std::path::Path::new("/repo/target/debug/soldr"),
                std::path::Path::new("/repo/target/debug/zccache"),
                std::path::Path::new("/tmp/restored-cache/zccache"),
            ),
            "save/load restores into a new SOLDR_CACHE_DIR must reuse the same private subtree",
        );
        assert_eq!(
            resolve_private_zccache_daemon_name(
                None,
                std::path::Path::new("/repo/target/debug/soldr"),
                std::path::Path::new("/tmp/cache-cold/bin/zccache-1.11.20/zccache"),
                std::path::Path::new("/tmp/cache-cold/cache/zccache"),
            ),
            resolve_private_zccache_daemon_name(
                None,
                std::path::Path::new("/repo/target/debug/soldr"),
                std::path::Path::new("/tmp/cache-warm/bin/zccache-1.11.20/zccache"),
                std::path::Path::new("/tmp/cache-warm/cache/zccache"),
            ),
            "managed zccache binaries under different SOLDR_CACHE_DIR roots must hash by runtime identity, not absolute path",
        );
        assert_ne!(
            resolve_private_zccache_daemon_name(
                None,
                std::path::Path::new("/repo/target/debug/soldr"),
                std::path::Path::new("/tmp/cache/bin/zccache-local-abc/zccache"),
                std::path::Path::new("/tmp/cache/cache/zccache"),
            ),
            resolve_private_zccache_daemon_name(
                None,
                std::path::Path::new("/repo/target/debug/soldr"),
                std::path::Path::new("/tmp/cache/bin/zccache-local-def/zccache"),
                std::path::Path::new("/tmp/cache/cache/zccache"),
            ),
            "different local zccache runtime labels should still get distinct private daemons",
        );
        assert_eq!(
            resolve_private_zccache_daemon_name(
                Some(" team/dev soldr "),
                std::path::Path::new("/ignored/soldr"),
                std::path::Path::new("/ignored/zccache"),
                std::path::Path::new("/ignored/cache"),
            ),
            "team_dev_soldr"
        );
    }

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    // Stale `runtime-binaries/` refresh on managed-zccache version
    // bump (issue #714). Namespace key is (soldr-binary-path,
    // zccache-binary-path) and stays stable when MANAGED_ZCCACHE_VERSION
    // flips under a `pip install -U soldr` bump that doesn't move
    // the soldr binary; zccache's own "directory populated -> skip"
    // short-circuit then keeps the old daemon binary forever.
    // ---------------------------------------------------------------

    #[test]
    fn refresh_decision_skips_when_runtime_binaries_dir_missing() {
        // Fresh namespace, no `runtime-binaries/` yet -> nothing to
        // refresh. zccache populates it on the first session-start;
        // the manifest write that follows establishes the baseline.
        assert!(!should_refresh_runtime_binaries("1.12.4", None, false));
        assert!(!should_refresh_runtime_binaries(
            "1.12.4",
            Some("1.11.8"),
            false
        ));
    }

    #[test]
    fn refresh_decision_skips_when_versions_match() {
        // Steady state: manifest matches what we just resolved.
        assert!(!should_refresh_runtime_binaries(
            "1.12.4",
            Some("1.12.4"),
            true
        ));
    }

    #[test]
    fn refresh_decision_triggers_when_versions_differ() {
        // The actual #714 scenario: user upgraded soldr; namespace
        // already has 1.11.8 binaries copied; soldr now pins 1.12.4.
        assert!(should_refresh_runtime_binaries(
            "1.12.4",
            Some("1.11.8"),
            true
        ));
    }

    #[test]
    fn refresh_decision_triggers_when_manifest_missing_but_dir_exists() {
        // Migration case: namespace was populated by a pre-#714
        // soldr that didn't write the manifest. We can't prove the
        // dir is current, so refresh defensively on the first
        // post-fix invocation. After that the manifest establishes
        // a baseline and subsequent runs become no-ops.
        assert!(should_refresh_runtime_binaries("1.12.4", None, true));
    }

    #[test]
    fn refresh_writes_manifest_for_fresh_namespace() {
        // Fresh namespace: no runtime-binaries/, no daemon. The call
        // should be a no-op aside from establishing the manifest so
        // subsequent invocations recognize the baseline.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmp.path();
        // Point the (unused) binary at our own current_exe so the
        // zccache stop branch — which won't fire — has a valid path
        // if anything regresses.
        let dummy_binary = std::env::current_exe().expect("current_exe");

        refresh_runtime_binaries_if_version_changed_scoped(
            &dummy_binary,
            cache_dir,
            true,
            Some("soldr-dev-test"),
            "1.12.4",
        )
        .expect("fresh-namespace refresh is best-effort and Ok(())");

        let manifest = cache_dir.join(RUNTIME_BINARIES_MANIFEST_FILENAME);
        assert!(manifest.exists(), "manifest must be created on first run");
        let recorded = read_runtime_binaries_manifest_version(&manifest);
        assert_eq!(recorded.as_deref(), Some("1.12.4"));
    }

    #[test]
    fn refresh_no_op_when_manifest_matches_current_version() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmp.path();
        let runtime_binaries = cache_dir.join("runtime-binaries");
        std::fs::create_dir_all(&runtime_binaries).expect("create runtime-binaries");
        // Drop a sentinel file so we can prove the refresh path
        // didn't delete the directory contents on a no-op.
        let sentinel_inside = runtime_binaries.join("untouched-marker");
        std::fs::write(&sentinel_inside, b"keep").expect("write sentinel");

        // Pre-write a matching manifest.
        write_runtime_binaries_manifest(
            &cache_dir.join(RUNTIME_BINARIES_MANIFEST_FILENAME),
            "1.12.4",
        );

        let dummy_binary = std::env::current_exe().expect("current_exe");
        refresh_runtime_binaries_if_version_changed_scoped(
            &dummy_binary,
            cache_dir,
            true,
            Some("soldr-dev-test"),
            "1.12.4",
        )
        .expect("matching-version refresh is Ok(())");

        assert!(
            sentinel_inside.exists(),
            "matching-version refresh must not touch runtime-binaries/"
        );
    }

    #[test]
    fn manifest_round_trip_records_version_field() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(RUNTIME_BINARIES_MANIFEST_FILENAME);
        write_runtime_binaries_manifest(&path, "1.12.4");
        assert_eq!(
            read_runtime_binaries_manifest_version(&path).as_deref(),
            Some("1.12.4")
        );
    }

    #[test]
    fn manifest_read_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("never-existed.json");
        assert!(read_runtime_binaries_manifest_version(&path).is_none());
    }

    #[test]
    fn manifest_read_returns_none_for_corrupt_json() {
        // A corrupt manifest must be treated the same as missing —
        // refresh_decision then triggers a defensive refresh, which
        // is the safe outcome.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(RUNTIME_BINARIES_MANIFEST_FILENAME);
        std::fs::write(&path, b"{ not valid json").expect("write garbage");
        assert!(read_runtime_binaries_manifest_version(&path).is_none());
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

    #[test]
    fn cache_lifecycle_defaults_to_job_long_cache() {
        assert_eq!(
            cache_lifecycle_from_env_value(None).unwrap(),
            CacheLifecycle::Job
        );
        assert_eq!(
            cache_lifecycle_from_env_value(Some(OsStr::new(""))).unwrap(),
            CacheLifecycle::Job
        );
        assert_eq!(
            cache_lifecycle_from_env_value(Some(OsStr::new("job"))).unwrap(),
            CacheLifecycle::Job
        );
    }

    #[test]
    fn cache_lifecycle_accepts_command_lifetime_aliases() {
        for value in ["command", "COMMAND", "command-lifetime", "self-build"] {
            assert_eq!(
                cache_lifecycle_from_env_value(Some(OsStr::new(value))).unwrap(),
                CacheLifecycle::Command,
                "expected {value:?} to enable command-lifetime cache shutdown"
            );
        }
    }

    #[test]
    fn cache_lifecycle_rejects_unknown_values() {
        let err = cache_lifecycle_from_env_value(Some(OsStr::new("forever"))).unwrap_err();
        assert!(
            err.to_string().contains(SOLDR_CACHE_LIFECYCLE_ENV_VAR),
            "expected env var name in error: {err}"
        );
    }

    #[test]
    fn command_lifetime_shutdown_timeout_parser_defaults_and_validates() {
        assert_eq!(parse_shutdown_timeout_seconds("").unwrap(), 30);
        assert_eq!(parse_shutdown_timeout_seconds(" 5 ").unwrap(), 5);
        assert!(parse_shutdown_timeout_seconds("0").is_err());
        assert!(parse_shutdown_timeout_seconds("abc").is_err());
    }
}
