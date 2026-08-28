use crate::common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, bytes).expect("write file");
}

fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = common::unique_temp_dir(name);
    let ws = root.join("workspace");
    let cache = root.join("cache");
    write(&ws.join("Cargo.toml"), b"[package]\nname=\"x\"\n");
    write(&ws.join("src/lib.rs"), b"pub fn x() {}\n");
    write(&cache.join("ab/cd/object.bin"), b"warm-cache-payload");
    write(&cache.join("logs/session.log"), b"runtime log");
    write(&cache.join("compile.lock"), b"lock");
    (ws, cache, root.join("cache.tar.zst"))
}

fn run_command(mut command: Command, context: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("{context}: {err}"));
    assert!(
        output.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn soldr_command(args: &[&str]) -> Command {
    let mut command = common::isolated_soldr_command();
    command.args(args);
    command
}

fn create_real_crate(dir: &Path) {
    write(
        &dir.join("Cargo.toml"),
        br#"[package]
name = "save_ci_real_hits"
version = "0.1.0"
edition = "2021"
"#,
    );
    write(
        &dir.join("src/main.rs"),
        br#"fn main() {
    println!("{}", save_ci_real_hits());
}

fn save_ci_real_hits() -> u32 {
    (0..32).sum()
}
"#,
    );
}

fn read_json_file(path: &Path) -> Value {
    let raw =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    serde_json::from_str(raw.trim())
        .unwrap_or_else(|err| panic!("parse {}: {err}\n{raw}", path.display()))
}

fn u64_field(json: &Value, key: &str) -> u64 {
    json.get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing numeric {key} in {json:#?}"))
}

#[test]
fn save_ci_json_reports_profile_and_exclusions() {
    let (ws, cache, archive) = fixture("save-ci-json");
    let output = Command::new(common::soldr_bin())
        .args(["save", "--ci", "--json", "--zstd-level", "1"])
        .arg("--cache-dir")
        .arg(&cache)
        .arg("--workspace")
        .arg(&ws)
        .arg("--out")
        .arg(&archive)
        .output()
        .expect("run soldr save --ci --json");

    assert!(
        output.status.success(),
        "soldr save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse save json");
    assert_eq!(json["profile"], "ci");
    assert_eq!(json["cache_files"], 1);
    assert_eq!(json["excluded_files"], 2);
    assert!(json["excluded_bytes"].as_u64().unwrap() > 0);
    assert!(json["archive_bytes"].as_u64().unwrap() > 0);
    assert_eq!(json["mtimes_only"], false);
    assert!(archive.exists());
}

#[test]
fn save_minimal_alias_selects_ci_profile() {
    let (ws, cache, archive) = fixture("save-minimal-json");
    let output = Command::new(common::soldr_bin())
        .args(["save", "--minimal", "--json", "--zstd-level", "1"])
        .arg("--cache-dir")
        .arg(&cache)
        .arg("--workspace")
        .arg(&ws)
        .arg("--out")
        .arg(&archive)
        .output()
        .expect("run soldr save --minimal --json");

    assert!(
        output.status.success(),
        "soldr save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse save json");
    assert_eq!(json["profile"], "ci");
    assert_eq!(json["cache_files"], 1);
    assert_eq!(json["excluded_files"], 2);
}

#[test]
fn hydrate_primary_and_load_alias_restore_the_same_archive() {
    let (ws, cache, archive) = fixture("hydrate-alias");
    let root = archive.parent().expect("archive parent");
    let hydrated = root.join("hydrated");
    let loaded = root.join("loaded");

    let mut save = soldr_command(&["save", "--json", "--zstd-level", "1"]);
    save.arg("--cache-dir")
        .arg(&cache)
        .arg("--workspace")
        .arg(&ws)
        .arg("--out")
        .arg(&archive);
    run_command(save, "soldr save hydrate fixture");

    for (verb, destination) in [("hydrate", &hydrated), ("load", &loaded)] {
        let mut restore = soldr_command(&[verb, "--json"]);
        restore
            .arg("--archive")
            .arg(&archive)
            .arg("--cache-dir")
            .arg(destination)
            .arg("--workspace")
            .arg(&ws);
        run_command(restore, &format!("soldr {verb} archive"));
    }

    assert_eq!(
        fs::read(hydrated.join("ab/cd/object.bin")).expect("read hydrated payload"),
        fs::read(loaded.join("ab/cd/object.bin")).expect("read load-alias payload"),
    );
}

#[test]
fn save_profile_env_selects_ci_when_flag_absent() {
    let (ws, cache, archive) = fixture("save-ci-env-json");
    let output = Command::new(common::soldr_bin())
        .env("SOLDR_SAVE_PROFILE", "minimal")
        .args(["save", "--json", "--zstd-level", "1"])
        .arg("--cache-dir")
        .arg(&cache)
        .arg("--workspace")
        .arg(&ws)
        .arg("--out")
        .arg(&archive)
        .output()
        .expect("run soldr save with SOLDR_SAVE_PROFILE=minimal");

    assert!(
        output.status.success(),
        "soldr save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse save json");
    assert_eq!(json["profile"], "ci");
    assert_eq!(json["cache_files"], 1);
    assert_eq!(json["excluded_files"], 2);
}

#[test]
#[ignore = "does two real soldr cargo builds; run explicitly for #1297 acceptance"]
fn save_ci_load_preserves_real_warm_rustc_hits() {
    let root = common::unique_temp_dir("save-ci-real-hits");
    let workspace = root.join("workspace");
    let cold_root = root.join("cold-cache-root");
    let warm_root = root.join("warm-cache-root");
    let archive = root.join("cache.tar.zst");
    let cold_cache = cold_root.join("cache");
    let warm_cache = warm_root.join("cache");

    create_real_crate(&workspace);
    fs::create_dir_all(&cold_cache).expect("create cold cache");
    fs::create_dir_all(&warm_cache).expect("create warm cache");

    let mut cold_build = soldr_command(&["cargo", "build", "--release"]);
    cold_build
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &cold_root);
    run_command(cold_build, "cold soldr cargo build");

    let mut flush = soldr_command(&["cache", "flush", "--json"]);
    flush.env("SOLDR_CACHE_DIR", &cold_root);
    run_command(flush, "cold cache flush");

    let mut shutdown = soldr_command(&["cache", "shutdown", "--no-wait", "--json"]);
    shutdown.env("SOLDR_CACHE_DIR", &cold_root);
    run_command(shutdown, "cold cache shutdown");

    write(
        &cold_cache.join("zccache/runtime-binaries/zccache"),
        b"runtime binary must not enter ci archive",
    );

    let mut save = soldr_command(&["save", "--ci", "--json", "--zstd-level", "1"]);
    save.arg("--cache-dir")
        .arg(&cold_cache)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--out")
        .arg(&archive);
    let save_output = run_command(save, "soldr save --ci");
    let save_json: Value = serde_json::from_slice(&save_output.stdout).expect("parse save json");
    assert_eq!(save_json["profile"], "ci");
    assert!(
        u64_field(&save_json, "cache_files") > 0,
        "ci save must include real cache payloads: {save_json:#?}"
    );
    assert!(
        u64_field(&save_json, "excluded_files") > 0,
        "ci save should report excluded runtime files: {save_json:#?}"
    );

    let mut load = soldr_command(&["load", "--json"]);
    load.arg("--archive")
        .arg(&archive)
        .arg("--cache-dir")
        .arg(&warm_cache)
        .arg("--workspace")
        .arg(&workspace);
    run_command(load, "soldr load ci archive");
    assert!(
        !warm_cache.join("zccache/runtime-binaries/zccache").exists(),
        "ci load must not restore zccache runtime binaries"
    );

    let _ = fs::remove_dir_all(workspace.join("target"));

    let mut warm_build = soldr_command(&["cargo", "build", "--release"]);
    warm_build
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &warm_root);
    run_command(warm_build, "warm soldr cargo build");

    let stats = read_json_file(
        &warm_cache
            .join("zccache")
            .join("logs")
            .join("last-session-stats.json"),
    );
    assert!(
        u64_field(&stats, "hits") > 0,
        "warm build should hit restored ci archive cache: {stats:#?}"
    );
    assert_eq!(
        u64_field(&stats, "misses"),
        0,
        "warm build should not miss after restoring ci archive: {stats:#?}"
    );
}
