//! Tests for the warm-restore short-circuit in [`crate::rust_plan`]:
//! plan_inputs_hash invariants, sentinel write/read round-trips, and the
//! gating env var that lets operators flip the optimisation off.

use crate::rust_plan::{
    compute_plan_inputs_hash, current_unix_seconds, evaluate_warm_restore_skip,
    should_skip_warm_restore, warm_restore_sentinel_path, warm_restore_skip_enabled,
    warm_restore_target_marker_path, write_warm_restore_sentinel, RustArtifactPlan,
    RustArtifactPlanContext, RustPlanInputs, RustPlanPackages, RustToolchainIdentity,
    WarmRestoreSentinel, WarmRestoreSkipInputs, WarmRestoreTargetMarker,
};
use crate::{SKIP_WARM_RESTORE_ENV_VAR, WARM_RESTORE_MAX_AGE_SECONDS};
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;
use tempfile::TempDir;

/// Serialises tests that mutate process-wide environment variables so
/// they do not race with each other under parallel `cargo test`. The
/// guard objects below restore the previous value on drop, but two
/// tests touching the same key concurrently would still observe each
/// other's mid-test state without this lock.
pub(super) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that sets or removes an environment variable for the
/// duration of a test and restores the previous value on drop. Modelled
/// after the same helper in `soldr-core`'s test module.
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

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
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

fn warm_restore_test_plan() -> RustArtifactPlan {
    RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v1"),
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
            manifest_hashes: vec!["M1".to_string(), "M2".to_string()],
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
        journal_log_path: Some("/tmp/journal".to_string()),
    }
}

fn warm_restore_test_sentinel(plan: &RustArtifactPlan) -> WarmRestoreSentinel {
    WarmRestoreSentinel {
        schema_version: 2,
        target_generation: "generation-1".to_string(),
        plan_inputs_hash: compute_plan_inputs_hash(plan),
        target_dir: plan.target_dir.clone(),
        github_run_id: "111".to_string(),
        github_job: "test".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-1".to_string(),
        saved_at_unix_seconds: 1_000_000,
    }
}

/// The sentinel hash must change whenever any plan input cargo would
/// consult to decide freshness changes. Otherwise the warm-restore
/// short-circuit could fire across step pairs that are not actually
/// equivalent.
#[test]
fn plan_inputs_hash_changes_when_inputs_change() {
    let plan_a = warm_restore_test_plan();
    let mut plan_b = warm_restore_test_plan();
    plan_b.inputs.lockfile_hash = "different".to_string();
    assert_ne!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_b),
    );

    let mut plan_c = warm_restore_test_plan();
    plan_c.toolchain.rustc = "rustc 9.9.9".to_string();
    assert_ne!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_c),
    );

    let mut plan_d = warm_restore_test_plan();
    plan_d.target_triple = "aarch64-apple-darwin".to_string();
    assert_ne!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_d),
    );
}

/// Cosmetic plan fields (the journal path, the schema version we
/// already pin to 1) must not leak into the sentinel hash, so an
/// unrelated path swap does not invalidate the warm-restore optim.
#[test]
fn plan_inputs_hash_ignores_cosmetic_fields() {
    let plan_a = warm_restore_test_plan();
    let mut plan_b = warm_restore_test_plan();
    plan_b.journal_log_path = Some("/tmp/other-journal".to_string());
    plan_b.workspace_root = "/different/ws".to_string();
    assert_eq!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_b),
    );
}

/// Happy path: sentinel proves the same plan was just saved into the
/// same target dir from the same CI job/attempt — restore is skipped.
#[test]
fn warm_restore_skip_fires_on_exact_match() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_some(), "expected skip; got {result:?}");
}

/// Plain "no sentinel on disk" must fall through to the normal restore.
#[test]
fn warm_restore_skip_falls_through_when_sentinel_missing() {
    let plan = warm_restore_test_plan();
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: "111",
        github_job: "test",
        github_run_attempt: "1",
        now_unix_seconds: 1_000_000,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    assert!(evaluate_warm_restore_skip(None, &inputs).is_none());
}

/// Sentinel from a prior re-run attempt must NOT short-circuit into a
/// fresh attempt — the action restored the cache from scratch and the
/// `target/` mtimes are no longer guaranteed to be the live ones.
#[test]
fn warm_restore_skip_rejects_mismatched_run_attempt() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: "2", // different attempt
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Sentinel from a different job in the same workflow must not bleed
/// across job boundaries even when the run id matches.
#[test]
fn warm_restore_skip_rejects_mismatched_job() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: "other-job",
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Sentinel for an unrelated target dir (e.g. a sibling workspace
/// also writing into the shared bundle dir) must not short-circuit.
#[test]
fn warm_restore_skip_rejects_mismatched_target_dir() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: "/tmp/different-target",
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Once a plan input changes (lockfile bump, new manifest, etc.) the
/// sentinel hash diverges and restore must run normally.
#[test]
fn warm_restore_skip_rejects_mismatched_inputs_hash() {
    let plan = warm_restore_test_plan();
    let mut sentinel = warm_restore_test_sentinel(&plan);
    sentinel.plan_inputs_hash = "stale-hash".to_string();
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Stale sentinels (older than the configured window) must not
/// short-circuit. Otherwise a leftover sentinel from a previous
/// workflow run could cause skipping in a fresh job that happened to
/// inherit the same env identifiers.
#[test]
fn warm_restore_skip_rejects_stale_sentinel() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + WARM_RESTORE_MAX_AGE_SECONDS + 1;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// A future-version sentinel (say after a soldr upgrade that bumps
/// the schema) must be ignored, never crash, and force a normal
/// restore on the next invocation.
#[test]
fn warm_restore_skip_rejects_unknown_schema_version() {
    let plan = warm_restore_test_plan();
    let mut sentinel = warm_restore_test_sentinel(&plan);
    sentinel.schema_version = 99;
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Sentinel must round-trip as JSON without dropping fields, so
/// disk-roundtrip behavior is observable here too (the
/// filesystem-bound caller relies on serde to be exact).
#[test]
fn warm_restore_sentinel_round_trips_json() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let json = serde_json::to_string(&sentinel).expect("serialize sentinel");
    let parsed: WarmRestoreSentinel = serde_json::from_str(&json).expect("parse sentinel back");
    assert_eq!(parsed, sentinel);
}

/// Build a `RustArtifactPlanContext` whose plan-derived fields match
/// `plan` and whose filesystem-touching paths live under `tempdir`. The
/// other fields are filled with deterministic placeholders so tests can
/// inspect them without caring about the daemon plumbing they would
/// drive in production.
fn warm_restore_test_context(
    plan: &RustArtifactPlan,
    tempdir: &TempDir,
) -> RustArtifactPlanContext {
    let root = tempdir.path();
    RustArtifactPlanContext {
        path: root.join("plan.json"),
        cache_dir: root.join("cache"),
        session_id: "session-test".to_string(),
        journal_path: root.join("journal"),
        backend: "fs".to_string(),
        cache_profile: Some("thin-v1"),
        plan_inputs_hash: compute_plan_inputs_hash(plan),
        target_dir: root.join("target").display().to_string(),
    }
}

/// With the gating env var enabled, `write_warm_restore_sentinel` must
/// materialise a JSON sentinel under the plan's cache dir whose fields
/// reflect the plan inputs and the current GitHub Actions env. This is
/// the producer half of the warm-restore short-circuit.
#[test]
fn write_warm_restore_sentinel_emits_matching_json_when_enabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-42");
    let _job = EnvVarGuard::set("GITHUB_JOB", "test-job");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "3");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    std::fs::create_dir_all(&ctx.target_dir).expect("create target dir");

    write_warm_restore_sentinel(&ctx);

    let sentinel_path = warm_restore_sentinel_path(&ctx);
    let raw =
        std::fs::read_to_string(&sentinel_path).expect("sentinel file should exist after write");
    let sentinel: WarmRestoreSentinel =
        serde_json::from_str(&raw).expect("sentinel JSON should parse");

    assert_eq!(sentinel.schema_version, 2);
    assert!(!sentinel.target_generation.is_empty());
    assert_eq!(sentinel.plan_inputs_hash, ctx.plan_inputs_hash);
    assert_eq!(sentinel.target_dir, ctx.target_dir);
    assert_eq!(sentinel.github_run_id, "run-42");
    assert_eq!(sentinel.github_job, "test-job");
    assert_eq!(sentinel.github_run_attempt, "3");
    assert_eq!(sentinel.session_id, ctx.session_id);

    let marker_raw = std::fs::read_to_string(warm_restore_target_marker_path(&ctx))
        .expect("target marker should exist after write");
    let marker: WarmRestoreTargetMarker =
        serde_json::from_str(&marker_raw).expect("target marker JSON should parse");
    assert_eq!(marker.schema_version, 1);
    assert_eq!(marker.target_generation, sentinel.target_generation);
    assert_eq!(marker.plan_inputs_hash, ctx.plan_inputs_hash);
}

/// When the gating env var is explicitly opted out (falsy value), the
/// producer must be a strict no-op so the short-circuit cannot
/// accidentally fire on the next invocation. No sentinel file should
/// appear on disk.
#[test]
fn write_warm_restore_sentinel_is_noop_when_disabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);

    write_warm_restore_sentinel(&ctx);

    let sentinel_path = warm_restore_sentinel_path(&ctx);
    assert!(
        !sentinel_path.exists(),
        "no sentinel should be written when {SKIP_WARM_RESTORE_ENV_VAR} is set to a falsy value"
    );
}

/// Full filesystem round-trip: write a sentinel that exactly matches
/// the current plan and CI env, then ask `should_skip_warm_restore`
/// whether it should fire. The short-circuit must return `Some` with
/// a non-empty operator-visible reason string.
#[test]
fn should_skip_warm_restore_returns_some_on_full_match() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    std::fs::create_dir_all(&ctx.target_dir).expect("create target dir");
    write_warm_restore_sentinel(&ctx);

    let result = should_skip_warm_restore(&ctx);
    let reason = result.expect("expected Some(reason) on full match");
    assert!(
        !reason.is_empty(),
        "skip reason should be non-empty for operator visibility"
    );
}

/// A sentinel left behind by a previous invocation with a different
/// `plan_inputs_hash` (e.g. after a lockfile bump) must not fire the
/// short-circuit even when the file is otherwise present and fresh.
#[test]
fn should_skip_warm_restore_returns_none_on_hash_mismatch() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    let sentinel_path = warm_restore_sentinel_path(&ctx);
    std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
        .expect("create sentinel parent");
    let sentinel = WarmRestoreSentinel {
        schema_version: 2,
        target_generation: "generation-stale-hash".to_string(),
        plan_inputs_hash: "stale-hash-from-previous-step".to_string(),
        target_dir: ctx.target_dir.clone(),
        github_run_id: "run-7".to_string(),
        github_job: "build".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-prev".to_string(),
        saved_at_unix_seconds: current_unix_seconds(),
    };
    std::fs::write(
        &sentinel_path,
        serde_json::to_string(&sentinel).expect("serialize sentinel"),
    )
    .expect("write sentinel");

    assert!(should_skip_warm_restore(&ctx).is_none());
}

/// When no sentinel file exists at all, the short-circuit must fall
/// through without panicking on the missing-file IO error.
#[test]
fn should_skip_warm_restore_returns_none_when_sentinel_missing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    assert!(!warm_restore_sentinel_path(&ctx).exists());

    assert!(should_skip_warm_restore(&ctx).is_none());
}

/// With the gating env var explicitly opted out (`"0"`), the
/// short-circuit must stay off even when a perfectly-matching sentinel
/// exists. This is the safety property that lets operators disable the
/// feature on demand without having to clear stale sentinel files.
#[test]
fn should_skip_warm_restore_returns_none_when_disabled_even_with_match() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    let sentinel_path = warm_restore_sentinel_path(&ctx);
    std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
        .expect("create sentinel parent");
    let sentinel = WarmRestoreSentinel {
        schema_version: 2,
        target_generation: "generation-disabled".to_string(),
        plan_inputs_hash: ctx.plan_inputs_hash.clone(),
        target_dir: ctx.target_dir.clone(),
        github_run_id: "run-7".to_string(),
        github_job: "build".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-prev".to_string(),
        saved_at_unix_seconds: current_unix_seconds(),
    };
    std::fs::write(
        &sentinel_path,
        serde_json::to_string(&sentinel).expect("serialize sentinel"),
    )
    .expect("write sentinel");

    assert!(should_skip_warm_restore(&ctx).is_none());
}

/// After the #229 validation flip, an unset env var must enable the
/// short-circuit by default. This locks in the default-on contract so
/// future refactors cannot regress it without updating the test.
#[test]
fn warm_restore_skip_enabled_defaults_on() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::remove(SKIP_WARM_RESTORE_ENV_VAR);

    assert!(
        warm_restore_skip_enabled(),
        "warm-restore skip must default to enabled when {SKIP_WARM_RESTORE_ENV_VAR} is unset"
    );
}

/// The default-on flip preserves an explicit opt-out path: each of the
/// recognised falsy spellings (`0`, `false`, `no`, `off`, empty string,
/// case-insensitive) must disable the short-circuit.
#[test]
fn warm_restore_skip_enabled_respects_explicit_falsy() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    for value in ["0", "false", "FALSE", "No", "off", "OFF", "", "  0  "] {
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, value);
        assert!(
            !warm_restore_skip_enabled(),
            "warm-restore skip must be disabled when {SKIP_WARM_RESTORE_ENV_VAR} is set to {value:?}"
        );
    }
}

#[test]
fn warm_restore_skip_rejects_target_deleted_after_save() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-delete");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let target_dir = tempdir.path().join("target");
    std::fs::create_dir_all(&target_dir).expect("create target dir");
    std::fs::write(target_dir.join("artifact.rlib"), b"artifact").expect("seed target artifact");

    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    write_warm_restore_sentinel(&ctx);
    assert!(
        should_skip_warm_restore(&ctx).is_some(),
        "an untouched target generation should skip restore"
    );

    std::fs::remove_dir_all(&target_dir).expect("simulate cargo clean");
    assert!(
        should_skip_warm_restore(&ctx).is_none(),
        "deleting target/ after save must invalidate the warm-restore skip"
    );
}

#[test]
fn warm_restore_skip_rejects_invalid_target_markers() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-markers");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    std::fs::create_dir_all(&ctx.target_dir).expect("create target dir");
    write_warm_restore_sentinel(&ctx);
    assert!(should_skip_warm_restore(&ctx).is_some());

    let marker_path = warm_restore_target_marker_path(&ctx);
    let valid_marker = std::fs::read_to_string(&marker_path).expect("read valid marker");

    std::fs::remove_file(&marker_path).expect("remove marker");
    assert!(
        should_skip_warm_restore(&ctx).is_none(),
        "missing marker must force restore"
    );

    std::fs::write(&marker_path, "{\"schema_version\":").expect("write partial marker");
    assert!(
        should_skip_warm_restore(&ctx).is_none(),
        "partially written marker must force restore"
    );

    let mut stale_marker: WarmRestoreTargetMarker =
        serde_json::from_str(&valid_marker).expect("parse valid marker");
    stale_marker.target_generation = "older-generation".to_string();
    std::fs::write(
        &marker_path,
        serde_json::to_string(&stale_marker).expect("serialize stale marker"),
    )
    .expect("write stale marker");
    assert!(
        should_skip_warm_restore(&ctx).is_none(),
        "stale generation marker must force restore"
    );

    stale_marker.target_generation = serde_json::from_str::<WarmRestoreTargetMarker>(&valid_marker)
        .expect("parse valid marker again")
        .target_generation;
    stale_marker.plan_inputs_hash = "different-plan".to_string();
    std::fs::write(
        &marker_path,
        serde_json::to_string(&stale_marker).expect("serialize mismatched marker"),
    )
    .expect("write mismatched marker");
    assert!(
        should_skip_warm_restore(&ctx).is_none(),
        "plan-mismatched marker must force restore"
    );
}
