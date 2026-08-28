//! Integration tests for report-only primary GC kinds (#323 slice 5).

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write_file(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create parent dir");
    }
    fs::write(path, body).expect("failed to write fixture file");
}

fn seed_report_only_roots(label: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let cache_root = unique_temp_dir(&format!("{label}-soldr-cache"));
    let cargo_home = unique_temp_dir(&format!("{label}-cargo-home"));
    let rustup_home = unique_temp_dir(&format!("{label}-rustup-home"));

    let registry_cache =
        cargo_home.join("registry/cache/index.crates.io-abc123/serde-1.0.213.crate");
    write_file(&registry_cache, b"crate tarball");

    let git_db = cargo_home.join("git/db/tokio-abc123");
    write_file(&git_db.join("objects/pack/pack.bin"), b"git db");

    let installed_bin = cargo_home.join("bin/cargo-demo.exe");
    write_file(&installed_bin, b"binary");

    let rustup_toolchain = rustup_home.join("toolchains/1.94.1-test");
    write_file(&rustup_toolchain.join("bin/rustc.exe"), b"rustc");

    (cache_root, cargo_home, rustup_home, registry_cache)
}

#[test]
fn gc_list_json_reports_primary_kinds_without_purge_eligibility() {
    let (cache_root, cargo_home, rustup_home, _registry_cache) =
        seed_report_only_roots("gc-report-only-list");

    let output = Command::new(common::soldr_bin())
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .output()
        .expect("failed to run soldr gc list --json");

    assert!(
        output.status.success(),
        "gc list failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list must be JSON");
    let entries = json["entries"].as_array().expect("entries");
    for kind in [
        "cargo_registry_cache",
        "cargo_git_db",
        "cargo_installed_binaries",
        "rustup_toolchain",
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry["kind"].as_str() == Some(kind))
            .unwrap_or_else(|| panic!("missing {kind}: {entries:?}"));
        assert_eq!(entry["purge_safety"].as_str(), Some("primary"));
        assert!(entry["size_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(entry["file_count"].as_u64().unwrap_or(0) > 0);
    }

    let registry_entry = entries
        .iter()
        .find(|entry| entry["kind"].as_str() == Some("cargo_registry_cache"))
        .expect("registry cache entry");
    assert_eq!(
        registry_entry["owner_crate"].as_str(),
        Some("serde@1.0.213")
    );

    let toolchain_entry = entries
        .iter()
        .find(|entry| entry["kind"].as_str() == Some("rustup_toolchain"))
        .expect("rustup toolchain entry");
    assert_eq!(
        toolchain_entry["owner_toolchain"].as_str(),
        Some("1.94.1-test")
    );
}

#[test]
fn gc_purge_report_only_kind_is_rejected_and_preserves_files() {
    let (cache_root, cargo_home, rustup_home, registry_cache) =
        seed_report_only_roots("gc-report-only-purge");
    assert!(registry_cache.exists());

    let output = Command::new(common::soldr_bin())
        .args([
            "gc",
            "purge",
            "--kind",
            "cargo_registry_cache",
            "--all",
            "--json",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", &rustup_home)
        .output()
        .expect("failed to run soldr gc purge report-only kind");

    assert!(
        !output.status.success(),
        "report-only kind must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("report-only"),
        "stderr should explain report-only safety: {stderr}"
    );
    assert!(
        registry_cache.exists(),
        "report-only purge must not delete {}",
        registry_cache.display()
    );
}
