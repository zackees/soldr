//! Tests for [`crate::rust_plan::build_rust_artifact_plan`] and the
//! allowed/dropped artifact-class metadata derived from `(mode, cache_profile)`.

use crate::build_cache_session::BuildCacheSession;
use crate::cargo_front_door::{cargo_profile, cargo_target_triple, selected_cargo_args};
use crate::rust_plan::{
    allowed_artifact_classes, build_rust_artifact_plan, cargo_metadata_passthrough_args,
    derive_toolchain_identity, dropped_artifact_classes, rust_artifact_cache_profile_from_env,
    CargoMetadata, CargoMetadataPackage, RustToolchainIdentity, ToolchainProbe,
    WorkspaceFileHashes,
};
use crate::TARGET_CACHE_PROFILE_ENV_VAR;
use std::sync::Mutex;

/// Serialises tests that mutate `SOLDR_TARGET_CACHE_PROFILE` so they
/// don't race under parallel `cargo test`. Matches the pattern used in
/// `warm_restore` for `SKIP_WARM_RESTORE_ENV_VAR`.
static PROFILE_ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn explicit_dylint_channel_overrides_parent_toolchain_in_plan_identity() {
    let probe = ToolchainProbe {
        rustc_verbose: concat!(
            "rustc 1.94.0-nightly (111111111 2026-01-17)\n",
            "commit-hash: 1111111111111111111111111111111111111111\n",
            "host: x86_64-unknown-linux-gnu\n",
            "release: 1.94.0-nightly\n",
        )
        .to_string(),
        cargo_version: "cargo 1.94.0-nightly\n".to_string(),
    };
    let identity = derive_toolchain_identity(&probe, Some("nightly-2026-01-18"));
    assert_eq!(identity.channel, "nightly-2026-01-18");
    assert!(identity
        .rustc
        .contains("1111111111111111111111111111111111111111"));
}

#[test]
fn rust_artifact_plan_selects_external_packages_and_path_exclusions() {
    let root = std::env::temp_dir().join(format!("soldr-rust-plan-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::create_dir_all(root.join("local_dep/src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();
    std::fs::write(
        root.join("local_dep/Cargo.toml"),
        "[package]\nname='local_dep'\n",
    )
    .unwrap();

    let metadata = CargoMetadata {
        workspace_root: root.clone(),
        target_directory: root.join("target"),
        workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
        packages: vec![
            CargoMetadataPackage {
                id: "path+file:///repo/app#app@0.1.0".to_string(),
                source: None,
                manifest_path: None,
            },
            CargoMetadataPackage {
                id: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0".to_string(),
                source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
                manifest_path: None,
            },
            CargoMetadataPackage {
                id: "path+file:///repo/local_dep#local_dep@0.1.0".to_string(),
                source: None,
                manifest_path: None,
            },
        ],
    };
    let toolchain = RustToolchainIdentity {
        rustc: "rustc 1.0.0-test".to_string(),
        cargo: "cargo 1.0.0-test".to_string(),
        channel: "test".to_string(),
        host: "x86_64-unknown-test".to_string(),
    };
    let session = BuildCacheSession {
        cache_dir: root.join("cache"),
        session_id: "session-1".to_string(),
        session_log_path: root.join("cache/logs/last-session.log"),
        journal_path: root.join("cache/logs/last-session.jsonl"),
        session_stats_path: root.join("cache/logs/last-session-stats.json"),
    };
    let args = vec![
        "build".to_string(),
        "--release".to_string(),
        "--features".to_string(),
        "serde/derive".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];

    let file_hashes =
        WorkspaceFileHashes::collect(&metadata.workspace_root).expect("collect file hashes");
    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        &args,
        "thin",
        Some("thin-v1"),
        &session,
        None,
        &file_hashes,
    )
    .expect("build rust artifact plan");

    assert_eq!(plan.schema_version, 1);
    assert_eq!(plan.mode, "thin");
    assert_eq!(plan.cache_profile, Some("thin-v1"));
    assert_eq!(plan.profile, "release");
    assert_eq!(plan.target_triple, "x86_64-unknown-linux-gnu");
    assert_eq!(plan.packages.workspace_package_ids.len(), 1);
    assert_eq!(plan.packages.selected_package_ids.len(), 1);
    assert!(plan.packages.selected_package_ids[0].contains("serde"));
    assert_eq!(plan.packages.excluded_path_package_ids.len(), 1);
    assert!(plan.allowed_artifact_classes.contains(&"cargo_fingerprint"));
    assert!(plan.dropped_artifact_classes.is_empty());
    assert_eq!(plan.cache_schema_version, 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rust_artifact_plan_helpers_parse_mode_profile_target_and_metadata_args() {
    let args = vec![
        "+stable".to_string(),
        "build".to_string(),
        "--locked".to_string(),
        "--features=fast".to_string(),
        "--target".to_string(),
        "wasm32-unknown-unknown".to_string(),
        "--profile".to_string(),
        "release-lto".to_string(),
        "--".to_string(),
        "--ignored".to_string(),
    ];

    assert_eq!(cargo_profile(&args), "release-lto");
    assert_eq!(
        cargo_target_triple(&args, "x86_64-unknown-linux-gnu"),
        "wasm32-unknown-unknown"
    );
    assert_eq!(
        selected_cargo_args(&args, &["--features"]),
        vec!["--features=fast".to_string()]
    );
    assert_eq!(allowed_artifact_classes("full", None), Vec::<&str>::new());
    assert_eq!(
        cargo_metadata_passthrough_args(&args)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        vec!["--locked".to_string(), "--features=fast".to_string()]
    );
}

/// `thin-v1` is the legacy slice. It must continue to ship the
/// historically-included library-output classes so rollout day 0 is a
/// no-op for callers that did not opt in to `thin-v2`.
#[test]
fn allowed_artifact_classes_thin_v1_keeps_legacy_set() {
    let allowed = allowed_artifact_classes("thin", Some("thin-v1"));
    for expected in [
        "rlib",
        "rmeta",
        "dep_info",
        "proc_macro",
        "cargo_fingerprint",
        "build_script_metadata",
        "build_script_output",
        // soldr#1579: thin-v1 must also retain the compiled build-script
        // binary itself. Without it, cargo's fingerprint check for units
        // whose build.rs output feeds their own compilation sees a stale
        // `build_script_build` artifact and cascades a `StaleDepFingerprint`
        // rebuild through everything downstream of that build script.
        "build_script_build",
    ] {
        assert!(
            allowed.contains(&expected),
            "thin-v1 must keep {expected} in the allowlist; got {allowed:?}"
        );
    }
    assert!(dropped_artifact_classes("thin", Some("thin-v1")).is_empty());
}

/// `thin-v2` keeps the freshness metadata plus the primary outputs that
/// Cargo's JSON closure can verify. Transient/debug categories remain absent.
#[test]
fn allowed_artifact_classes_thin_v2_keeps_hydratable_outputs() {
    let allowed = allowed_artifact_classes("thin", Some("thin-v2"));

    // Drop list per design Section 3.2.
    for forbidden in ["incremental", "cargo_fingerprint", "dwo", "pdb", "dsym"] {
        assert!(
            !allowed.contains(&forbidden),
            "thin-v2 must drop {forbidden} from the allowlist; got {allowed:?}"
        );
    }

    // Keep list per design Section 3.1.
    for required in [
        "cargo_fingerprint_meta",
        "dep_info",
        "build_script_metadata",
        "build_script_output",
        "rlib",
        "rmeta",
        "proc_macro",
        "shared_lib",
        "build_script_build",
        "cargo_fingerprint_outputs",
    ] {
        assert!(
            allowed.contains(&required),
            "thin-v2 must keep {required} in the allowlist; got {allowed:?}"
        );
    }

    // The drop list is surfaced as data so zccache can short-circuit.
    let dropped = dropped_artifact_classes("thin", Some("thin-v2"));
    for forbidden in [
        "incremental",
        "rlib",
        "rmeta",
        "proc_macro",
        "build_script_build",
        "dwo",
        "pdb",
        "dsym",
        "cargo_fingerprint_outputs",
    ] {
        assert!(
            dropped.contains(&forbidden),
            "thin-v2 must publish {forbidden} in dropped_artifact_classes; got {dropped:?}"
        );
    }
}

/// Bumping `cache_schema_version` from 1 to 2 is the contract zccache
/// uses to decide whether the new fingerprint split is in effect.
#[test]
fn rust_artifact_plan_bumps_cache_schema_version_for_thin_v2() {
    let root = std::env::temp_dir().join(format!(
        "soldr-rust-plan-thinv2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();

    let metadata = CargoMetadata {
        workspace_root: root.clone(),
        target_directory: root.join("target"),
        workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
        packages: vec![CargoMetadataPackage {
            id: "path+file:///repo/app#app@0.1.0".to_string(),
            source: None,
            manifest_path: None,
        }],
    };
    let toolchain = RustToolchainIdentity {
        rustc: "rustc 1.0.0-test".to_string(),
        cargo: "cargo 1.0.0-test".to_string(),
        channel: "test".to_string(),
        host: "x86_64-unknown-test".to_string(),
    };
    let session = BuildCacheSession {
        cache_dir: root.join("cache"),
        session_id: "session-thinv2".to_string(),
        session_log_path: root.join("cache/logs/last-session.log"),
        journal_path: root.join("cache/logs/last-session.jsonl"),
        session_stats_path: root.join("cache/logs/last-session-stats.json"),
    };

    let file_hashes =
        WorkspaceFileHashes::collect(&metadata.workspace_root).expect("collect file hashes");
    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        &["build".to_string()],
        "thin",
        Some("thin-v2"),
        &session,
        None,
        &file_hashes,
    )
    .expect("build rust artifact plan");

    assert_eq!(plan.schema_version, 1, "outer schema is unchanged");
    assert_eq!(
        plan.cache_schema_version, 2,
        "thin-v2 bumps the cache-side schema so zccache can branch on it"
    );
    assert_eq!(plan.cache_profile, Some("thin-v2"));
    assert!(plan.allowed_artifact_classes.contains(&"dep_info"));
    assert!(plan.allowed_artifact_classes.contains(&"rlib"));

    let _ = std::fs::remove_dir_all(&root);
}

/// `thin-v3` is an explicit compatibility boundary: until Cargo supplies a
/// complete package-to-artifact closure, Soldr asks zccache to retain all
/// durable compiler output rather than risk a partial cook partition.
#[test]
fn rust_artifact_plan_marks_thin_v3_zccache_all_fallback() {
    let root = std::env::temp_dir().join(format!(
        "soldr-rust-plan-thinv3-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();

    let metadata = CargoMetadata {
        workspace_root: root.clone(),
        target_directory: root.join("target"),
        workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
        packages: vec![CargoMetadataPackage {
            id: "path+file:///repo/app#app@0.1.0".to_string(),
            source: None,
            manifest_path: None,
        }],
    };
    let toolchain = RustToolchainIdentity {
        rustc: "rustc 1.0.0-test".to_string(),
        cargo: "cargo 1.0.0-test".to_string(),
        channel: "test".to_string(),
        host: "x86_64-unknown-test".to_string(),
    };
    let session = BuildCacheSession {
        cache_dir: root.join("cache"),
        session_id: "session-thinv3".to_string(),
        session_log_path: root.join("cache/logs/last-session.log"),
        journal_path: root.join("cache/logs/last-session.jsonl"),
        session_stats_path: root.join("cache/logs/last-session-stats.json"),
    };
    let file_hashes =
        WorkspaceFileHashes::collect(&metadata.workspace_root).expect("collect file hashes");
    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        &["build".to_string()],
        "thin",
        Some("thin-v3"),
        &session,
        None,
        &file_hashes,
    )
    .expect("build rust artifact plan");

    assert_eq!(plan.cache_schema_version, 3);
    assert_eq!(plan.cache_profile, Some("thin-v3"));
    assert_eq!(
        plan.packages.ownership_policy,
        Some("thin-v3-lifetime-partition-v1")
    );
    assert_eq!(plan.packages.ownership_mode, Some("zccache-all-v1"));
    assert!(!plan.packages.ownership_complete);
    assert!(plan.packages.artifact_owners.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

/// Default flip for issue #461: when `SOLDR_TARGET_CACHE_PROFILE` is
/// unset, soldr must pick `thin-v2` so warm CI restores no longer ship
/// the heavy `.rlib`/`.rmeta`/proc-macro bytes through the GHA cache.
/// Requires managed zccache >= 1.9.1 (which understands `cache_profile`
/// and `dropped_artifact_classes`).
#[test]
fn rust_artifact_cache_profile_default_is_thin_v2() {
    let _lock = PROFILE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os(TARGET_CACHE_PROFILE_ENV_VAR);
    std::env::remove_var(TARGET_CACHE_PROFILE_ENV_VAR);
    let result = rust_artifact_cache_profile_from_env();
    if let Some(value) = previous {
        std::env::set_var(TARGET_CACHE_PROFILE_ENV_VAR, value);
    }
    assert_eq!(result.expect("default profile resolves"), "thin-v2");
}

/// Operators pinned to zccache < 1.9.1 (or recovering from a thin-v2
/// regression) must still be able to opt back into the legacy slice via
/// an explicit `SOLDR_TARGET_CACHE_PROFILE=thin-v1`.
#[test]
fn rust_artifact_cache_profile_thin_v1_remains_explicit_opt_out() {
    let _lock = PROFILE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os(TARGET_CACHE_PROFILE_ENV_VAR);
    std::env::set_var(TARGET_CACHE_PROFILE_ENV_VAR, "thin-v1");
    let result = rust_artifact_cache_profile_from_env();
    match previous {
        Some(value) => std::env::set_var(TARGET_CACHE_PROFILE_ENV_VAR, value),
        None => std::env::remove_var(TARGET_CACHE_PROFILE_ENV_VAR),
    }
    assert_eq!(result.expect("thin-v1 still parses"), "thin-v1");
}
