use crate::cargo_front_door::profile_debug::CargoProfileDebugDefault;
use crate::core::{SoldrError, SoldrPaths};
use crate::native_cc;
use crate::rust_plan::{self, RustArtifactPlanContext};
use crate::zccache::{
    finish_zccache_build, prepare_rustc_wrapper_plan, stop_zccache_after_command, RustcWrapperPlan,
    ZccacheBuildSession,
};
use crate::ZccacheSourceArg;

pub(crate) struct CargoCachePlan {
    cache_enabled_for_cargo: bool,
    rustc_wrapper: Option<RustcWrapperPlan>,
    rust_artifact_plan: Option<RustArtifactPlanContext>,
}

impl CargoCachePlan {
    pub(crate) async fn prepare(
        cache_enabled_for_cargo: bool,
        paths: &SoldrPaths,
        zccache_source: ZccacheSourceArg,
    ) -> Result<Self, SoldrError> {
        let rustc_wrapper = if cache_enabled_for_cargo {
            Some(prepare_rustc_wrapper_plan(paths, zccache_source).await?)
        } else {
            None
        };
        Ok(Self {
            cache_enabled_for_cargo,
            rustc_wrapper,
            rust_artifact_plan: None,
        })
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
            if let Some(session) = wrapper.session() {
                native_cc::inject_native_cache_env(command, &session.binary_path, explicit_target)?;
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
        )?;
        Ok(())
    }

    pub(crate) fn target_dir_for_hooks(&self, args: &[String]) -> Option<std::path::PathBuf> {
        self.rust_artifact_plan
            .as_ref()
            .map(|plan| std::path::PathBuf::from(&plan.target_dir))
            .or_else(|| super::resolve_target_dir_for_gc(args))
    }

    pub(crate) fn restore_rust_artifacts(&self) -> Result<(), SoldrError> {
        let Some(plan) = self.rust_artifact_plan.as_ref() else {
            return Ok(());
        };
        if let Some(reason) = rust_plan::should_skip_warm_restore(plan) {
            eprintln!("{reason}");
        } else if let Some(reason) = rust_plan::should_skip_restore_due_to_prepopulated_target(plan)
        {
            // Issue #480: refuse to restore on top of a target/ that cook
            // (or a prior build) has already populated. The current zccache
            // restore path reports `restored_file_count: 0 /
            // artifact_absent_from_restored_plan: 1` and the subsequent
            // cargo build dies on missing rmetas. Skipping restore lets
            // cargo work with what's there.
            eprintln!("{reason}");
        } else {
            rust_plan::run_zccache_rust_plan(plan, "restore", false)?;
        }
        Ok(())
    }

    pub(crate) fn save_rust_artifacts(&self) -> Result<(), SoldrError> {
        if let Some(plan) = self.rust_artifact_plan.as_ref() {
            rust_plan::run_zccache_rust_plan(plan, "save", true)?;
            rust_plan::write_warm_restore_sentinel(plan);
        }
        Ok(())
    }

    pub(crate) fn prune_orphan_rmetas_after_failed_build(&self) {
        if let Some(plan) = self.rust_artifact_plan.as_ref() {
            rust_plan::prune_orphan_rmetas_after_failed_build(plan);
        }
    }

    pub(crate) fn finish_zccache_session(
        &self,
        command_lifetime_shutdown_timeout: Option<std::time::Duration>,
    ) -> Result<(), SoldrError> {
        let Some(session) = self.zccache_session() else {
            return Ok(());
        };
        let finish_result = finish_zccache_build(session);
        let shutdown_result = if let Some(timeout) = command_lifetime_shutdown_timeout {
            stop_zccache_after_command(session, timeout)
        } else {
            Ok(())
        };
        finish_result?;
        shutdown_result?;
        Ok(())
    }

    pub(crate) fn zccache_session(&self) -> Option<&ZccacheBuildSession> {
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

    fn fake_session() -> ZccacheBuildSession {
        ZccacheBuildSession {
            binary_path: std::path::PathBuf::from("/tmp/zccache"),
            cache_dir: std::path::PathBuf::from("/tmp/soldr-zccache"),
            session_id: "session-1".into(),
            session_log_path: std::path::PathBuf::from("/tmp/soldr-zccache/log"),
            journal_path: std::path::PathBuf::from("/tmp/soldr-zccache/journal"),
            session_stats_path: std::path::PathBuf::from("/tmp/soldr-zccache/stats.json"),
            private_daemon: None,
        }
    }

    fn managed_wrapper_plan() -> RustcWrapperPlan {
        RustcWrapperPlan::ManagedZccache(Box::new(ManagedZccacheWrapperPlan {
            session: fake_session(),
            child_env: ZccacheChildEnv {
                path_remap: Some("auto"),
                worktree_root: Some(std::path::PathBuf::from("/tmp/worktree")),
            },
        }))
    }

    crate::timed_test!(managed_plan_applies_session_and_path_remap_env, {
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
        assert!(command_env_override(&command, "RUSTC_WRAPPER")
            .and_then(|value| value)
            .is_some());
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_BINARY_ENV_VAR),
            Some(Some(OsString::from("/tmp/zccache")))
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR),
            Some(Some(OsString::from("session-1")))
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR),
            Some(Some(OsString::from("auto")))
        );
        assert_eq!(
            command_env_override(&command, crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR),
            Some(Some(OsString::from("/tmp/worktree")))
        );
    });

    crate::timed_test!(native_cache_opt_out_leaves_cc_and_cxx_unset, {
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
    });

    crate::timed_test!(
        custom_wrapper_plan_sets_wrapper_and_clears_managed_zccache_env,
        {
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
    );

    crate::timed_test!(
        disabled_wrapper_plan_removes_wrapper_and_managed_zccache_env,
        {
            let mut command = std::process::Command::new("cargo");
            command.env("RUSTC_WRAPPER", "old-wrapper");
            command.env(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR, "/old/zccache");
            command.env(crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR, "old-session");
            let plan = CargoCachePlan {
                cache_enabled_for_cargo: true,
                rustc_wrapper: Some(RustcWrapperPlan::Disabled),
                rust_artifact_plan: None,
            };

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
    );

    crate::timed_test!(
        non_cacheable_plan_marks_cache_disabled_without_touching_wrapper,
        {
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
    );
}
