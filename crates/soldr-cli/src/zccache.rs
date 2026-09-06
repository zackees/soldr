//! Zccache build-session orchestration and compatibility subprocess helpers.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{SoldrError, SoldrPaths};
use crate::{non_empty_env_path, RUSTC_WRAPPER_OVERRIDE_ENV_VAR};
use std::ffi::OsStr;

pub(crate) use crate::build_cache_session::BuildCacheSession;

pub(crate) const SOLDR_CACHE_LIFECYCLE_ENV_VAR: &str = "SOLDR_CACHE_LIFECYCLE";
pub(crate) const SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR: &str =
    "SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS";
pub(crate) const SOLDR_ZCCACHE_SESSION_DIR_ENV_VAR: &str = "SOLDR_ZCCACHE_SESSION_DIR";

/// Opt-in: when set to a truthy value (`1`, `true`, `yes`, `on`), route
/// per-command session artifacts, rust-plan/native-cache state, and the
/// default `soldr save`/`soldr load --cache-dir` to `<cwd>/.zccache`.
///
/// Rust compiler artifacts still live in soldr-daemon's isolated embedded
/// cache root; this compatibility flag does not relocate the daemon's live
/// artifact index. Issue #802.
pub(crate) const SOLDR_ZCCACHE_PRIVATE_ENV_VAR: &str = "SOLDR_ZCCACHE_PRIVATE";

/// Directory name used under the cwd when `SOLDR_ZCCACHE_PRIVATE` is on.
pub(crate) const PRIVATE_SESSION_CACHE_DIR_NAME: &str = ".zccache";

/// Parse `SOLDR_ZCCACHE_PRIVATE` truthiness. Truthy: `1`, `true`,
/// `yes`, `on` (case-insensitive, trimmed). Anything else (including
/// `0`, `false`, empty, unset) is falsy.
pub(crate) fn parse_private_session_flag(value: Option<&str>) -> bool {
    value.is_some_and(crate::core::flag_value)
}

/// True when `SOLDR_ZCCACHE_PRIVATE` resolves truthy in the current env.
pub(crate) fn private_session_requested() -> bool {
    parse_private_session_flag(std::env::var(SOLDR_ZCCACHE_PRIVATE_ENV_VAR).ok().as_deref())
}

/// `<cwd>/.zccache`. The well-known cache directory for private sessions.
pub(crate) fn private_session_cache_dir() -> Result<std::path::PathBuf, SoldrError> {
    Ok(std::env::current_dir()?.join(PRIVATE_SESSION_CACHE_DIR_NAME))
}

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
        Err(std::env::VarError::NotPresent) => Ok(300),
        Err(err) => Err(SoldrError::Other(format!(
            "{SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR} is not valid Unicode: {err}"
        ))),
    }
}

pub(crate) fn parse_shutdown_timeout_seconds(raw: &str) -> Result<u64, SoldrError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(300);
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
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedZccacheWrapperPlan {
    /// Parent-side build session carrier (soldr#1368). It no longer
    /// tracks an external zccache daemon — rustc compiles are cached by
    /// the soldr-daemon embedded service — but it still carries the
    /// zccache cache dir + per-build paths that the in-process rust-plan
    /// save/restore (rust_plan.rs) and native-C caching (native_cc.rs)
    /// need. `wrapper_path` is a compiler-named soldr multicall shim so the
    /// wrapper contract cannot be confused with a different multicall verb.
    pub(crate) session: BuildCacheSession,
    pub(crate) child_env: ZccacheChildEnv,
    pub(crate) wrapper_path: std::path::PathBuf,
    /// Canonically named daemon image inherited by compiler shims. A wrapper's
    /// own executable is named `rustc`/`clang`; launching that image directly
    /// would make the PID safety gate correctly reject the daemon.
    pub(crate) daemon_path: std::path::PathBuf,
    /// Route registered by the front door for the broker. Compiler shims carry
    /// only this opaque identity; they never place or spawn the daemon.
    pub(crate) broker_service_name: String,
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
    pub(crate) fn is_managed_zccache(&self) -> bool {
        matches!(self, Self::ManagedZccache(_))
    }

    pub(crate) fn session(&self) -> Option<&BuildCacheSession> {
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
                // soldr#1368: point cargo's RUSTC_WRAPPER at soldr so
                // rustc invocations route to the daemon's embedded
                // zccache service. There is no externally-resolved
                // zccache binary or managed session to plumb, so clear
                // the legacy session env and only seed the parent-cache
                // path-remap vars.
                crate::wrapper_identity::set_owned_rustc_wrapper(
                    cargo,
                    plan.wrapper_path.as_os_str(),
                    crate::wrapper_identity::WrapperOrigin::SoldrManaged,
                );
                cargo.env(
                    crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
                    &plan.broker_service_name,
                );
                cargo.env_remove(crate::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR);
                remove_managed_zccache_env(cargo);
                cargo.env_remove(SOLDR_ZCCACHE_SESSION_DIR_ENV_VAR);
                plan.child_env.apply_to_command(cargo);
            }
            Self::Custom {
                wrapper,
                sccache_dir,
            } => {
                if let Some(sccache_dir) = sccache_dir {
                    cargo.env("SCCACHE_DIR", sccache_dir);
                }
                crate::wrapper_identity::set_owned_rustc_wrapper(
                    cargo,
                    wrapper,
                    crate::wrapper_identity::WrapperOrigin::CustomOverride,
                );
                remove_managed_zccache_env(cargo);
            }
            Self::Disabled => {
                crate::wrapper_identity::remove_owned_rustc_wrapper(cargo);
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

pub(crate) async fn prepare_rustc_wrapper_plan(
    paths: &SoldrPaths,
) -> Result<RustcWrapperPlan, SoldrError> {
    match rustc_wrapper_mode() {
        RustcWrapperMode::ManagedZccache => prepare_zccache_build(paths)
            .await
            .map(|plan| RustcWrapperPlan::ManagedZccache(Box::new(plan))),
        RustcWrapperMode::Custom(wrapper) => {
            // `SOLDR_RUSTC_WRAPPER` is also the supported way for source CI
            // to pin compiler re-entries to the exact Soldr binary under
            // test. Treat that one identity as the managed embedded-cache
            // path, not as an opaque third-party wrapper. Otherwise we omit
            // the broker service registration and child route export that a
            // Soldr wrapper requires. An in-place rebuild then changes the
            // sibling `soldr-daemon` image hash, the wrapper requests that new
            // route, and the stable broker correctly refuses it because no
            // matching service definition was written.
            //
            // File identity is intentional: a basename check would mistake
            // an unrelated executable named `soldr` for this process, while
            // canonical path equality would miss hardlinked Soldr shims.
            if custom_wrapper_is_current_soldr(&wrapper) {
                let mut plan = prepare_zccache_build(paths).await?;
                plan.wrapper_path = std::path::PathBuf::from(wrapper);
                return Ok(RustcWrapperPlan::ManagedZccache(Box::new(plan)));
            }
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

fn custom_wrapper_is_current_soldr(wrapper: &std::ffi::OsStr) -> bool {
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    crate::platform::fs::identity::same_file(std::path::Path::new(wrapper), &current)
}

pub(crate) fn is_sccache_wrapper(wrapper: &std::ffi::OsStr) -> bool {
    std::path::Path::new(wrapper)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"))
}

async fn prepare_zccache_build(
    paths: &SoldrPaths,
) -> Result<ManagedZccacheWrapperPlan, SoldrError> {
    // soldr#1368: rustc compiles are cached by the soldr-daemon's
    // embedded zccache service over the RUSTC_WRAPPER=soldr IPC path
    // (see `wrapper.rs` + `compile_dispatch.rs`). The front door no
    // longer downloads a managed zccache binary or spawns a separate
    // zccache daemon. It still builds a lightweight build *session*
    // carrier so the in-process rust-plan save/restore and native-C
    // caching keep working against the shared zccache cache dir.
    let zccache_base_dir = managed_zccache_cache_dir(paths)?;
    let child_env = ZccacheChildEnv::from_current_process()?;

    // Session/auxiliary cache-dir resolution mirrors the pre-#1368 logic: an
    // explicit ZCCACHE_CACHE_DIR wins; otherwise `SOLDR_ZCCACHE_PRIVATE`
    // selects `<cwd>/.zccache`; otherwise use the shared Soldr-owned zccache
    // directory. The embedded daemon's Rust artifact root is configured
    // independently in `zccache_embedded.rs`.
    let inherited_soldr_managed_dir =
        non_empty_env_path(crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?;
    let explicit_zccache_cache_dir =
        non_empty_env_path(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?
            .filter(|path| inherited_soldr_managed_dir.as_ref() != Some(path));
    let private_override = if explicit_zccache_cache_dir.is_none() && private_session_requested() {
        Some(normalize_path_for_compare(&private_session_cache_dir()?)?)
    } else {
        None
    };
    let zccache_dir = explicit_zccache_cache_dir
        .or(private_override)
        .unwrap_or(zccache_base_dir);
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;

    let session = BuildCacheSession {
        // Retained for struct compatibility only — nothing spawns it now.
        cache_dir: zccache_dir.clone(),
        session_id: synthetic_build_session_id(),
        session_log_path: crate::cache_lib::session_log_path(&zccache_dir),
        journal_path: crate::cache_lib::session_journal_path(&zccache_dir),
        session_stats_path: crate::cache_lib::session_stats_path(&zccache_dir),
    };

    let wrapper_path = crate::binaries::rustc_wrapper_shim_binary(paths)?;
    let (daemon_path, broker_service_name) = register_broker_daemon_service()?;
    Ok(ManagedZccacheWrapperPlan {
        session,
        child_env,
        wrapper_path,
        daemon_path,
        broker_service_name,
    })
}

/// Materialize + register the soldr-daemon image with the broker and return
/// `(daemon_path, service_name)`.
///
/// soldr#2451: shared by [`prepare_rustc_wrapper_plan`] and the
/// caller-provided-`RUSTC_WRAPPER` maturin path. Any build front door that
/// hands a child `RUSTC_WRAPPER=soldr` must also hand it `SOLDR_BROKER_SERVICE`,
/// because the cargo wrapper re-entries resolve the broker route by that name.
/// Without it they fall back to hashing a sibling `soldr-daemon` next to the
/// wrapper — which does not exist in a wheel install — and fail as
/// "cannot resolve the broker daemon route (os error 2)".
pub(crate) fn register_broker_daemon_service() -> Result<(std::path::PathBuf, String), SoldrError> {
    let daemon_path = crate::binaries::soldr_daemon_binary()?;
    let installed = crate::daemon::service_definition::install_service_definition(&daemon_path)
        .map_err(|err| {
            SoldrError::Other(format!(
                "failed to register soldr-daemon image {} with the broker: {err}",
                daemon_path.display()
            ))
        })?;
    if let Ok(paths) = crate::core::SoldrPaths::new() {
        crate::daemon::lifecycle::preflight_displace_stale_daemon_for_service(
            &paths,
            Some(installed.definition.service_name.as_ref()),
        );
    }
    Ok((daemon_path, installed.definition.service_name))
}

/// Generate a short, unique-enough id for a front-door build session
/// (soldr#1368). It is no longer a real zccache daemon session id — it's
/// a cosmetic correlation handle — so a blake3 of (pid, monotonic nanos)
/// is plenty and avoids a `uuid` dependency.
fn synthetic_build_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = zccache::hash::StreamHasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..12])
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
    fn current_soldr_override_is_recognized_as_the_embedded_cache_front_door() {
        let current = std::env::current_exe().expect("current test executable");
        assert!(custom_wrapper_is_current_soldr(current.as_os_str()));
    }

    #[test]
    fn unrelated_custom_wrapper_is_not_recognized_as_soldr() {
        let wrapper = unique_test_dir("custom-wrapper").join("soldr");
        std::fs::write(&wrapper, b"not the running executable").expect("fake custom wrapper");
        assert!(!custom_wrapper_is_current_soldr(wrapper.as_os_str()));
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

    // ---------------------------------------------------------------
    // Private-session opt-in (`SOLDR_ZCCACHE_PRIVATE`). Routes session-local
    // auxiliary state to `<cwd>/.zccache`; it does not relocate the embedded
    // daemon's Rust artifact store.
    // ---------------------------------------------------------------

    #[test]
    fn private_session_flag_truthy_values() {
        for v in [
            "1", "true", "yes", "on", "TRUE", "Yes", "ON", " 1 ", " true ",
        ] {
            assert!(
                parse_private_session_flag(Some(v)),
                "expected {v:?} to parse truthy",
            );
        }
    }

    #[test]
    fn private_session_flag_falsy_values() {
        for v in [
            "0", "false", "no", "off", "FALSE", "No", "OFF", "", "   ", "maybe", "2",
        ] {
            assert!(
                !parse_private_session_flag(Some(v)),
                "expected {v:?} to parse falsy",
            );
        }
        assert!(
            !parse_private_session_flag(None),
            "unset env should be falsy",
        );
    }

    #[test]
    fn private_session_cache_dir_is_dot_zccache_under_cwd() {
        let cwd = std::env::current_dir().expect("cwd");
        let resolved = private_session_cache_dir().expect("private dir");
        assert_eq!(resolved, cwd.join(PRIVATE_SESSION_CACHE_DIR_NAME));
        assert!(
            resolved.is_absolute(),
            "private session cache dir must be absolute: {}",
            resolved.display(),
        );
        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some(".zccache"),
            "private session cache dir tail must be `.zccache`",
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
        assert_eq!(parse_shutdown_timeout_seconds("").unwrap(), 300);
        assert_eq!(parse_shutdown_timeout_seconds(" 5 ").unwrap(), 5);
        assert!(parse_shutdown_timeout_seconds("0").is_err());
        assert!(parse_shutdown_timeout_seconds("abc").is_err());
    }
}
