//! Regression tests for issue #1538: skip the post-build `rust-plan save`
//! entirely when the just-finished cargo invocation provably wrote nothing
//! new into `target/`.
//!
//! [`should_skip_rust_plan_save`] is deliberately NOT a mtime/size check —
//! see its doc comment. These tests exercise the two load-bearing gates
//! directly: (a) the happy path where a truly no-op rebuild skips the
//! save, and (b) the "degenerate mtime" correctness gate, where content
//! could have changed (modeled here as a non-zero compile count — the only
//! way bytes land under `target/` in this architecture) even though a
//! naive stat-based check might have looked "unchanged". The skip
//! mechanism must never fire in that case.

use super::warm_restore::ENV_LOCK;
use crate::rust_plan::{
    compute_plan_inputs_hash, should_skip_rust_plan_save, RustArtifactPlan,
    RustArtifactPlanContext, RustPlanInputs, RustPlanPackages, RustPlanRestoreOutcome,
    RustToolchainIdentity,
};
use crate::SKIP_WARM_RESTORE_ENV_VAR;
use std::ffi::{OsStr, OsString};

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn save_skip_test_plan() -> RustArtifactPlan {
    RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v2"),
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0-test".to_string(),
            cargo: "cargo 1.0.0-test".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "test".to_string(),
        inputs: RustPlanInputs {
            features_hash: "F".to_string(),
            rustflags_hash: "R".to_string(),
            env_hash: "E".to_string(),
            lockfile_hash: "L".to_string(),
            cargo_config_hash: "C".to_string(),
            manifest_hashes: vec!["M1".to_string()],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["serde@1.0.0".to_string()],
            workspace_package_ids: vec!["app@0.1.0".to_string()],
            excluded_path_package_ids: vec![],
            ownership_policy: None,
            ownership_mode: None,
            artifact_owners: Vec::new(),
            ownership_complete: false,
        },
        allowed_artifact_classes: vec!["rlib", "rmeta"],
        dropped_artifact_classes: vec![],
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
        cache_schema_version: 1,
        journal_log_path: None,
    }
}

fn save_skip_test_context(plan: &RustArtifactPlan) -> RustArtifactPlanContext {
    RustArtifactPlanContext {
        path: std::path::PathBuf::from("/tmp/ws/plan.json"),
        cache_dir: std::path::PathBuf::from("/tmp/ws/cache"),
        session_id: "session-test".to_string(),
        journal_path: std::path::PathBuf::from("/tmp/ws/journal"),
        backend: "fs".to_string(),
        cache_profile: Some("thin-v2"),
        plan_inputs_hash: compute_plan_inputs_hash(plan),
        target_dir: plan.target_dir.clone(),
    }
}

// (a) The true no-op rebuild: restore was skipped this invocation (target
// already proven to hold the exact bytes the last save produced) and the
// wrapper recorded zero compiles. There is nothing new to save — skip.
#[test]
fn skip_fires_when_restore_skipped_and_zero_compilations() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");

    let plan = save_skip_test_plan();
    let ctx = save_skip_test_context(&plan);

    let result = should_skip_rust_plan_save(&ctx, RustPlanRestoreOutcome::Skipped, Some(0));
    let reason = result.expect("expected Some(reason): nothing changed, save should be skipped");
    assert!(!reason.is_empty(), "skip reason must be operator-visible");
}

// (b) The "degenerate mtime" correctness gate: even though restore was
// skipped (target looked untouched by the coarse generation-marker
// proof), a real compile happened this build (`compilations_this_build =
// Some(1)`) — the only way new bytes can land under `target/`. This
// models "content changed under the same mtime/size" at the causal
// level: whatever changed, it went through a wrapper invocation, so the
// skip must NOT fire and `rust-plan save` must run normally.
#[test]
fn skip_does_not_fire_when_any_compilation_happened() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");

    let plan = save_skip_test_plan();
    let ctx = save_skip_test_context(&plan);

    let result = should_skip_rust_plan_save(&ctx, RustPlanRestoreOutcome::Skipped, Some(1));
    assert!(
        result.is_none(),
        "a nonzero compile count must force a real save even though restore was skipped"
    );
}

// An unreachable daemon (no baseline/current compile-stats snapshot)
// means we cannot prove zero writes happened. Treat as unproven, never
// skip — the safe default when the authoritative signal is missing.
#[test]
fn skip_does_not_fire_when_compilation_count_is_unproven() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");

    let plan = save_skip_test_plan();
    let ctx = save_skip_test_context(&plan);

    let result = should_skip_rust_plan_save(&ctx, RustPlanRestoreOutcome::Skipped, None);
    assert!(
        result.is_none(),
        "an unproven compile count (daemon unreachable) must never skip the save"
    );
}

// If restore actually ran (or was never attempted), we have no proof
// `target/` already matched a prior save's generation, so the save must
// always proceed regardless of the compile count.
#[test]
fn skip_does_not_fire_when_restore_was_not_skipped() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");

    let plan = save_skip_test_plan();
    let ctx = save_skip_test_context(&plan);

    let restored = RustPlanRestoreOutcome::Restored {
        restored_file_count: 42,
    };
    assert!(should_skip_rust_plan_save(&ctx, restored, Some(0)).is_none());
    assert!(
        should_skip_rust_plan_save(&ctx, RustPlanRestoreOutcome::NotAttempted, Some(0)).is_none()
    );
}

// The gating env var must disable this short-circuit exactly like it
// disables the restore-side one, even when every other condition matches.
#[test]
fn skip_does_not_fire_when_feature_disabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");

    let plan = save_skip_test_plan();
    let ctx = save_skip_test_context(&plan);

    let result = should_skip_rust_plan_save(&ctx, RustPlanRestoreOutcome::Skipped, Some(0));
    assert!(result.is_none());
}
