//! Regression test for issue #1538: the thin-v2 diagnostic manifest
//! (`manifest.v2.json`) must be scoped to the bundle
//! [`crate::rust_plan::run_zccache_rust_plan`] just produced, not to the
//! *accumulated* rust-plan cache root (which, in the default/unpinned
//! configuration, keeps every cache key ever saved locally).
//!
//! Before the fix, `write_thin_manifest` was always called with
//! `plan.cache_dir` — the shared `rust-plan-cache/` root — so every save
//! re-walked and re-sorted every bundle the cache root had ever
//! accumulated, and the emitted manifest silently mixed files from
//! unrelated cache keys (different packages/profiles/target triples) into
//! one file list.

use crate::rust_plan::{run_zccache_rust_plan, RustArtifactPlanContext, ThinSliceManifest};
use crate::THIN_MANIFEST_FILENAME;
use zccache::artifact::{
    rust_plan_bundle_dir, rust_plan_cache_key, RustArtifactPlanV1, RustPlanInputs, RustPlanMode,
    RustPlanPackages, RustToolchainIdentity, RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
    RUST_ARTIFACT_PLAN_SCHEMA_VERSION,
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

fn thin_v2_plan(
    workspace: &std::path::Path,
    target_dir: &std::path::Path,
    package_id: &str,
) -> RustArtifactPlanV1 {
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
        // Distinguish the two plans' cache keys (soldr#461: package ids
        // fold into `rust_plan_identity_hash`).
        packages: RustPlanPackages {
            selected_package_ids: vec![package_id.to_string()],
            ..RustPlanPackages::default()
        },
        allowed_artifact_classes: Vec::new(),
        cache_schema_version: RUST_ARTIFACT_CACHE_SCHEMA_VERSION,
        journal_log_path: None,
        cache_profile: Some("thin-v2".to_string()),
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
        cache_profile: Some("thin-v2"),
        plan_inputs_hash: "hash".into(),
        target_dir: String::new(),
    }
}

fn save_one_bundle(
    cache_dir: &std::path::Path,
    package_id: &str,
    artifact_name: &str,
) -> RustArtifactPlanV1 {
    let workspace = unique_dir(&format!("rustplan-ws-{package_id}"));
    let target_dir = workspace.join("target");
    let artifact = target_dir.join("debug").join("deps").join(artifact_name);
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"contents").unwrap();

    let plan = thin_v2_plan(&workspace, &target_dir, package_id);
    let plan_path = unique_dir(&format!("rustplan-plan-{package_id}")).join("plan.pb");
    std::fs::write(&plan_path, plan.to_proto_bytes().expect("encode plan")).unwrap();

    let ctx = context_for(&plan_path, cache_dir);
    run_zccache_rust_plan(&ctx, "save", true).expect("in-process local save");
    plan
}

#[test]
fn thin_v2_manifest_is_scoped_to_current_bundle_not_accumulated_cache() {
    // Shared, unpinned cache dir — the default local-dev configuration
    // this issue is about (no SOLDR_TARGET_CACHE_BUNDLE_DIR override).
    let cache_dir = unique_dir("rustplan-accum-cache");

    // Save an unrelated bundle first ("prior build in the accumulated
    // cache"), then save the bundle under test.
    let _prior_plan = save_one_bundle(&cache_dir, "unrelated-pkg@1.0.0", "libunrelated.rlib");
    let plan_under_test = save_one_bundle(&cache_dir, "demo-pkg@1.0.0", "libdemo.rlib");

    let cache_key = rust_plan_cache_key(&plan_under_test);
    let bundle_dir = rust_plan_bundle_dir(&cache_dir, &cache_key).into_path_buf();
    let manifest_path = bundle_dir.join(THIN_MANIFEST_FILENAME);
    assert!(
        manifest_path.is_file(),
        "expected the thin-slice manifest next to the current bundle at {}",
        manifest_path.display()
    );

    let raw = std::fs::read_to_string(&manifest_path).expect("read manifest");
    let manifest: ThinSliceManifest = serde_json::from_str(&raw).expect("parse manifest");

    let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains("libdemo.rlib")),
        "manifest must list the current bundle's own artifact; got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains("libunrelated.rlib")),
        "manifest must NOT list a file from a different (unrelated) \
             cache key's bundle — it must be scoped to the current bundle, \
             not the accumulated cache dir; got {paths:?}"
    );

    // No manifest.v2.json should have been written at the top of the
    // shared accumulated cache root either — that was the old (buggy)
    // location.
    assert!(
        !cache_dir.join(THIN_MANIFEST_FILENAME).exists(),
        "manifest must not land at the accumulated cache root"
    );

    let _ = std::fs::remove_dir_all(&cache_dir);
}
