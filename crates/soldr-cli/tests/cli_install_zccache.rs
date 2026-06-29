//! Integration tests for `soldr install-zccache` at the CLI surface.
//! Covers argv parsing, JSON output, mutual-exclusion errors, and the
//! round-trip from `install` → `--status` → `--remove`.

#![allow(unused_imports)]

mod common;

use common::unique_temp_dir;
use serde_json::Value;
use soldr_cli::fetch::MANAGED_ZCCACHE_VERSION;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn write_fake_binary(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn seed_source_dir(root: &Path) -> PathBuf {
    let src = root.join("zccache-src");
    write_fake_binary(&src, &bin_name("zccache"), b"cli-bytes");
    write_fake_binary(&src, &bin_name("zccache-daemon"), b"daemon-bytes");
    write_fake_binary(&src, &bin_name("zccache-fp"), b"fp-bytes");
    src
}

/// Per-test isolated home directory used to anchor the pinned-zccache
/// install (issue #426 — `paths.pinned_bin` lives under `$HOME/.soldr/bin/`
/// regardless of `SOLDR_CACHE_DIR`). Without this, every test in this
/// binary would write into the same `$HOME/.soldr/bin/zccache-pinned/`
/// at the same time when cargo runs them in parallel — Linux CI surfaces
/// the race as "Directory not empty (os error 39)" during `remove_dir_all`
/// + `create_dir_all` in `install_zccache_from_source`.
fn unique_home_dir(label: &str) -> PathBuf {
    let home = unique_temp_dir(&format!("{label}-home"));
    // pre-create .soldr/bin to match what `SoldrPaths::ensure_dirs()` would
    // do on a fresh production install; keeps test setup mirroring reality.
    fs::create_dir_all(home.join(".soldr").join("bin")).expect("seed home/.soldr/bin");
    home
}

/// Path the pin lands at under the per-test isolated home dir.
fn pinned_dir_in(home_root: &Path) -> PathBuf {
    home_root.join(".soldr").join("bin").join("zccache-pinned")
}

#[test]
fn install_zccache_from_directory_writes_sidecar() {
    let tmp = unique_temp_dir("install-zccache-dir");
    let cache_root = tmp.join("soldr-root");
    let home_root = unique_home_dir("install-zccache-dir");
    let src = seed_source_dir(&tmp);

    let output = Command::new(common::soldr_bin())
        .args(["install-zccache"])
        .arg(&src)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("failed to run install-zccache");

    assert!(
        output.status.success(),
        "install-zccache failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The pin is home-anchored (issue #426), not under SOLDR_CACHE_DIR.
    let pinned = pinned_dir_in(&home_root);
    assert!(pinned.join(bin_name("zccache")).exists());
    assert!(pinned.join(bin_name("zccache-daemon")).exists());
    assert!(pinned.join(bin_name("zccache-fp")).exists());
    assert!(pinned.join("source.json").exists());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("install dir:"),
        "expected human output: {stdout}"
    );
    assert!(stdout.contains("source:"), "expected source line: {stdout}");
}

#[test]
fn install_zccache_json_round_trip() {
    let tmp = unique_temp_dir("install-zccache-json");
    let cache_root = tmp.join("soldr-root");
    let home_root = unique_home_dir("install-zccache-json");
    let src = seed_source_dir(&tmp);

    // install --json
    let install = Command::new(common::soldr_bin())
        .args(["install-zccache", "--json"])
        .arg(&src)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache --json");
    assert!(install.status.success());
    let install_json: Value =
        serde_json::from_slice(&install.stdout).expect("install --json should emit valid JSON");
    assert_eq!(install_json["command"], "install-zccache");
    assert_eq!(install_json["source_kind"], "path");
    assert_eq!(
        install_json["binaries"]["zccache"]["size_bytes"],
        b"cli-bytes".len()
    );

    // --status --json
    let status = Command::new(common::soldr_bin())
        .args(["install-zccache", "--status", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache --status --json");
    assert!(status.status.success());
    let status_json: Value =
        serde_json::from_slice(&status.stdout).expect("status --json should emit valid JSON");
    assert_eq!(status_json["command"], "install-zccache --status");
    assert_eq!(status_json["pinned"]["source_kind"], "path");
    assert_eq!(status_json["managed_version"], MANAGED_ZCCACHE_VERSION);
    assert!(
        status_json["drift_from_managed"].is_boolean(),
        "drift_from_managed must be a bool"
    );

    // --remove --json
    let remove = Command::new(common::soldr_bin())
        .args(["install-zccache", "--remove", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache --remove --json");
    assert!(remove.status.success());
    let remove_json: Value = serde_json::from_slice(&remove.stdout).expect("remove --json");
    assert_eq!(remove_json["removed"], true);

    // Second remove is idempotent: removed=false.
    let remove2 = Command::new(common::soldr_bin())
        .args(["install-zccache", "--remove", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache --remove --json (second)");
    assert!(remove2.status.success());
    let remove2_json: Value = serde_json::from_slice(&remove2.stdout).expect("remove --json 2");
    assert_eq!(remove2_json["removed"], false);
}

#[test]
fn install_zccache_status_with_no_install_reports_managed_default() {
    let tmp = unique_temp_dir("install-zccache-status-empty");
    let cache_root = tmp.join("soldr-root");
    fs::create_dir_all(&cache_root).unwrap();
    let home_root = unique_home_dir("install-zccache-status-empty");

    let output = Command::new(common::soldr_bin())
        .args(["install-zccache", "--status", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("status with no install");
    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).expect("status --json");
    assert!(json["pinned"].is_null(), "pinned should be null: {json}");
    assert_eq!(json["managed_version"], MANAGED_ZCCACHE_VERSION);
}

#[test]
fn install_zccache_no_source_no_flags_errors() {
    let tmp = unique_temp_dir("install-zccache-empty-args");
    let cache_root = tmp.join("soldr-root");
    let home_root = unique_home_dir("install-zccache-empty-args");

    let output = Command::new(common::soldr_bin())
        .args(["install-zccache"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache (no args)");
    assert!(!output.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SOURCE") || stderr.contains("--remove") || stderr.contains("--status"),
        "stderr should describe the missing input: {stderr}"
    );
}

#[test]
fn install_zccache_mutually_exclusive_flags_rejected() {
    let tmp = unique_temp_dir("install-zccache-mutex");
    let cache_root = tmp.join("soldr-root");
    let home_root = unique_home_dir("install-zccache-mutex");

    // clap should reject `--remove` + `--status` outright.
    let output = Command::new(common::soldr_bin())
        .args(["install-zccache", "--remove", "--status"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache --remove --status");
    assert!(
        !output.status.success(),
        "expected mutual-exclusion failure"
    );

    // SOURCE + --remove is also rejected.
    let output = Command::new(common::soldr_bin())
        .args(["install-zccache", "system", "--remove"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache SOURCE --remove");
    assert!(
        !output.status.success(),
        "expected mutual-exclusion failure (SOURCE + --remove)"
    );
}

#[test]
fn install_zccache_unknown_extension_errors() {
    let tmp = unique_temp_dir("install-zccache-unknown-ext");
    let cache_root = tmp.join("soldr-root");
    let home_root = unique_home_dir("install-zccache-unknown-ext");
    let bogus = tmp.join("zccache.7z");
    fs::write(&bogus, b"junk").unwrap();

    let output = Command::new(common::soldr_bin())
        .args(["install-zccache"])
        .arg(&bogus)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .output()
        .expect("install-zccache with bogus archive");
    assert!(!output.status.success(), "expected unknown-ext failure");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(".zip") && stderr.contains(".tar.gz") && stderr.contains(".tar.zst"),
        "stderr should list supported extensions: {stderr}"
    );
}
