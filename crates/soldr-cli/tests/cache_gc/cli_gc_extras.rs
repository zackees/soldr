#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// ---------------------------------------------------------------------------
// Issue #323 — soldr gc cargo / gc locations / gc sweep coverage.
// ---------------------------------------------------------------------------

#[test]
fn gc_cargo_help_lists_max_flags_and_dry_run() {
    let output = Command::new(common::soldr_bin())
        .args(["gc", "cargo", "--help"])
        .output()
        .expect("failed to run soldr gc cargo --help");
    assert!(output.status.success(), "soldr gc cargo --help failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--dry-run",
        "--toolchain",
        "--max-src-age",
        "--max-crate-age",
        "--max-index-age",
        "--max-git-co-age",
        "--max-git-db-age",
        "--max-download-age",
        "--max-src-size",
        "--max-crate-size",
        "--max-git-size",
        "--max-download-size",
        "--json",
    ] {
        assert!(
            stdout.contains(flag),
            "gc cargo --help missing {flag}: {stdout}"
        );
    }
}

#[test]
fn gc_cargo_rejects_unknown_flag() {
    let output = Command::new(common::soldr_bin())
        .args(["gc", "cargo", "--bogus-flag"])
        .output()
        .expect("failed to run soldr gc cargo --bogus-flag");
    assert!(!output.status.success(), "unknown flag should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("unrecognized"),
        "expected clap-style unknown-flag error, got: {stderr}"
    );
}

#[test]
fn gc_locations_json_emits_valid_schema_even_without_caches() {
    let cache_root = unique_temp_dir("gc-locations-json");
    let output = Command::new(common::soldr_bin())
        .args(["gc", "locations", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        // Disable auto-GC so the test isn't affected by background work.
        .env("SOLDR_AUTO_GC_DISABLED", "1")
        .output()
        .expect("failed to run soldr gc locations --json");

    assert!(
        output.status.success(),
        "gc locations failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("gc locations --json must be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "gc");
    assert_eq!(json["mode"], "locations");
    let locations = json["locations"]
        .as_array()
        .expect("locations must be an array");
    assert!(!locations.is_empty(), "should report at least soldr_cache");
    // The kinds we expect to always appear, even when paths don't exist.
    let kinds: Vec<&str> = locations
        .iter()
        .filter_map(|entry| entry["kind"].as_str())
        .collect();
    for required in [
        "cargo_registry_src",
        "cargo_registry_cache",
        "cargo_registry_index",
        "cargo_git_db",
        "cargo_git_checkouts",
        "cargo_global_cache",
        "rustup_toolchains",
        "rustup_update_hashes",
        "soldr_cache",
        "soldr_state_db",
    ] {
        assert!(
            kinds.contains(&required),
            "gc locations missing kind {required}: {kinds:?}"
        );
    }
    // Each entry must carry the schema fields.
    for entry in locations {
        for field in [
            "kind",
            "path",
            "exists",
            "size_bytes",
            "size_human",
            "file_count",
            "owner",
            "purge_safety",
        ] {
            assert!(
                entry.get(field).is_some(),
                "gc locations entry missing field {field}: {entry}"
            );
        }
    }
}

#[test]
fn gc_sweep_no_cargo_dry_run_runs_end_to_end_without_changes() {
    let cache_root = unique_temp_dir("gc-sweep-dryrun");
    let target = seed_gc_candidate(&cache_root, "sweep-dryrun");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "sweep", "--no-cargo-gc", "--dry-run", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_AUTO_GC_DISABLED", "1")
        .output()
        .expect("failed to run soldr gc sweep --no-cargo-gc --dry-run --json");

    assert!(
        output.status.success(),
        "gc sweep dry-run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        target.exists(),
        "dry-run sweep must not delete {}",
        target.display()
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc sweep --json must be JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "gc");
    assert_eq!(json["mode"], "sweep");
    assert_eq!(json["dry_run"], true);
    assert!(
        json["cargo_gc"].is_null(),
        "cargo_gc must be null when --no-cargo-gc"
    );
    assert!(
        json["soldr_targets"].is_null(),
        "dry-run sweep must not run soldr target purge"
    );
    assert!(json["locations"].is_array());
}
