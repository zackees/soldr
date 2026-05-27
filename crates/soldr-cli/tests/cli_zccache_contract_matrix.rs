//! Compact zccache integration contract matrix for issue #548.
//!
//! The narrow regression tests still own the details. This file locks the
//! cross-cutting contract that those details are supposed to compose into:
//! source resolution, managed daemon/session lifecycle, cargo env propagation,
//! rust-plan restore/save ordering, and command-lifetime shutdown.

#![allow(unused_imports)]

mod common;

use common::*;
use serde_json::Value;
use soldr_cli::timed_test;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

struct ContractFixture {
    cache_root: PathBuf,
    workspace: PathBuf,
    plan_cache: PathBuf,
    zccache_cache_dir: PathBuf,
    log_path: PathBuf,
    metadata_path: PathBuf,
    cargo: PathBuf,
    rustc: PathBuf,
    zccache: PathBuf,
}

fn seed_contract_fixture(label: &str) -> ContractFixture {
    let cache_root = unique_temp_dir(&format!("{label}-cache"));
    let workspace = unique_temp_dir(&format!("{label}-workspace"));
    let plan_cache = cache_root.join("target-artifact-cache");
    let zccache_cache_dir = cache_root.join("cache").join("zccache");
    let log_path = cache_root.join("tool.log");
    let metadata_path = cache_root.join("metadata.json");
    let target_dir = workspace.join("target");

    fs::create_dir_all(workspace.join(".git")).expect("create fake git root");
    fs::create_dir_all(workspace.join("app").join("src")).expect("create app source");
    fs::create_dir_all(&target_dir).expect("create target dir");
    fs::write(workspace.join("Cargo.lock"), "# lock\n").expect("write lockfile");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\n",
    )
    .expect("write workspace manifest");
    fs::write(
        workspace.join("app").join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write app manifest");
    fs::write(
        workspace.join("app").join("src").join("lib.rs"),
        "pub fn answer() -> i32 { 42 }\n",
    )
    .expect("write app source");

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
        .expect("write cargo metadata fixture");

    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);

    ContractFixture {
        cache_root,
        workspace,
        plan_cache,
        zccache_cache_dir,
        log_path,
        metadata_path,
        cargo,
        rustc,
        zccache,
    }
}

fn soldr_with_fake_toolchain(fixture: &ContractFixture) -> Command {
    let mut command = isolated_soldr_command();
    command
        .current_dir(&fixture.workspace)
        .env("SOLDR_CACHE_DIR", &fixture.cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &fixture.cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &fixture.rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &fixture.zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .env_remove("SOLDR_TARGET_CACHE_PROFILE");
    command
}

fn log_contains_path(log: &str, prefix: &str, path: &Path) -> bool {
    path_display_variants(path)
        .iter()
        .any(|variant| log.contains(&format!("{prefix}{variant}")))
}

fn assert_log_order(log: &str, left: &str, right: &str) {
    let left_pos = log
        .find(left)
        .unwrap_or_else(|| panic!("missing {left:?} in log:\n{log}"));
    let right_pos = log
        .find(right)
        .unwrap_or_else(|| panic!("missing {right:?} in log:\n{log}"));
    assert!(
        left_pos < right_pos,
        "expected {left:?} before {right:?} in log:\n{log}"
    );
}

fn assert_no_managed_zccache(log: &str) {
    for needle in [
        "zccache start",
        "zccache session-start",
        "zccache wrapper",
        "zccache rust-plan",
        "zccache session-end",
        "zccache stop",
    ] {
        assert!(
            !log.contains(needle),
            "managed zccache should be skipped but found {needle:?} in log:\n{log}"
        );
    }
}

timed_test!(
    managed_zccache_contract_matrix_covers_session_env_rust_plan_and_shutdown,
    Duration::from_secs(120),
    {
        let fixture = seed_contract_fixture("zccache-contract-matrix");
        let down_marker = fixture.cache_root.join("zccache-down");
        let output = soldr_with_fake_toolchain(&fixture)
            .args(["cargo", "build", "--locked"])
            .env("SOLDR_TEST_CARGO_METADATA_PATH", &fixture.metadata_path)
            .env("SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER", &down_marker)
            .env("SOLDR_TARGET_CACHE_MODE", "thin")
            .env("SOLDR_TARGET_CACHE_BUNDLE_DIR", &fixture.plan_cache)
            .env("SOLDR_CACHE_LIFECYCLE", "command")
            .env("SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS", "5")
            .output()
            .expect("run soldr cargo build");

        assert!(
            output.status.success(),
            "contract matrix build failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&fixture.log_path).expect("read fake tool log");
        assert_log_order(&log, "zccache start", "zccache session-start");
        assert_log_order(&log, "zccache session-start", "zccache rust-plan restore");
        assert_log_order(&log, "zccache rust-plan restore", "cargo wrapper=");
        assert_log_order(&log, "cargo wrapper=", "zccache wrapper");
        assert_log_order(&log, "zccache wrapper", "zccache rust-plan save");
        assert_log_order(
            &log,
            "zccache rust-plan save",
            "zccache session-end test-session --json",
        );
        assert_log_order(
            &log,
            "zccache session-end test-session --json",
            "zccache stop",
        );

        assert!(
            log_contains_owned_soldr_wrapper(&log, &fixture.cache_root),
            "soldr should own RUSTC_WRAPPER in managed mode:\n{log}"
        );
        assert!(
            log.contains("cache=1") && log.contains("session=test-session"),
            "cargo child should receive cache/session env:\n{log}"
        );
        assert!(
            log_contains_path(&log, "zccache_dir=", &fixture.zccache_cache_dir)
                && log_contains_path(&log, "cache_dir=", &fixture.zccache_cache_dir),
            "cargo and zccache should use the soldr-owned zccache cache dir:\n{log}"
        );
        assert!(
            log.contains("path_remap=auto"),
            "managed zccache should enable normalized path remap:\n{log}"
        );
        assert!(
            log_contains_path(&log, "worktree_root=", &fixture.workspace),
            "managed zccache should pass the git root as worktree root:\n{log}"
        );
        assert!(
            log_contains_path(&log, "--cache-dir ", &fixture.plan_cache),
            "rust-plan should receive the target artifact bundle dir:\n{log}"
        );

        let logs_dir = fixture.zccache_cache_dir.join("logs");
        let session_log = logs_dir.join("last-session.log");
        let journal = logs_dir.join("last-session.jsonl");
        let stats = logs_dir.join("last-session-stats.json");
        assert!(session_log.exists(), "missing {}", session_log.display());
        assert!(journal.exists(), "missing {}", journal.display());
        let stats_json: Value = serde_json::from_str(
            &fs::read_to_string(&stats)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", stats.display())),
        )
        .expect("parse session stats");
        assert_eq!(stats_json["status"], "ok");
        assert_eq!(stats_json["session_id"], "test-session");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("soldr: zccache session summary"),
            "session summary should remain visible to users:\n{stderr}"
        );
    }
);

timed_test!(
    disabled_and_non_build_paths_skip_managed_zccache,
    Duration::from_secs(120),
    {
        let no_cache = seed_contract_fixture("zccache-contract-no-cache");
        let no_cache_output = soldr_with_fake_toolchain(&no_cache)
            .args(["--no-cache", "cargo", "build", "--locked"])
            .output()
            .expect("run soldr --no-cache cargo build");
        assert!(
            no_cache_output.status.success(),
            "no-cache build failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&no_cache_output.stdout),
            String::from_utf8_lossy(&no_cache_output.stderr)
        );
        let no_cache_log = fs::read_to_string(&no_cache.log_path).expect("read no-cache log");
        assert!(
            no_cache_log.contains("cache=0"),
            "--no-cache should propagate SOLDR_CACHE_ENABLED=0:\n{no_cache_log}"
        );
        assert_no_managed_zccache(&no_cache_log);

        let metadata = seed_contract_fixture("zccache-contract-metadata");
        let metadata_output = soldr_with_fake_toolchain(&metadata)
            .args(["cargo", "metadata"])
            .output()
            .expect("run soldr cargo metadata");
        assert!(
            metadata_output.status.success(),
            "metadata command failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&metadata_output.stdout),
            String::from_utf8_lossy(&metadata_output.stderr)
        );
        let metadata_log = fs::read_to_string(&metadata.log_path).expect("read metadata log");
        assert!(
            metadata_log.contains("cache=0"),
            "non-build cargo should propagate SOLDR_CACHE_ENABLED=0:\n{metadata_log}"
        );
        assert_no_managed_zccache(&metadata_log);
    }
);
