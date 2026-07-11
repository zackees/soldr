//! Wire-format regression tests for [`crate::rust_plan::RustArtifactPlan`].
//! zccache v1.4.0 uses `#[serde(deny_unknown_fields)]` on
//! `RustArtifactPlanV1`, so the legacy `thin-v1` / `full` shapes must NOT
//! serialize `cache_profile` or `dropped_artifact_classes`. The `thin-v2`
//! opt-in is allowed (and required) to surface them.

use crate::rust_plan::{RustArtifactPlan, RustPlanInputs, RustPlanPackages, RustToolchainIdentity};

/// Regression test for the zccache v1.4.0 wire-compat bug. zccache
/// v1.4.0 deserializes the plan with `#[serde(deny_unknown_fields)]`
/// and does NOT know about `cache_profile` / `dropped_artifact_classes`.
/// Therefore the default `thin-v1` (and `full`) JSON must look exactly
/// like the pre-PR plan: neither field may appear in the JSON. The
/// thin-v2 opt-in is allowed (and required) to surface them.
#[test]
fn rust_artifact_plan_thin_v1_json_omits_new_fields_for_zccache_compat() {
    let plan = RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v1"),
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "f".to_string(),
            rustflags_hash: "r".to_string(),
            env_hash: "e".to_string(),
            lockfile_hash: "l".to_string(),
            cargo_config_hash: "c".to_string(),
            manifest_hashes: vec![],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![],
            workspace_package_ids: vec![],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec!["cargo_fingerprint"],
        dropped_artifact_classes: vec![],
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
        cache_schema_version: 1,
        journal_log_path: None,
    };

    let json = serde_json::to_string(&plan).expect("serialize thin-v1 plan");
    assert!(
        !json.contains("\"cache_profile\""),
        "thin-v1 plan must NOT serialize cache_profile (zccache v1.4.0 \
         rejects unknown fields); got: {json}"
    );
    assert!(
        !json.contains("\"dropped_artifact_classes\""),
        "thin-v1 plan must NOT serialize dropped_artifact_classes; got: {json}"
    );
}

/// `full` mode also predates the new fields and zccache's strict
/// deserializer rejects them, so `cache_profile == None` plus an empty
/// drop list must serialize without either field.
#[test]
fn rust_artifact_plan_full_mode_json_omits_new_fields() {
    let plan = RustArtifactPlan {
        schema_version: 1,
        mode: "full".to_string(),
        cache_profile: None,
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "f".to_string(),
            rustflags_hash: "r".to_string(),
            env_hash: "e".to_string(),
            lockfile_hash: "l".to_string(),
            cargo_config_hash: "c".to_string(),
            manifest_hashes: vec![],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![],
            workspace_package_ids: vec![],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec![],
        dropped_artifact_classes: vec![],
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
        cache_schema_version: 1,
        journal_log_path: None,
    };

    let json = serde_json::to_string(&plan).expect("serialize full plan");
    assert!(!json.contains("\"cache_profile\""), "got: {json}");
    assert!(
        !json.contains("\"dropped_artifact_classes\""),
        "got: {json}"
    );
}

/// thin-v2 is the opt-in that ships the new wire fields. zccache
/// builds that consume thin-v2 must see both `cache_profile` and the
/// non-empty `dropped_artifact_classes` list.
#[test]
fn rust_artifact_plan_thin_v2_json_includes_new_fields() {
    let plan = RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v2"),
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "f".to_string(),
            rustflags_hash: "r".to_string(),
            env_hash: "e".to_string(),
            lockfile_hash: "l".to_string(),
            cargo_config_hash: "c".to_string(),
            manifest_hashes: vec![],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![],
            workspace_package_ids: vec![],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec!["dep_info"],
        dropped_artifact_classes: vec!["rlib", "rmeta"],
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
        cache_schema_version: 2,
        journal_log_path: None,
    };

    let json = serde_json::to_string(&plan).expect("serialize thin-v2 plan");
    assert!(
        json.contains("\"cache_profile\":\"thin-v2\""),
        "thin-v2 must serialize cache_profile; got: {json}"
    );
    assert!(
        json.contains("\"dropped_artifact_classes\""),
        "thin-v2 must serialize dropped_artifact_classes; got: {json}"
    );
}
