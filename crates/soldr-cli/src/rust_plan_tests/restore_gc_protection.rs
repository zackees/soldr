//! Regression tests for issue #1558: the pre-cargo target-GC pass must
//! not discard hash families a verified rust-plan restore just
//! materialized into a fresh `target/`.
//!
//! Observed failure mode (#1529 three-sample validation): after a correct
//! 745-file restore into a deleted `target/`, the front door's
//! `AutoPrunePhase::Before` keep-latest pass pruned 64 restored hash
//! families before cargo evaluated them, so the build still ran 123
//! `Compiling` units. The keep-latest strategy keeps a single hash family
//! per artifact prefix, but a restored bundle legitimately carries
//! multiple live families per prefix (build-dep vs. normal-dep variants
//! of the same crate) with preserved bundle timestamps.
//!
//! The fix routes the pre-cargo pass through
//! [`crate::cargo_front_door::run_pre_cargo_target_gc`], which skips the
//! destructive pass when [`RustPlanRestoreOutcome`] says a restore just
//! materialized files. These tests prove:
//!
//! 1. just-restored dual hash families survive the pre-cargo GC decision
//!    (the #1558 regression),
//! 2. without the restore signal the same pass still prunes the stale
//!    family (GC correctness is not weakened elsewhere),
//! 3. skip/empty/absent restore outcomes fall back to the existing GC.

use crate::cargo_front_door::run_pre_cargo_target_gc;
use crate::cargo_front_door::CargoCachePlan;
use crate::rust_plan::{RustArtifactPlanContext, RustPlanRestoreOutcome};
use zccache::artifact::{
    RustArtifactPlanV1, RustPlanInputs, RustPlanMode, RustPlanPackages, RustToolchainIdentity,
    RUST_ARTIFACT_CACHE_SCHEMA_VERSION, RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
};

fn unique_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn full_mode_plan(workspace: &std::path::Path, target_dir: &std::path::Path) -> RustArtifactPlanV1 {
    RustArtifactPlanV1 {
        schema_version: RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
        mode: RustPlanMode::Full,
        workspace_root: workspace.to_path_buf().into(),
        target_dir: target_dir.to_path_buf().into(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc".into(),
            cargo: "cargo".into(),
            channel: "stable".into(),
            host: "x86_64-unknown-linux-gnu".into(),
        },
        target_triple: "x86_64-unknown-linux-gnu".into(),
        profile: "debug".into(),
        inputs: RustPlanInputs {
            features_hash: "feat".into(),
            rustflags_hash: "flags".into(),
            env_hash: "env".into(),
            lockfile_hash: "lock".into(),
            cargo_config_hash: "cfg".into(),
            manifest_hashes: vec!["manifest".into()],
        },
        packages: RustPlanPackages::default(),
        allowed_artifact_classes: Vec::new(),
        cache_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        journal_log_path: None,
        cache_profile: None,
        dropped_artifact_classes: Vec::new(),
    }
}

fn context_for(
    plan_path: &std::path::Path,
    cache_dir: &std::path::Path,
    target_dir: &std::path::Path,
) -> RustArtifactPlanContext {
    RustArtifactPlanContext {
        path: plan_path.to_path_buf(),
        zccache_binary: std::path::PathBuf::from("zccache"),
        cache_dir: cache_dir.to_path_buf(),
        zccache_daemon_cache_dir: cache_dir.to_path_buf(),
        zccache_daemon_cache_dir_env: false,
        zccache_daemon_name: None,
        session_id: "test-session".into(),
        journal_path: cache_dir.join("journal.jsonl"),
        backend: "local".into(),
        cache_profile: None,
        plan_inputs_hash: "hash".into(),
        target_dir: target_dir.display().to_string(),
    }
}

/// One live hash family: a `deps/` rlib plus its cargo
/// `.fingerprint/<name>-<hash>/invoked.timestamp`, both stamped with
/// `mtime_unix` so keep-latest ranking is deterministic.
fn write_family(target: &std::path::Path, hash: &str, mtime_unix: u64) {
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime_unix);
    let rlib = target
        .join("debug")
        .join("deps")
        .join(format!("libdemo-{hash}.rlib"));
    std::fs::create_dir_all(rlib.parent().unwrap()).unwrap();
    std::fs::write(&rlib, format!("demo-rlib-{hash}")).unwrap();
    let invoked = target
        .join("debug")
        .join(".fingerprint")
        .join(format!("demo-{hash}"))
        .join("invoked.timestamp");
    std::fs::create_dir_all(invoked.parent().unwrap()).unwrap();
    std::fs::write(&invoked, b"").unwrap();
    for path in [&rlib, &invoked] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
}

fn family_files(target: &std::path::Path, hash: &str) -> [std::path::PathBuf; 2] {
    [
        target
            .join("debug")
            .join("deps")
            .join(format!("libdemo-{hash}.rlib")),
        target
            .join("debug")
            .join(".fingerprint")
            .join(format!("demo-{hash}"))
            .join("invoked.timestamp"),
    ]
}

const OLD_HASH: &str = "aaaaaaaaaaaaa";
const NEW_HASH: &str = "bbbbbbbbbbbbb";

/// Save a target tree carrying two live hash families of the same
/// artifact prefix, wipe `target/`, and restore it through the real
/// `CargoCachePlan::restore_rust_artifacts` path. Returns the restore
/// outcome plus the workspace/target/cache dirs for cleanup.
fn save_wipe_restore() -> (
    RustPlanRestoreOutcome,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let workspace = unique_dir("gc1558-ws");
    let target = workspace.join("target");
    // Two hash families of the same prefix — e.g. the build-dependency
    // and normal-dependency compilations of one crate. Both are live in
    // cargo's eyes; keep-latest would keep only the newer one.
    write_family(&target, OLD_HASH, 1_700_000_000);
    write_family(&target, NEW_HASH, 1_700_000_500);

    let cache_dir = unique_dir("gc1558-cache");
    let plan_path = unique_dir("gc1558-plan").join("plan.pb");
    let plan = full_mode_plan(&workspace, &target);
    std::fs::write(&plan_path, plan.to_proto_bytes().expect("encode plan")).unwrap();

    let ctx = context_for(&plan_path, &cache_dir, &target);
    crate::rust_plan::run_zccache_rust_plan(&ctx, "save", true).expect("in-process local save");

    // Whole-target deletion — the fresh-target scenario from the issue.
    std::fs::remove_dir_all(&target).unwrap();
    assert!(!target.exists());

    let cache_plan = CargoCachePlan::for_test_with_rust_artifact_plan(ctx);
    let outcome = cache_plan
        .restore_rust_artifacts()
        .expect("in-process local restore");

    (outcome, workspace, target, cache_dir)
}

crate::timed_test!(restored_hash_families_survive_pre_cargo_target_gc, {
    let (outcome, workspace, target, cache_dir) = save_wipe_restore();

    // The verified restore materialized all four files (2 rlibs + 2
    // invoked.timestamp) and reports itself as a Restored outcome.
    assert_eq!(
        outcome,
        RustPlanRestoreOutcome::Restored {
            restored_file_count: 4
        },
        "restore must report the materialized file count"
    );

    // The front-door pre-cargo GC decision: with a just-completed
    // restore the destructive pass must be skipped entirely.
    let gc = run_pre_cargo_target_gc(&target, &outcome);
    assert!(
        gc.is_none(),
        "pre-cargo target GC must be skipped right after a restore that \
         materialized files, got {gc:?}"
    );

    // Both restored hash families must still be on disk for cargo — the
    // #1558 regression deleted the older family here.
    for hash in [OLD_HASH, NEW_HASH] {
        for file in family_files(&target, hash) {
            assert!(
                file.exists(),
                "restored family file must survive pre-cargo GC: {}",
                file.display()
            );
        }
    }

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&cache_dir);
});

crate::timed_test!(
    pre_cargo_gc_without_restore_signal_still_prunes_stale_family,
    {
        // Same restored tree, but pretend no restore ran (e.g. target cache
        // disabled). The existing keep-latest pass must still prune the
        // stale/older hash family — protection must not weaken GC for
        // ordinary builds. This is also the demonstration of the #1558 bug:
        // before the fix the front door took exactly this path right after
        // the restore.
        let (_, workspace, target, cache_dir) = save_wipe_restore();

        let gc = run_pre_cargo_target_gc(&target, &RustPlanRestoreOutcome::NotAttempted);
        let gc = gc.expect("without a restore signal the GC pass must run");
        assert!(
            gc.deleted > 0,
            "keep-latest must prune the stale hash family, got {gc:?}"
        );
        for file in family_files(&target, OLD_HASH) {
            assert!(
                !file.exists(),
                "stale family file must be pruned: {}",
                file.display()
            );
        }
        for file in family_files(&target, NEW_HASH) {
            assert!(
                file.exists(),
                "newest family file must be kept: {}",
                file.display()
            );
        }

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
);

crate::timed_test!(non_restore_outcomes_fall_back_to_existing_gc, {
    // Unknown or partial plan state falls back to the existing GC pass
    // (issue #1527 correctness constraint): only a restore that actually
    // materialized files may skip it.
    for outcome in [
        RustPlanRestoreOutcome::NotAttempted,
        RustPlanRestoreOutcome::Skipped,
        RustPlanRestoreOutcome::Restored {
            restored_file_count: 0,
        },
    ] {
        assert_eq!(
            outcome.materialized_file_count(),
            None,
            "{outcome:?} must not claim materialized files"
        );
        let target = unique_dir("gc1558-fallback");
        let gc = run_pre_cargo_target_gc(&target, &outcome);
        assert!(
            gc.is_some(),
            "GC pass must run for non-materializing outcome {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&target);
    }

    assert_eq!(
        RustPlanRestoreOutcome::Restored {
            restored_file_count: 3
        }
        .materialized_file_count(),
        Some(3)
    );
});

crate::timed_test!(prepopulated_target_restore_maps_to_skipped_outcome, {
    // A populated target/ trips the #480 guard; the outcome must be
    // Skipped so the pre-cargo GC keeps running exactly as before.
    let workspace = unique_dir("gc1558-prepop-ws");
    let target = workspace.join("target");
    write_family(&target, NEW_HASH, 1_700_000_500);

    let cache_dir = unique_dir("gc1558-prepop-cache");
    let plan_path = unique_dir("gc1558-prepop-plan").join("plan.pb");
    let plan = full_mode_plan(&workspace, &target);
    std::fs::write(&plan_path, plan.to_proto_bytes().expect("encode plan")).unwrap();

    let ctx = context_for(&plan_path, &cache_dir, &target);
    let cache_plan = CargoCachePlan::for_test_with_rust_artifact_plan(ctx);
    let outcome = cache_plan
        .restore_rust_artifacts()
        .expect("prepopulated restore resolves to a skip, not an error");
    assert_eq!(
        outcome,
        RustPlanRestoreOutcome::Skipped,
        "prepopulated target must map to the Skipped outcome"
    );
    assert_eq!(outcome.materialized_file_count(), None);

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&cache_dir);
});
