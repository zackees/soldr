pub(crate) const NO_GC_TARGET_FLAG: &str = "--no-gc-target";
pub(crate) const NO_GC_TARGET_BEFORE_FLAG: &str = "--no-gc-target-before";
pub(crate) const NO_GC_TARGET_AFTER_FLAG: &str = "--no-gc-target-after";
pub(crate) const DYLINT_DEPENDENCY_COOK_FLAG: &str = "--soldr-dylint-dependency-cook";

const INHERITED_SOLDR_WORKSPACE_ENV_VARS: &[&str] = &[
    crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
    crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
    crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR,
    crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
    crate::TARGET_CACHE_MODE_ENV_VAR,
    "SOLDR_TARGET_CACHE_DIR",
    crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR,
    crate::TARGET_CACHE_PROFILE_ENV_VAR,
    crate::TARGET_CACHE_BACKEND_ENV_VAR,
    crate::TARGET_CACHE_TAR_THREADS_ENV_VAR,
    "SOLDR_TARGET_CACHE_COMPRESS",
    "SOLDR_TARGET_CACHE_COMPRESS_LEVEL",
    "SOLDR_BUILD_CACHE_MODE",
];

struct EnvRestore {
    key: OsString,
    previous: Option<OsString>,
}

struct FreshSoldrWorkspaceEnvGuard {
    entries: Vec<EnvRestore>,
}

impl FreshSoldrWorkspaceEnvGuard {
    fn apply_unless_trusted(trust_inherited_soldr_env: bool) -> Self {
        if trust_inherited_soldr_env {
            return Self {
                entries: Vec::new(),
            };
        }

        let mut keys: Vec<OsString> = INHERITED_SOLDR_WORKSPACE_ENV_VARS
            .iter()
            .map(OsString::from)
            .collect();
        keys.extend(
            std::env::vars_os()
                .map(|(key, _)| key)
                .filter(|key| key.to_string_lossy().starts_with("SETUP_SOLDR_")),
        );
        keys.sort();
        keys.dedup();

        let mut entries = Vec::new();
        for key in keys {
            let previous = std::env::var_os(&key);
            if previous.is_some() {
                std::env::remove_var(&key);
                entries.push(EnvRestore { key, previous });
            }
        }
        Self { entries }
    }
}

impl Drop for FreshSoldrWorkspaceEnvGuard {
    fn drop(&mut self) {
        for entry in self.entries.iter().rev() {
            if let Some(value) = &entry.previous {
                std::env::set_var(&entry.key, value);
            }
        }
    }
}

/// Read a soldr-owned switch (soldr#2740).
///
/// Thin alias kept because the name is used at a dozen call sites; the rule
/// itself lives in `soldr_core::core::env_flag`. Every caller here is a
/// `SOLDR_*` / `ZCCACHE_*` variable, so they all take the *owned* allowlist:
/// an unrecognised value must never enable a `NO_*` / `*_DISABLE` switch.
/// The two foreign variables this module also reads -- `GITHUB_ACTIONS` and
/// `RUSTC_BOOTSTRAP` -- use `foreign_env_flag` instead.
pub(super) fn env_flag_truthy(key: &str) -> bool {
    crate::core::flag(key)
}

/// Read a variable defined outside soldr (soldr#2740).
///
/// `RUSTC_BOOTSTRAP`'s real convention is `1` *or a crate name*, so the
/// owned allowlist would read `RUSTC_BOOTSTRAP=serde` as unset.
pub(super) fn foreign_env_flag(key: &str) -> bool {
    crate::core::foreign_flag(key)
}

/// Issue #1364: a truthy `ZCCACHE_DISABLE` in the caller's environment is
/// treated as `--no-cache`.
///
/// `ZCCACHE_DISABLE` is the standard zccache kill-switch, but soldr never
/// consulted it, so users who set it saw no effect (the build still went
/// through the wrapper/daemon). Mapping it onto the existing `--no-cache`
/// path fully bypasses the wrapper + daemon (and propagates
/// `SOLDR_CACHE_ENABLED=0` to the child cargo), which is also the
/// recovery path when a build hangs on a wedged cache.
pub(crate) fn zccache_disable_requested() -> bool {
    env_flag_truthy("ZCCACHE_DISABLE")
}

/// Remove retired `--no-gc-target*` flags from the argument vector.
/// Flags after the `--` separator are passed through untouched.
pub(crate) fn strip_no_gc_target_flags(args: &[String]) -> Vec<String> {
    let mut cleaned = Vec::with_capacity(args.len());
    let mut past_separator = false;
    for arg in args {
        if past_separator {
            cleaned.push(arg.clone());
            continue;
        }
        if arg == "--" {
            past_separator = true;
            cleaned.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            NO_GC_TARGET_FLAG | NO_GC_TARGET_BEFORE_FLAG | NO_GC_TARGET_AFTER_FLAG => {}
            _ => cleaned.push(arg.clone()),
        }
    }
    cleaned
}

fn strip_dylint_dependency_cook_flag(args: &[String]) -> (Vec<String>, bool) {
    let mut cleaned = Vec::with_capacity(args.len());
    let mut found = false;
    let mut past_separator = false;
    for arg in args {
        if arg == "--" {
            past_separator = true;
        }
        if !past_separator && arg == DYLINT_DEPENDENCY_COOK_FLAG {
            found = true;
        } else {
            cleaned.push(arg.clone());
        }
    }
    (cleaned, found)
}

/// Resolve the Cargo `target/` directory used by front-door hooks.
/// Mirrors Cargo's resolution order:
/// 1. `--target-dir <DIR>` inside the arg list.
/// 2. `CARGO_TARGET_DIR` env var (if non-empty).
/// 3. `<workspace_root>/target` derived from the nearest enclosing
///    `Cargo.toml` to cwd.
///
/// Returns `None` when no manifest can be found cheaply so callers can
/// skip rather than guess.
fn resolve_target_dir_for_hooks(args: &[String]) -> Option<std::path::PathBuf> {
    if let Some(value) = disk::cargo_arg_value(args, "--target-dir") {
        return Some(disk::absolutize_path(std::path::PathBuf::from(value)));
    }
    if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let s = env_dir.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Some(disk::absolutize_path(std::path::PathBuf::from(s)));
        }
    }
    let manifest = crate::trampoline::find_nearest_manifest()?;
    let manifest_dir = manifest.parent()?.to_path_buf();
    Some(manifest_dir.join("target"))
}

#[cfg(test)]
fn apply_target_registry_memo(
    command: &mut std::process::Command,
    target_dir: &std::path::Path,
    paths: &SoldrPaths,
) {
    // `cargo clean` removes target/ before the next soldr invocation. The
    // future path is still authoritative and the registry accepts paths that
    // do not exist yet, so absence must not disable wrapper memoization.
    let _ = (command, target_dir, paths);
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CargoAbortCleanupReport {
    orphan_rmetas_pruned: usize,
    incremental_dirs_removed: usize,
}

impl CargoAbortCleanupReport {
    fn summary(self) -> String {
        format!(
            "pruned {} orphan .rmeta file(s), removed {} incremental/ dir(s)",
            self.orphan_rmetas_pruned, self.incremental_dirs_removed
        )
    }
}

fn cargo_run_error_is_timeout(err: &SoldrError) -> bool {
    matches!(err, SoldrError::Other(message) if message.contains(CARGO_WAIT_TIMEOUT_ENV_VAR) && message.contains("timed out after"))
}

fn cleanup_after_aborted_cargo_run(
    cache_plan: &CargoCachePlan,
    args: &[String],
    timeout: bool,
) -> CargoAbortCleanupReport {
    let orphan_rmetas_pruned = cache_plan
        .target_dir_for_hooks(args)
        .map(|target_dir| {
            orphan_rmeta::prune_orphan_rmetas_after_failed_build(&target_dir)
        })
        .unwrap_or(0);
    let incremental_dirs_removed = if timeout {
        cache_plan
            .target_dir_for_hooks(args)
            .as_deref()
            .map(cleanup_target_incremental_dirs_after_aborted_build)
            .unwrap_or(0)
    } else {
        0
    };
    if orphan_rmetas_pruned > 0 || incremental_dirs_removed > 0 {
        eprintln!(
            "soldr: cleanup after aborted cargo build: {} (soldr#1384)",
            CargoAbortCleanupReport {
                orphan_rmetas_pruned,
                incremental_dirs_removed,
            }
            .summary()
        );
    }
    CargoAbortCleanupReport {
        orphan_rmetas_pruned,
        incremental_dirs_removed,
    }
}

fn cleanup_target_incremental_dirs_after_aborted_build(target_dir: &std::path::Path) -> usize {
    let mut candidates = Vec::new();
    collect_incremental_dir_candidates(target_dir, &mut candidates);
    candidates.sort();
    candidates.dedup();

    let mut removed = 0usize;
    for path in candidates {
        if !path.is_dir() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed = removed.saturating_add(1),
            Err(err) => eprintln!(
                "soldr warning: failed to remove incremental dir {} after aborted cargo build: {err}",
                path.display()
            ),
        }
    }
    removed
}

fn collect_incremental_dir_candidates(
    target_dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) {
    let Ok(first_level) = std::fs::read_dir(target_dir) else {
        return;
    };
    for first in first_level.flatten() {
        let first_path = first.path();
        if !first_path.is_dir() {
            continue;
        }
        let direct = first_path.join(crate::cache_lib::prune_target::INCREMENTAL_SUBDIR);
        if direct.is_dir() {
            out.push(direct);
        }
        let Ok(second_level) = std::fs::read_dir(&first_path) else {
            continue;
        };
        for second in second_level.flatten() {
            let second_path = second.path();
            if !second_path.is_dir() {
                continue;
            }
            let nested = second_path.join(crate::cache_lib::prune_target::INCREMENTAL_SUBDIR);
            if nested.is_dir() {
                out.push(nested);
            }
        }
    }
}

fn augment_aborted_cargo_error(
    err: SoldrError,
    cleanup: CargoAbortCleanupReport,
    timeout: bool,
) -> SoldrError {
    let SoldrError::Other(mut message) = err else {
        return err;
    };
    message.push_str(&format!(
        "; soldr cleanup after abort: {}",
        cleanup.summary()
    ));
    if timeout {
        message.push_str(
            "; if the next build still stalls, run `ZCCACHE_DISABLE=1 soldr cargo clean -p <crate>` \
             or remove the affected target/*/incremental directory, then retry the same command \
             as `ZCCACHE_DISABLE=1 soldr cargo <same args>`; use \
             `soldr logs paths` to inspect durable logs, and lower \
             `SOLDR_CARGO_WAIT_TIMEOUT_SECS` or `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` while diagnosing",
        );
    }
    SoldrError::Other(message)
}

fn scrub_soldr_cache_lifecycle_env_for_child_cargo(command: &mut std::process::Command) {
    command.env_remove(SOLDR_CACHE_LIFECYCLE_ENV_VAR);
    command.env_remove(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR);
}

fn scrub_inherited_soldr_workspace_env_for_child_cargo(command: &mut std::process::Command) {
    for key in INHERITED_SOLDR_WORKSPACE_ENV_VARS {
        command.env_remove(key);
    }
    for key in std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("SETUP_SOLDR_"))
    {
        command.env_remove(key);
    }
}

fn maybe_apply_rustfmt_zccache_shim(
    command: &mut std::process::Command,
    args: &[String],
    cache_enabled: bool,
) -> Option<crate::shim_dir::ShimDirGuard> {
    if !cache_enabled
        || !cargo_args_should_apply_rustfmt_shim(args)
        || std::env::var_os("RUSTFMT").is_some()
    {
        return None;
    }

    match crate::shim_dir::build_shim_dir() {
        Ok(guard) => {
            command.env(
                "RUSTFMT",
                crate::shim_dir::shim_tool_path(&guard.path, "rustfmt"),
            );
            command.env(crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "soldr warning: failed to build rustfmt shim for cargo fmt; rustfmt will run without zccache format caching: {err}"
            );
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ZthreadsRetryContext {
    original_cargo_args: Vec<String>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
}

impl ZthreadsRetryContext {
    fn new(
        original_cargo_args: &[String],
        cache_enabled: bool,
        trust_inherited_soldr_env: bool,
    ) -> Self {
        Self {
            original_cargo_args: original_cargo_args.to_vec(),
            cache_enabled,
            trust_inherited_soldr_env,
        }
    }

    fn cli_args(&self) -> Vec<String> {
        let mut retry_args = Vec::with_capacity(self.original_cargo_args.len() + 3);
        if !self.cache_enabled {
            retry_args.push(String::from("--no-cache"));
        }
        if self.trust_inherited_soldr_env {
            retry_args.push(String::from("--trust-inherited-soldr-env"));
        }
        retry_args.push(String::from("cargo"));
        retry_args.extend_from_slice(&self.original_cargo_args);
        retry_args
    }
}
