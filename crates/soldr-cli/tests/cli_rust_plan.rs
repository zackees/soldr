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

    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build", "--locked"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
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

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    let zccache_session_dir = cache_root.join("cache").join("zccache");
    assert!(
        log.contains("zccache rust-plan restore") && log.contains("zccache rust-plan save"),
        "soldr should call zccache rust-plan restore/save when target cache is enabled: {log}"
    );
    assert!(
        !log.contains("--private-daemon")
            && !log.contains("--daemon-name soldr-dev-")
            && !log.contains("daemon_namespace=soldr-dev-"),
        "rust-plan calls should use the default zccache daemon/session by default: {log}"
    );
    assert!(
        path_display_variants(&plan_cache).iter().any(|path| {
            log.contains(&format!("--cache-dir {path}"))
                || log.contains(&format!("--cache-dir \"{path}\""))
        }),
        "rust-plan calls should still pass the target artifact bundle path via --cache-dir: {log}"
    );
    assert!(
        log.find("zccache rust-plan restore") < log.find("cargo wrapper=")
            && log.find("cargo wrapper=") < log.find("zccache rust-plan save"),
        "rust-plan restore should run before Cargo and save should run after Cargo: {log}"
    );

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

#[test]
fn cargo_front_door_warns_when_rust_plan_restore_is_partial() {
    let cache_root = unique_temp_dir("cargo-rust-plan-partial-cache");
    let workspace = unique_temp_dir("cargo-rust-plan-partial-workspace");
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
            }
        ],
        "workspace_members": ["path+file:///repo/app#app@0.1.0"],
        "workspace_root": workspace,
        "target_directory": target_dir
    });
    fs::write(&metadata_path, serde_json::to_string(&metadata).unwrap())
        .expect("write metadata fixture");

    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build", "--locked"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_CARGO_METADATA_PATH", &metadata_path)
        .env("SOLDR_TRUST_INHERITED_ENV", "1")
        .env("SOLDR_TARGET_CACHE_MODE", "thin")
        .env("SOLDR_TARGET_CACHE_BUNDLE_DIR", &plan_cache)
        .env("SOLDR_TEST_RUST_PLAN_STALE", "1")
        .output()
        .expect("failed to run soldr cargo build with stale rust-plan restore");

    assert!(
        output.status.success(),
        "soldr should continue after a partial rust-plan restore so Cargo can still run\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rust-plan restore is partial"),
        "soldr should warn when zccache reports artifact_absent_from_restored_plan > 0: {stderr}"
    );
    assert!(
        stderr.contains("target-dir") || stderr.contains("--target-dir"),
        "warning should point users at the shared target-dir workaround: {stderr}"
    );
    assert!(
        stderr.contains("228"),
        "warning should reference issue #228 so users can find context: {stderr}"
    );
}

#[test]
fn cargo_front_door_recovers_from_stale_zccache_daemon_start() {
    let cache_root = unique_temp_dir("cargo-stale-zccache-daemon");
    let log_path = cache_root.join("tool.log");
    let stale_marker = cache_root.join("stale-zccache-start");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_ZCCACHE_STALE_START_ONCE", &stale_marker)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with stale fake zccache");

    assert!(
        output.status.success(),
        "cache-enabled front door should recover from stale zccache daemon\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert_eq!(
        log.matches("zccache start").count(),
        2,
        "managed zccache start should be retried once after stale daemon detection: {log}"
    );
    assert!(
        log.contains("zccache stop"),
        "managed zccache should stop stale daemon state before retrying: {log}"
    );
    assert!(
        log.contains("zccache session-start")
            && log.contains("zccache wrapper")
            && log.contains("zccache session-end test-session"),
        "recovered build should still exercise the normal managed zccache path: {log}"
    );
    assert!(
        stale_marker.with_extension("stopped").exists(),
        "fake zccache should only allow recovery after stop"
    );
}

#[test]
fn cargo_front_door_removes_stale_zccache_daemon_lock_before_retry() {
    let cache_root = unique_temp_dir("cargo-stale-zccache-daemon-lock");
    let zccache_session_dir = cache_root.join("cache").join("zccache");
    let log_path = cache_root.join("tool.log");
    let stale_marker = cache_root.join("stale-zccache-lock");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("ZCCACHE_CACHE_DIR", &zccache_session_dir)
        .env("SOLDR_TRUST_INHERITED_ENV", "1")
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE", &stale_marker)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with stale fake zccache lock");

    assert!(
        output.status.success(),
        "cache-enabled front door should remove stale zccache daemon.lock before retry\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert_eq!(
        log.matches("zccache start").count(),
        2,
        "managed zccache start should be retried once after stale lock detection: {log}"
    );
    assert!(
        log.contains("zccache stop"),
        "managed zccache should still stop stale daemon state before removing the lock: {log}"
    );
    assert!(
        log.contains("zccache session-start")
            && log.contains("zccache wrapper")
            && log.contains("zccache session-end test-session"),
        "recovered build should still exercise the normal managed zccache path: {log}"
    );
    assert!(
        !zccache_session_dir.join("daemon.lock").exists(),
        "soldr should remove stale zccache daemon.lock before retrying start"
    );
}
