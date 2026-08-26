//! In-process local save/restore round-trip through
//! [`crate::rust_plan::run_zccache_rust_plan`] (soldr#1368).
//!
//! Before #1368 `run_zccache_rust_plan` shelled out to `zccache rust-plan
//! save|restore`. It now calls the compiled-in `zccache::artifact` library
//! (`save_rust_plan_local` / `restore_rust_plan_local`) directly. This test
//! proves the wiring: a `full`-mode plan saves a target-tree artifact into a
//! local bundle and restores it after the file is deleted — with no
//! subprocess and no managed zccache binary.

use crate::rust_plan::{run_zccache_rust_plan, RustArtifactPlanContext};
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
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
    }
}

fn context_for(
    plan_path: &std::path::Path,
    cache_dir: &std::path::Path,
) -> RustArtifactPlanContext {
    RustArtifactPlanContext {
        path: plan_path.to_path_buf(),
        cache_dir: cache_dir.to_path_buf(),
        session_id: "test-session".into(),
        journal_path: cache_dir.join("journal.jsonl"),
        backend: "local".into(),
        cache_profile: None,
        plan_inputs_hash: "hash".into(),
        target_dir: String::new(),
    }
}

#[test]
fn run_zccache_rust_plan_local_save_then_restore_roundtrips_an_artifact() {
    let workspace = unique_dir("rustplan-ws");
    let target_dir = workspace.join("target");
    // A plausible full-target artifact under target/debug/deps.
    let artifact = target_dir.join("debug").join("deps").join("libdemo.rlib");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"demo-rlib-contents").unwrap();

    let cache_dir = unique_dir("rustplan-cache");
    let plan_path = unique_dir("rustplan-plan").join("plan.pb");
    let plan = full_mode_plan(&workspace, &target_dir);
    std::fs::write(&plan_path, plan.to_proto_bytes().expect("encode plan")).unwrap();

    let ctx = context_for(&plan_path, &cache_dir);

    // Save: in-process `save_rust_plan_local` — must not spawn anything.
    run_zccache_rust_plan(&ctx, "save", true).expect("in-process local save");

    // Delete the artifact so restore has to bring it back.
    std::fs::remove_file(&artifact).unwrap();
    assert!(!artifact.exists());

    // Restore: in-process `restore_rust_plan_local`.
    run_zccache_rust_plan(&ctx, "restore", false).expect("in-process local restore");

    assert!(
        artifact.exists(),
        "restore must recreate the saved target artifact at {}",
        artifact.display()
    );
    assert_eq!(std::fs::read(&artifact).unwrap(), b"demo-rlib-contents");

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[test]
fn run_zccache_rust_plan_errors_on_missing_plan_file() {
    // Proves the in-process load path is exercised (no subprocess): a
    // missing plan file surfaces as a soldr error, not a spawn failure.
    let cache_dir = unique_dir("rustplan-missing-cache");
    let ctx = context_for(&cache_dir.join("does-not-exist.pb"), &cache_dir);
    let err =
        run_zccache_rust_plan(&ctx, "restore", false).expect_err("missing plan file must error");
    assert!(
        err.to_string().contains("load rust-plan"),
        "error should come from the in-process plan load, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&cache_dir);
}
