#![allow(unused_imports)]

mod common;

use common::*;
use prost::Message as _;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, PartialEq, prost::Message)]
struct RustArtifactPlanProto {
    #[prost(uint32, tag = "1")]
    schema_version: u32,
    #[prost(uint32, tag = "2")]
    mode: u32,
    #[prost(message, optional, tag = "9")]
    packages: Option<RustPlanPackagesProto>,
    #[prost(uint32, tag = "11")]
    cache_schema_version: u32,
    #[prost(string, tag = "13")]
    cache_profile: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct RustPlanPackagesProto {
    #[prost(string, repeated, tag = "1")]
    selected_package_ids: Vec<String>,
    #[prost(string, repeated, tag = "2")]
    workspace_package_ids: Vec<String>,
    #[prost(string, repeated, tag = "3")]
    excluded_path_package_ids: Vec<String>,
}

#[test]
fn cargo_front_door_invokes_zccache_rust_plan_when_target_cache_enabled() {
    let cache_root = unique_temp_dir("cargo-rust-plan-cache");
    let workspace = unique_temp_dir("cargo-rust-plan-workspace");
    let plan_cache = cache_root.join("target-artifact-cache");
    let log_path = cache_root.join("tool.log");
    let metadata_path = cache_root.join("metadata.json");
    let target_dir = workspace.join("target");
    fs::create_dir_all(workspace.join("app/src")).expect("create app source");
    fs::create_dir_all(&target_dir).expect("create target dir");
    fs::write(workspace.join("Cargo.lock"), "# lock\n").expect("write lockfile");
    fs::write(workspace.join("Cargo.toml"), "[workspace]\n").expect("write workspace manifest");
    fs::write(workspace.join("app/Cargo.toml"), "[package]\nname='app'\n")
        .expect("write app manifest");

    let metadata = serde_json::json!({
        "packages": [
            {
                "id": "path+file:///repo/app#app@0.1.0",
                "source": null
            },
            {
                "id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                "source": "registry+https://github.com/rust-lang/crates.io-index"
            }
        ],
        "workspace_members": ["path+file:///repo/app#app@0.1.0"],
        "workspace_root": workspace,
        "target_directory": target_dir
    });
    fs::write(&metadata_path, serde_json::to_string(&metadata).unwrap())
        .expect("write metadata fixture");

    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    let output = common::isolated_soldr_command()
        .args(["cargo", "build", "--locked"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_CARGO_METADATA_PATH", &metadata_path)
        .env("SOLDR_TRUST_INHERITED_ENV", "1")
        .env("SOLDR_TARGET_CACHE_MODE", "thin")
        .env("SOLDR_TARGET_CACHE_BUNDLE_DIR", &plan_cache)
        // setup-soldr exports SOLDR_TARGET_CACHE_PROFILE=thin-v1 today; this
        // test is asserting soldr's *own* default for the field, so clear
        // the env var so the runner-side override doesn't leak in.
        .env_remove("SOLDR_TARGET_CACHE_PROFILE")
        .output()
        .expect("failed to run soldr cargo build with rust-plan target cache");

    assert!(
        output.status.success(),
        "cache-enabled rust-plan front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // soldr#1368: rust-plan save/restore is now IN-PROCESS (no `zccache
    // rust-plan` subprocess), so the fake-zccache log no longer contains
    // restore/save lines. We instead assert the generated plan file the
    // front door writes before the in-process save runs.
    let zccache_session_dir = cache_root.join("cache").join("zccache");

    let plan_path = zccache_session_dir
        .join("plans")
        .join("last-rust-artifact-plan.pb");
    let plan_bytes = fs::read(&plan_path).expect("read generated rust plan");
    let plan = RustArtifactPlanProto::decode(plan_bytes.as_slice())
        .expect("parse generated rust plan protobuf");
    assert_eq!(plan.schema_version, 1);
    assert_eq!(plan.mode, 1);
    // Default profile flipped to thin-v2 in issue #461; cache_schema_version
    // bumps to 2 to signal the fingerprint-aware prune contract to zccache.
    assert_eq!(plan.cache_schema_version, 2);
    assert_eq!(plan.cache_profile, "thin-v2");
    let packages = plan.packages.expect("packages should be present");
    assert_eq!(
        packages.workspace_package_ids[0],
        "path+file:///repo/app#app@0.1.0"
    );
    assert!(
        packages.selected_package_ids[0].contains("serde"),
        "external dependency should be selected in generated plan: {:?}",
        packages.selected_package_ids
    );
}
