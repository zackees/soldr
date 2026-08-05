#![allow(unused_imports)]

mod common;

use common::*;
use serde_json::Value;
use soldr_cli::timed_test;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn gc_summary_surfaces_the_linked_worktree_total() {
    // soldr#2134 wants merged-worktree targets reclaimed eagerly. Deleting
    // outside disk pressure is a wider change; surfacing the total is what
    // lets someone act before a build blocks, which is the same benefit
    // without widening what gets deleted.
    let cache_root = unique_temp_dir("gc-worktree-total");
    let plain = seed_gc_candidate(&cache_root, "primary-checkout");
    let worktree = seed_gc_worktree_candidate(&cache_root, "linked-worktree");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc");
    assert!(output.status.success());
    assert!(
        plain.exists() && worktree.exists(),
        "gc summary must not delete"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("in linked worktree"),
        "summary must surface the worktree total: {stdout}"
    );
    // Exactly one of the two seeded targets is a linked worktree; the plain
    // one holds no `.git` at all, so it must not be counted.
    assert!(
        stdout.contains("1 in linked worktree"),
        "only the worktree-backed target counts: {stdout}"
    );
    assert!(
        stdout.contains("[linked worktree]"),
        "and it is marked in the eviction-order list: {stdout}"
    );
}

#[test]
fn gc_summary_is_non_destructive_and_lists_largest_candidates() {
    let cache_root = unique_temp_dir("gc-summary");
    let target = seed_gc_candidate(&cache_root, "summary-project");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc");

    assert!(
        output.status.success(),
        "gc summary failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        target.exists(),
        "soldr gc summary must not delete {}",
        target.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reclaimable:"),
        "summary should include total reclaimable bytes: {stdout}"
    );
    assert!(
        stdout.contains("next to be reclaimed (eviction order)"),
        "summary should list candidates in the order purge will take them: {stdout}"
    );
    assert!(
        stdout.contains("Run 'soldr gc purge'"),
        "summary should point to destructive purge command: {stdout}"
    );
}

#[test]
fn gc_summary_json_reports_candidates_without_deleting() {
    let cache_root = unique_temp_dir("gc-summary-json");
    let target = seed_gc_candidate(&cache_root, "summary-json-project");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "--json", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc --json");

    assert!(
        output.status.success(),
        "gc summary json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        target.exists(),
        "soldr gc --json summary must not delete {}",
        target.display()
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc --json must be JSON");
    assert_eq!(json["schema_version"], 3);
    assert_eq!(json["command"], "gc");
    assert_eq!(json["mode"], "summary");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["candidate_count"], 1);
    assert!(json["total_reclaimable_bytes"].as_u64().unwrap_or(0) > 0);
    assert_eq!(json["largest_candidates"].as_array().unwrap().len(), 1);
    assert_eq!(json["deleted_paths"].as_array().unwrap().len(), 0);
    let cand = &json["largest_candidates"].as_array().unwrap()[0];
    assert_eq!(cand["kind"].as_str(), Some("cargo_target"));
    assert_eq!(cand["purge_safety"].as_str(), Some("derived"));
}

#[test]
fn gc_purge_all_deletes_candidates_without_prompt() {
    let cache_root = unique_temp_dir("gc-purge-all");
    let target = seed_gc_candidate(&cache_root, "purge-project");

    let output = Command::new(common::soldr_bin())
        .args([
            "gc",
            "purge",
            "--all",
            "--older-than",
            "1s",
            "--larger-than",
            "1B",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc purge --all");

    assert!(
        output.status.success(),
        "gc purge --all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !target.exists(),
        "soldr gc purge --all should delete {}",
        target.display()
    );
}

#[test]
fn gc_purge_enter_accepts_candidate() {
    let cache_root = unique_temp_dir("gc-purge-enter");
    let target = seed_gc_candidate(&cache_root, "purge-enter-project");

    let mut child = Command::new(common::soldr_bin())
        .args(["gc", "purge", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn soldr gc purge");
    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(b"\n")
        .expect("failed to accept prompt");
    let output = child
        .wait_with_output()
        .expect("failed to wait for soldr gc purge");

    assert!(
        output.status.success(),
        "gc purge interactive failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !target.exists(),
        "Enter should accept and delete {}",
        target.display()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[Y/n]"),
        "prompt should advertise default yes: {stderr}"
    );
    assert!(
        stderr.contains("selected 1; succeeded 1; failed 0"),
        "final summary should include aggregate counts: {stderr}"
    );
}

#[test]
fn gc_purge_all_json_reports_error_log_path_and_keeps_failed_row() {
    let cache_root = unique_temp_dir("gc-purge-json-failure");
    let target = seed_gc_file_candidate(&cache_root, "purge-json-failure-project");

    let output = Command::new(common::soldr_bin())
        .args([
            "gc",
            "purge",
            "--all",
            "--json",
            "--older-than",
            "1s",
            "--larger-than",
            "1B",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc purge --all --json");

    assert!(
        output.status.success(),
        "gc purge --all --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("gc purge --json must be JSON");
    assert_eq!(json["mode"], "purge");
    assert_eq!(json["selected_count"], 1);
    assert_eq!(json["succeeded_count"], 0);
    assert_eq!(json["failed_count"], 1);
    let log_path = PathBuf::from(
        json["error_log_path"]
            .as_str()
            .expect("failure JSON should include error log path"),
    );
    assert!(
        log_path.exists(),
        "missing gc error log {}",
        log_path.display()
    );

    let registry =
        soldr_cli::cache_lib::target_registry::TargetRegistry::open(&cache_root.join("state.redb"))
            .expect("failed to open target registry");
    assert!(
        registry.get(&target).unwrap().is_some(),
        "failed deletion row should remain retryable"
    );
}

timed_test!(
    gc_list_json_reports_built_project_target_dir,
    Duration::from_secs(60),
    {
        let cache_root = unique_temp_dir("gc-list-build");
        let project_dir = unique_temp_dir("gc-list-project");
        // #323 slice 2: sandbox CARGO_HOME so the registry_src walker
        // doesn't see the developer's real `~/.cargo` and inject
        // cargo_registry_src entries into this test's assertions. Also
        // sandbox RUSTUP_HOME now that `gc list` reports rustup toolchains.
        let sandbox_cargo_home = unique_temp_dir("gc-list-build-cargo-home");
        let sandbox_rustup_home = unique_temp_dir("gc-list-build-rustup-home");

        fs::write(
        project_dir.join("Cargo.toml"),
        "[package]\nname = \"gc_list_demo\"\nversion = \"0.0.1\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("failed to write Cargo.toml");
        fs::create_dir_all(project_dir.join("src")).expect("failed to create src dir");
        fs::write(project_dir.join("src/main.rs"), "fn main() {}\n")
            .expect("failed to write main.rs");

        let soldr_bin = common::soldr_bin();
        let cargo = rustup_which("cargo");

        let start = Command::new(&soldr_bin)
            .args(["daemon", "start"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .output()
            .expect("start daemon for wrapper target registry");
        assert!(
            start.status.success(),
            "daemon start failed: {}",
            String::from_utf8_lossy(&start.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = Command::new(&soldr_bin)
                .args(["daemon", "status", "--json"])
                .env("SOLDR_CACHE_DIR", &cache_root)
                .output()
                .expect("query daemon status");
            if status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout)
                    .ok()
                    .and_then(|body| body["running"].as_bool())
                    .unwrap_or(false)
            {
                break;
            }
            assert!(Instant::now() < deadline, "daemon did not become ready");
            std::thread::sleep(Duration::from_millis(50));
        }

        let build = Command::new(&cargo)
            .args(["build", "--quiet"])
            .current_dir(&project_dir)
            .env("RUSTC_WRAPPER", &soldr_bin)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("ZCCACHE_DISABLE", "1")
            .env("SOLDR_CACHE_ENABLED", "0")
            .env_remove("CARGO_TARGET_DIR")
            // This fixture intentionally exercises soldr as a plain
            // RUSTC_WRAPPER, not as a child of the outer `soldr cargo test`
            // session that may be running this integration test.
            .env_remove(soldr_cli::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
            .env_remove(soldr_cli::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR)
            // Cache-disable keeps this registry fixture on direct rustc after the
            // wrapper records its target directory.
            .env_remove("SOLDR_TEST_ZCCACHE_BIN")
            .env_remove("ZCCACHE_BINARY")
            .output()
            .expect("failed to run cargo build through soldr wrapper");

        assert!(
            build.status.success(),
            "cargo build failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
        let target_dir = project_dir.join("target");
        assert!(target_dir.exists(), "target/ should exist after build");

        let canonical_target = fs::canonicalize(&target_dir).unwrap_or_else(|_| target_dir.clone());

        let output = Command::new(&soldr_bin)
            .args(["gc", "list", "--json"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("CARGO_HOME", &sandbox_cargo_home)
            .env("RUSTUP_HOME", &sandbox_rustup_home)
            .output()
            .expect("failed to run soldr gc list --json");

        assert!(
            output.status.success(),
            "gc list --json failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let json: Value =
            serde_json::from_slice(&output.stdout).expect("gc list --json must be JSON");
        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["command"], "gc");
        assert_eq!(json["mode"], "list");
        let entry_count = json["entry_count"].as_u64().expect("entry_count");
        assert!(entry_count >= 1, "expected at least one tracked target dir");

        let entries = json["entries"].as_array().expect("entries array");
        let canonical_str = canonical_target.display().to_string();
        let target_str = target_dir.display().to_string();
        let matched = entries.iter().find(|e| {
            let p = e["path"].as_str().unwrap_or("");
            let pb = PathBuf::from(p);
            if p == canonical_str || p == target_str || pb == canonical_target || pb == target_dir {
                return true;
            }
            if let Ok(canon_p) = fs::canonicalize(&pb) {
                return canon_p == canonical_target;
            }
            false
        });
        let entry = matched.unwrap_or_else(|| {
            panic!(
                "target dir {} not found in gc list entries: {}",
                canonical_target.display(),
                serde_json::to_string_pretty(entries).unwrap()
            )
        });

        let path_str = entry["path"].as_str().expect("entry path");
        assert_eq!(entry["kind"].as_str(), Some("cargo_target"));
        assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
        assert!(
            PathBuf::from(path_str).is_absolute(),
            "gc list entries must use absolute paths: {path_str}"
        );
        let size_bytes = entry["size_bytes"].as_u64().expect("size_bytes");
        assert!(size_bytes > 0, "built target/ should have non-zero size");
        let file_count = entry["file_count"].as_u64().expect("file_count");
        assert!(file_count > 0, "built target/ should contain files");

        for entry in entries {
            assert!(
                entry["path"].is_string(),
                "every entry must have a path string"
            );
            assert!(
                entry["size_bytes"].is_u64(),
                "every entry must have size_bytes"
            );
            assert!(
                entry["file_count"].is_u64(),
                "every entry must have file_count"
            );
            assert!(
                entry.get("exists").is_none(),
                "missing rows are pruned, so `exists` should not appear on listed entries"
            );
            let kind = entry["kind"].as_str().expect("every entry must have kind");
            assert!(
                matches!(
                    kind,
                    "cargo_target"
                        | "cargo_target_incremental"
                        | "cargo_target_build_script_binaries"
                        | "cargo_target_doc"
                        | "cargo_target_subcommand_caches"
                ),
                "unexpected gc list kind: {kind}"
            );
            assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
        }

        let stop = Command::new(&soldr_bin)
            .args(["daemon", "stop"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .output()
            .expect("stop explicit daemon");
        assert!(
            stop.status.success(),
            "daemon stop failed: {}",
            String::from_utf8_lossy(&stop.stderr)
        );
    }
);

#[test]
fn gc_list_json_entries_include_kind_and_purge_safety_defaults() {
    let cache_root = unique_temp_dir("gc-list-kind-defaults");
    let target = seed_gc_candidate(&cache_root, "kind-defaults-project");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc list --json");
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema_version"], 3);

    let entries = json["entries"].as_array().expect("entries");
    let entry = entries
        .iter()
        .find(|e| e["path"].as_str().is_some_and(|p| target == Path::new(p)))
        .expect("seeded target missing from entries");
    assert_eq!(entry["kind"].as_str(), Some("cargo_target"));
    assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
    // soldr#2134: eviction ranks worktree targets ahead of primary
    // checkouts, so the report has to say which one an entry is or the
    // resulting order is unexplainable. A plain seeded workspace is not
    // a linked worktree.
    assert_eq!(
        entry["in_worktree"].as_bool(),
        Some(false),
        "cargo_target entries must carry the tier that decides their eviction order"
    );
}

#[test]
fn gc_list_json_prunes_missing_registry_rows_in_one_pass() {
    let cache_root = unique_temp_dir("gc-list-prune");
    let dev_root = cache_root.join("dev-root");
    // #323 slice 2: sandbox CARGO_HOME so the registry_src walker
    // doesn't contribute extra entries to entry_count assertions. Also
    // sandbox RUSTUP_HOME now that `gc list` reports rustup toolchains.
    let sandbox_cargo_home = unique_temp_dir("gc-list-prune-cargo-home");
    let sandbox_rustup_home = unique_temp_dir("gc-list-prune-rustup-home");

    let live_workspace = dev_root.join("live-project");
    let live_target = live_workspace.join("target");
    fs::create_dir_all(&live_target).expect("failed to create live target dir");
    fs::write(live_target.join("artifact.bin"), b"keep me").expect("failed to seed live target");

    let missing_target = dev_root.join("ghost-project").join("target");

    {
        let registry = soldr_cli::cache_lib::target_registry::TargetRegistry::open(
            &cache_root.join("state.redb"),
        )
        .expect("failed to open target registry");
        let now = soldr_cli::cache_lib::target_registry::current_unix_seconds()
            .expect("failed to read clock for seeding");
        registry
            .upsert_with_time(&live_target, now - 30)
            .expect("failed to seed live row");
        registry
            .upsert_with_time(&missing_target, now - 30)
            .expect("failed to seed missing row");
        assert!(
            registry.get(&missing_target).unwrap().is_some(),
            "missing-row seed precondition"
        );
    }

    let output = Command::new(common::soldr_bin())
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &sandbox_cargo_home)
        .env("RUSTUP_HOME", &sandbox_rustup_home)
        .output()
        .expect("failed to run soldr gc list --json");
    assert!(
        output.status.success(),
        "gc list --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list must be JSON");
    assert_eq!(json["mode"], "list");
    assert_eq!(json["entry_count"], 1);
    assert_eq!(json["pruned_missing"], 1);

    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "missing entry must not be reported");
    let missing_str = missing_target.display().to_string();
    for entry in entries {
        let p = entry["path"].as_str().unwrap_or("");
        assert_ne!(p, missing_str, "missing path leaked into output");
    }

    let registry_after =
        soldr_cli::cache_lib::target_registry::TargetRegistry::open(&cache_root.join("state.redb"))
            .expect("failed to reopen registry");
    assert!(
        registry_after.get(&missing_target).unwrap().is_none(),
        "missing row should be batched out of the registry"
    );
    assert!(
        registry_after.get(&live_target).unwrap().is_some(),
        "live row must be preserved"
    );
}

#[test]
fn gc_flat_all_is_rejected_with_purge_hint() {
    let output = Command::new(common::soldr_bin())
        .args(["gc", "--all"])
        .env("SOLDR_CACHE_DIR", unique_temp_dir("gc-flat-all"))
        .output()
        .expect("failed to run soldr gc --all");

    assert!(
        !output.status.success(),
        "legacy flat gc --all must not silently delete"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("soldr gc purge --all"),
        "error should point to the new purge command: {stderr}"
    );
}
