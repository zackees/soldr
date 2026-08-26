use crate::build_cache_session::BuildCacheSession;
use crate::cargo_front_door::profile_debug::CargoProfileDebugDefault;
use crate::core::{SoldrError, SoldrPaths};
use crate::native_cc;
use crate::rust_plan::{self, RustArtifactPlanContext, RustPlanRestoreOutcome};
use crate::zccache::{prepare_rustc_wrapper_plan, RustcWrapperPlan};

pub(crate) struct CargoCachePlan {
    cache_enabled_for_cargo: bool,
    rustc_wrapper: Option<RustcWrapperPlan>,
    rust_artifact_plan: Option<RustArtifactPlanContext>,
}

/// Handle to an in-flight zccache wrapper-plan prefetch, kicked off at
/// the earliest decision point in the cargo front door so the binary
/// fetch + extract + redb init overlaps cargo's own startup (manifest
/// parse, dep-graph resolution).
///
/// L3 optimization from soldr#980: on cold builds where the managed
/// zccache binary is not yet on disk, [`prepare_rustc_wrapper_plan`]
/// can spend ~14 s pulling the GitHub release and initializing the
/// daemon. None of that work depends on the cargo manifest, so we
/// background it the moment we know a build is going to spawn.
/// The `.await` happens just before the wrapper env is injected,
/// which is also just before cargo is spawned.
///
/// Warm path (binary already cached): the inner future resolves
/// effectively immediately, so the join is free.
pub(crate) enum CargoCachePlanPrefetch {
    /// Caching disabled — no fetch was scheduled.
    Disabled,
    /// Caching enabled — a background task is resolving the wrapper plan.
    Pending(tokio::task::JoinHandle<Result<RustcWrapperPlan, SoldrError>>),
}

impl CargoCachePlanPrefetch {
    /// Kick off zccache wrapper-plan resolution on a background tokio
    /// task. Call this at the earliest point in the front door where we
    /// know a child cargo will be spawned. The returned handle is
    /// awaited by [`CargoCachePlan::finalize`] just before the wrapper
    /// env is injected onto the cargo command.
    pub(crate) fn start(cache_enabled_for_cargo: bool, paths: &SoldrPaths) -> Self {
        if !cache_enabled_for_cargo {
            return Self::Disabled;
        }
        // soldr#2188: once per build, before any crate compiles. Recorded
        // rather than printed so it replays at a failure too -- the moment it
        // is most needed is when LNK1104 has already scrolled past.
        if let Some(warning) = crate::compile_diagnostics::maxpath_headroom_warning(&paths.cache) {
            eprintln!("{warning}");
            soldr_core::warning_log::record(warning);
        }
        let paths = paths.clone();
        let handle = tokio::spawn(async move { prepare_rustc_wrapper_plan(&paths).await });
        Self::Pending(handle)
    }
}

impl CargoCachePlan {
    /// soldr#2545: the effective wrapper identity this plan applies, for the
    /// build log. `None` when no plan was prepared (cache disabled before
    /// planning); `Some` with `effective: None` when the plan explicitly
    /// cleared the wrapper.
    pub(crate) fn wrapper_identity(&self) -> Option<crate::build_log::WrapperIdentity> {
        use crate::wrapper_identity::WrapperOrigin;
        let plan = self.rustc_wrapper.as_ref()?;
        Some(match plan {
            RustcWrapperPlan::ManagedZccache(managed) => crate::build_log::WrapperIdentity {
                effective: Some(managed.wrapper_path.clone()),
                origin: WrapperOrigin::SoldrManaged.as_str(),
            },
            RustcWrapperPlan::Custom { wrapper, .. } => crate::build_log::WrapperIdentity {
                effective: Some(std::path::PathBuf::from(wrapper)),
                origin: WrapperOrigin::CustomOverride.as_str(),
            },
            RustcWrapperPlan::Disabled => crate::build_log::WrapperIdentity {
                effective: None,
                origin: WrapperOrigin::Disabled.as_str(),
            },
        })
    }

    pub(crate) fn uses_managed_zccache(&self) -> bool {
        self.rustc_wrapper
            .as_ref()
            .is_some_and(RustcWrapperPlan::is_managed_zccache)
    }

    /// Finalize the cache plan by awaiting the background prefetch
    /// kicked off by [`CargoCachePlanPrefetch::start`]. Call this
    /// immediately before [`CargoCachePlan::apply_to_command`] — i.e.
    /// as late in the front-door pipeline as possible — to give the
    /// background fetch maximum overlap with the synchronous setup
    /// work that does not depend on the resolved zccache binary.
    pub(crate) async fn finalize(
        cache_enabled_for_cargo: bool,
        prefetch: CargoCachePlanPrefetch,
    ) -> Result<Self, SoldrError> {
        let rustc_wrapper = match prefetch {
            CargoCachePlanPrefetch::Disabled => None,
            CargoCachePlanPrefetch::Pending(handle) => match handle.await {
                Ok(result) => Some(result?),
                Err(join_err) => {
                    if join_err.is_panic() {
                        std::panic::resume_unwind(join_err.into_panic());
                    }
                    return Err(SoldrError::Other(format!(
                        "zccache prefetch task was cancelled before completion: {join_err}"
                    )));
                }
            },
        };
        Ok(Self {
            cache_enabled_for_cargo,
            rustc_wrapper,
            rust_artifact_plan: None,
        })
    }

    /// Test-only constructor that bypasses the background prefetch.
    /// Production callers should go through
    /// [`CargoCachePlanPrefetch::start`] +
    /// [`CargoCachePlan::finalize`].
    #[cfg(test)]
    pub(crate) async fn prepare(
        cache_enabled_for_cargo: bool,
        paths: &SoldrPaths,
    ) -> Result<Self, SoldrError> {
        let rustc_wrapper = if cache_enabled_for_cargo {
            Some(prepare_rustc_wrapper_plan(paths).await?)
        } else {
            None
        };
        Ok(Self {
            cache_enabled_for_cargo,
            rustc_wrapper,
            rust_artifact_plan: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_rust_artifact_plan(plan: RustArtifactPlanContext) -> Self {
        Self {
            cache_enabled_for_cargo: true,
            rustc_wrapper: None,
            rust_artifact_plan: Some(plan),
        }
    }

    pub(crate) fn apply_to_command(
        &self,
        command: &mut std::process::Command,
        explicit_target: Option<&str>,
    ) -> Result<(), SoldrError> {
        command.env(
            crate::cache_lib::CACHE_ENABLED_ENV_VAR,
            crate::cache_lib::cache_enabled_env_value(self.cache_enabled_for_cargo),
        );

        if let Some(wrapper) = self.rustc_wrapper.as_ref() {
            wrapper.apply_to_command(command)?;
            if wrapper.session().is_some() {
                // soldr#1368: native C/C++ caching wraps CC/CXX with the
                // `zccache-soldr` shim (not the removed managed binary),
                // so build-script compiles route to the soldr-daemon
                // embedded zccache service over the Compile IPC verb.
                let shim = crate::binaries::zccache_soldr_shim_binary()?;
                native_cc::inject_native_cache_env(command, &shim, explicit_target)?;
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_rust_artifact_plan(
        &mut self,
        cargo: &std::path::Path,
        rustc: &std::path::Path,
        args: &[String],
        cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
        toolchain_channel_override: Option<&str>,
    ) -> Result<(), SoldrError> {
        let Some(session) = self.zccache_session() else {
            return Ok(());
        };
        self.rust_artifact_plan = rust_plan::maybe_prepare_rust_artifact_plan(
            cargo,
            rustc,
            args,
            session,
            cargo_profile_debug_default,
            toolchain_channel_override,
        )?;
        Ok(())
    }

    pub(crate) fn target_dir_for_hooks(&self, args: &[String]) -> Option<std::path::PathBuf> {
        self.rust_artifact_plan
            .as_ref()
            .map(|plan| std::path::PathBuf::from(&plan.target_dir))
            .or_else(|| super::resolve_target_dir_for_hooks(args))
    }

    pub(crate) fn restore_rust_artifacts(&self) -> Result<RustPlanRestoreOutcome, SoldrError> {
        let Some(plan) = self.rust_artifact_plan.as_ref() else {
            return Ok(RustPlanRestoreOutcome::NotAttempted);
        };
        if let Some(reason) = rust_plan::should_skip_warm_restore(plan) {
            eprintln!("{reason}");
            Ok(RustPlanRestoreOutcome::Skipped)
        } else {
            let summary = rust_plan::run_zccache_rust_plan(plan, "restore", false)?;
            Ok(RustPlanRestoreOutcome::Restored {
                restored_file_count: summary.restored_file_count,
            })
        }
    }

    /// `restore_outcome` is what [`Self::restore_rust_artifacts`] returned
    /// earlier this invocation (issue #1538): when it was
    /// [`RustPlanRestoreOutcome::Skipped`] and this build's zccache session
    /// recorded zero rustc-wrapper invocations, `target/` provably holds
    /// exactly what the last successful save already wrote, so the save
    /// (and its target walk/copy/rehash) is skipped entirely. The
    /// warm-restore sentinel is still refreshed unconditionally — that's a
    /// cheap two-small-file write, not a target walk — so the skip window
    /// keeps sliding forward instead of going stale.
    pub(crate) fn save_rust_artifacts(
        &self,
        restore_outcome: RustPlanRestoreOutcome,
    ) -> Result<(), SoldrError> {
        if let Some(plan) = self.rust_artifact_plan.as_ref() {
            let compilations_this_build = self.zccache_session().and_then(|session| {
                crate::cache::compilations_since_baseline(&session.cache_dir, &session.session_id)
            });
            if let Some(reason) = rust_plan::should_skip_rust_plan_save(
                plan,
                restore_outcome,
                compilations_this_build,
            ) {
                eprintln!("{reason}");
            } else {
                rust_plan::run_zccache_rust_plan(plan, "save", true)?;
            }
            rust_plan::write_warm_restore_sentinel(plan);
        }
        Ok(())
    }

    pub(crate) fn record_cargo_artifact_closure(
        &self,
        paths: &[String],
        complete: bool,
    ) -> Result<(), SoldrError> {
        if let Some(plan) = self.rust_artifact_plan.as_ref() {
            rust_plan::record_cargo_artifact_closure(&plan.path, paths, complete)?;
        }
        Ok(())
    }

    pub(crate) fn has_rust_artifact_plan(&self) -> bool {
        self.rust_artifact_plan.is_some()
    }

    pub(crate) fn prune_orphan_rmetas_after_failed_build(&self) -> usize {
        self.rust_artifact_plan
            .as_ref()
            .map(rust_plan::prune_orphan_rmetas_after_failed_build)
            .unwrap_or(0)
    }

    pub(crate) fn finish_zccache_session(
        &self,
        command_lifetime_shutdown_timeout: Option<std::time::Duration>,
    ) -> Result<(), SoldrError> {
        if self.zccache_session().is_none() {
            return Ok(());
        }
        // soldr#1368: rustc compiles are cached by the soldr-daemon
        // embedded service — there is no external managed zccache session
        // to end or daemon to stop. For command-lifetime mode (the
        // setup-soldr "flush before archiving" contract, soldr#383), ask
        // the embedded daemon to make its state durable on disk.
        if command_lifetime_shutdown_timeout.is_some() {
            let paths = SoldrPaths::new()?;
            let sock = crate::daemon::server::server_sock_path(&paths);
            match crate::daemon::client::flush_caches(&sock) {
                Ok(report) if report.is_complete() => {}
                Ok(report) => {
                    return Err(SoldrError::Other(format!(
                        "embedded zccache checkpoint incomplete: {}",
                        report.incomplete_reason()
                    )));
                }
                // The daemon may be gone because code run by this command
                // stopped it. Final checkpointing is best-effort in that
                // already-unavailable state.
                Err(crate::daemon::client::ClientError::NotRunning) => {}
                Err(err) => {
                    return Err(SoldrError::Other(format!(
                        "embedded zccache checkpoint unavailable: {err:?}"
                    )));
                }
            }
        }
        // soldr#1368 observability restore: diff the embedded zccache
        // compile counters against the build-start baseline and write the
        // per-build hit/miss summary to `last-session-stats.json` so
        // `soldr cache report` (and the perf harness) surface the hit rate
        // again. The pre-#1368 managed `zccache session-end` path used to
        // produce this artifact; the embedded service does not, so soldr
        // writes it from the daemon's live `CompileStats`.
        if let Some(session) = self.zccache_session() {
            crate::cache::finalize_build_session_stats(&session.cache_dir, &session.session_id);
        }
        Ok(())
    }

    pub(crate) fn zccache_session(&self) -> Option<&BuildCacheSession> {
        self.rustc_wrapper
            .as_ref()
            .and_then(RustcWrapperPlan::session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zccache::{ManagedZccacheWrapperPlan, ZccacheChildEnv};
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn command_env_override(
        command: &std::process::Command,
        key: &'static str,
    ) -> Option<Option<OsString>> {
        command
            .get_envs()
            .find(|(candidate, _)| *candidate == OsStr::new(key))
            .map(|(_, value)| value.map(OsString::from))
    }

    fn fake_session() -> BuildCacheSession {
        BuildCacheSession {
            cache_dir: std::path::PathBuf::from("/tmp/soldr-zccache"),
            session_id: "session-1".into(),
            session_log_path: std::path::PathBuf::from("/tmp/soldr-zccache/log"),
            journal_path: std::path::PathBuf::from("/tmp/soldr-zccache/journal"),
            session_stats_path: std::path::PathBuf::from("/tmp/soldr-zccache/stats.json"),
        }
    }

    fn managed_wrapper_plan() -> RustcWrapperPlan {
        RustcWrapperPlan::ManagedZccache(Box::new(ManagedZccacheWrapperPlan {
            session: fake_session(),
            child_env: ZccacheChildEnv {
                path_remap: Some("auto"),
                worktree_root: Some(std::path::PathBuf::from("/tmp/worktree")),
            },
            wrapper_path: std::path::PathBuf::from("/tmp/soldr-shims/rustc"),
            daemon_path: std::path::PathBuf::from("/tmp/soldr-daemon"),
            broker_service_name: "soldr-daemon-test-route".to_string(),
        }))
    }

    #[test]
    fn managed_plan_applies_session_and_path_remap_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _native = EnvGuard::set(native_cc::NATIVE_CACHE_ENV_VAR, "0");
        let mut command = std::process::Command::new("cargo");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: true,
            rustc_wrapper: Some(managed_wrapper_plan()),
            rust_artifact_plan: None,
        };

        plan.apply_to_command(&mut command, Some("x86_64-unknown-linux-gnu"))
            .expect("apply cache plan");

        assert_eq!(
            command_env_override(&command, crate::cache_lib::CACHE_ENABLED_ENV_VAR),
            Some(Some(OsString::from(
                crate::cache_lib::cache_enabled_env_value(true)
            )))
        );
        assert_eq!(
            command_env_override(&command, "RUSTC_WRAPPER"),
            Some(Some(OsString::from("/tmp/soldr-shims/rustc")))
        );
        assert_eq!(
            command_env_override(
                &command,
                crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR
            ),
            Some(Some(OsString::from("soldr-daemon-test-route")))
        );
        assert_eq!(
            command_env_override(&command, crate::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR),
            Some(None),
            "compiler children receive only a broker route, never a daemon spawn image"
        );
        // soldr#1368: the front door no longer plumbs an external zccache
        // binary or managed session — those env vars are cleared, not set.
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_BINARY_ENV_VAR),
            Some(None)
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR),
            Some(None)
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR),
            Some(Some(OsString::from("auto")))
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR),
            Some(Some(OsString::from("/tmp/worktree")))
        );
    }

    #[test]
    fn native_cache_opt_out_leaves_cc_and_cxx_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _native = EnvGuard::set(native_cc::NATIVE_CACHE_ENV_VAR, "0");
        let _cc = EnvGuard::remove("CC");
        let _cxx = EnvGuard::remove("CXX");
        let mut command = std::process::Command::new("cargo");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: true,
            rustc_wrapper: Some(managed_wrapper_plan()),
            rust_artifact_plan: None,
        };

        plan.apply_to_command(&mut command, Some("x86_64-unknown-linux-gnu"))
            .expect("apply cache plan");

        assert_eq!(command_env_override(&command, "CC"), None);
        assert_eq!(command_env_override(&command, "CXX"), None);
    }

    #[test]
    fn native_cache_opt_out_keeps_windows_gnu_cross_compiler_unwrapped() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _native = EnvGuard::set(native_cc::NATIVE_CACHE_ENV_VAR, "0");
        let _cc = EnvGuard::set("CC_x86_64_pc_windows_gnu", "C:\\soldr\\mingw\\bin\\gcc.exe");
        let _cxx = EnvGuard::set(
            "CXX_x86_64_pc_windows_gnu",
            "C:\\soldr\\mingw\\bin\\g++.exe",
        );
        let mut command = std::process::Command::new("cargo");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: true,
            rustc_wrapper: Some(managed_wrapper_plan()),
            rust_artifact_plan: None,
        };

        plan.apply_to_command(&mut command, Some("x86_64-pc-windows-gnu"))
            .expect("apply cache plan");

        assert_eq!(
            command_env_override(&command, crate::cache_lib::CACHE_ENABLED_ENV_VAR),
            Some(Some(OsString::from(
                crate::cache_lib::cache_enabled_env_value(true)
            )))
        );
        assert!(command_env_override(&command, "RUSTC_WRAPPER")
            .and_then(|value| value)
            .is_some());
        assert_eq!(
            command_env_override(&command, "CC_x86_64_pc_windows_gnu"),
            None,
            "SOLDR_NATIVE_CACHE=0 must not wrap the managed MinGW C compiler"
        );
        assert_eq!(
            command_env_override(&command, "CXX_x86_64_pc_windows_gnu"),
            None,
            "SOLDR_NATIVE_CACHE=0 must not wrap the managed MinGW C++ compiler"
        );
    }

    #[test]
    fn custom_wrapper_plan_sets_wrapper_and_clears_managed_zccache_env() {
        let mut command = std::process::Command::new("cargo");
        command.env(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR, "/old/zccache");
        command.env(
            crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
            "/old/cache",
        );
        command.env(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR, "old-session");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: true,
            rustc_wrapper: Some(RustcWrapperPlan::Custom {
                wrapper: OsString::from("sccache"),
                sccache_dir: Some(std::path::PathBuf::from("/tmp/sccache")),
            }),
            rust_artifact_plan: None,
        };
        assert!(!plan.uses_managed_zccache());

        plan.apply_to_command(&mut command, None)
            .expect("apply cache plan");

        assert_eq!(
            command_env_override(&command, "RUSTC_WRAPPER"),
            Some(Some(OsString::from("sccache")))
        );
        assert_eq!(
            command_env_override(&command, "SCCACHE_DIR"),
            Some(Some(OsString::from("/tmp/sccache")))
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_BINARY_ENV_VAR),
            Some(None)
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR),
            Some(None)
        );
    }

    #[test]
    fn disabled_wrapper_plan_removes_wrapper_and_managed_zccache_env() {
        let mut command = std::process::Command::new("cargo");
        command.env("RUSTC_WRAPPER", "old-wrapper");
        command.env(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR, "/old/zccache");
        command.env(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR, "old-session");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: true,
            rustc_wrapper: Some(RustcWrapperPlan::Disabled),
            rust_artifact_plan: None,
        };
        assert!(!plan.uses_managed_zccache());

        plan.apply_to_command(&mut command, None)
            .expect("apply cache plan");

        assert_eq!(command_env_override(&command, "RUSTC_WRAPPER"), Some(None));
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_BINARY_ENV_VAR),
            Some(None)
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR),
            Some(None)
        );
    }

    #[test]
    fn disabled_wrapper_plan_keeps_windows_gnu_cross_compiler_unwrapped() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _cc = EnvGuard::set("CC_x86_64_pc_windows_gnu", "C:\\soldr\\mingw\\bin\\gcc.exe");
        let _cxx = EnvGuard::set(
            "CXX_x86_64_pc_windows_gnu",
            "C:\\soldr\\mingw\\bin\\g++.exe",
        );
        let mut command = std::process::Command::new("cargo");
        command.env("RUSTC_WRAPPER", "old-wrapper");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: true,
            rustc_wrapper: Some(RustcWrapperPlan::Disabled),
            rust_artifact_plan: None,
        };

        plan.apply_to_command(&mut command, Some("x86_64-pc-windows-gnu"))
            .expect("apply cache plan");

        assert_eq!(command_env_override(&command, "RUSTC_WRAPPER"), Some(None));
        assert_eq!(
            command_env_override(&command, "CC_x86_64_pc_windows_gnu"),
            None,
            "disabled rustc wrapper plan must not wrap the managed MinGW C compiler"
        );
        assert_eq!(
            command_env_override(&command, "CXX_x86_64_pc_windows_gnu"),
            None,
            "disabled rustc wrapper plan must not wrap the managed MinGW C++ compiler"
        );
    }

    #[test]
    fn non_cacheable_plan_marks_cache_disabled_without_touching_wrapper() {
        let mut command = std::process::Command::new("cargo");
        command.env("RUSTC_WRAPPER", "inherited-wrapper");
        let plan = CargoCachePlan {
            cache_enabled_for_cargo: false,
            rustc_wrapper: None,
            rust_artifact_plan: None,
        };

        plan.apply_to_command(&mut command, None)
            .expect("apply cache plan");

        assert_eq!(
            command_env_override(&command, crate::cache_lib::CACHE_ENABLED_ENV_VAR),
            Some(Some(OsString::from(
                crate::cache_lib::cache_enabled_env_value(false)
            )))
        );
        assert_eq!(
            command_env_override(&command, "RUSTC_WRAPPER"),
            Some(Some(OsString::from("inherited-wrapper")))
        );
    }
}
