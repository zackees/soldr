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
    println!("{}", save_ci_real_hits::value());
}
"#,
    );
    write(
        &dir.join("src/lib.rs"),
        b"pub fn value() -> u32 { (0..32).sum() }\n",
    );
}

fn native_library_events(cache_root: &Path) -> Vec<Value> {
    let mut paths = soldr_command(&["logs", "paths", "--json"]);
    paths.env("SOLDR_CACHE_DIR", cache_root);
    let output = run_command(paths, "resolve native compiler journal");
    let inventory: Value = serde_json::from_slice(&output.stdout).expect("parse log paths");
    let logs = inventory["paths"]
        .as_array()
        .expect("log path inventory")
        .iter()
        .find(|entry| entry["name"] == "zccache-embedded-logs")
        .expect("canonical embedded logs entry");
    let journal =
        Path::new(logs["path"].as_str().expect("logs path")).join("compile_journal.jsonl");
    println!("selected native compiler journal: {}", journal.display());
    fs::read_to_string(&journal)
        .unwrap_or_else(|err| panic!("read native journal {}: {err}", journal.display()))
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("native compiler event"))
        .filter(|event| {
            let args = event["args"].as_array().expect("compiler arguments");
            args.windows(2)
                .any(|pair| pair[0] == "--crate-name" && pair[1] == "save_ci_real_hits")
                && args
                    .windows(2)
                    .any(|pair| pair[0] == "--crate-type" && pair[1] == "lib")
        })
        .collect()
}

fn u64_field(json: &Value, key: &str) -> u64 {
    json.get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing numeric {key} in {json:#?}"))
}

fn load_real_compiler_archive(archive: &Path, cache_root: &Path, workspace: &Path) -> Output {
    let mut load = soldr_command(&["load", "--json"]);
    load.env("SOLDR_CACHE_DIR", cache_root)
        .arg("--archive")
        .arg(archive)
        .arg("--cache-dir")
        .arg(cache_root.join("cache"))
        .arg("--workspace")
        .arg(workspace);
    run_command(load, "soldr load ci archive")
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
#[ignore = "two real compiler builds; bosn cache-snapshot-acceptance, soldr#3604"]
fn save_ci_load_preserves_real_warm_rustc_hits() {
    let root = common::unique_temp_dir("save-ci-real-hits");
    let workspace = root.join("workspace");
    let cold_root = root.join("cold-cache-root");
    let warm_root = root.join("warm-cache-root");
    let target_dir = root.join("target");
    let archive = root.join("cache.tar.zst");
    let cold_cache = cold_root.join("cache");
    let warm_cache = warm_root.join("cache");

    create_real_crate(&workspace);
    fs::create_dir_all(&cold_cache).expect("create cold cache");
    fs::create_dir_all(&warm_cache).expect("create warm cache");

    let mut cold_build = soldr_command(&["cargo", "build", "--release"]);
    cold_build
        .current_dir(&workspace)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("SOLDR_CACHE_DIR", &cold_root);
    run_command(cold_build, "cold soldr cargo build");

    let mut flush = soldr_command(&["cache", "flush", "--json"]);
    flush.env("SOLDR_CACHE_DIR", &cold_root);
    run_command(flush, "cold cache flush");

    write(
        &cold_cache.join("zccache/runtime-binaries/zccache"),
        b"runtime binary must not enter ci archive",
    );

    let mut save = soldr_command(&["save", "--ci", "--json", "--zstd-level", "1"]);
    save.env("SOLDR_CACHE_DIR", &cold_root)
        .arg("--cache-dir")
        .arg(&cold_cache)
        .arg("--workspace")
        .arg(&workspace)
        .arg("--out")
        .arg(&archive);
    let save_output = run_command(save, "soldr save --ci");
    println!(
        "save transport receipt: stdout={} stderr={}",
        String::from_utf8_lossy(&save_output.stdout),
        String::from_utf8_lossy(&save_output.stderr)
    );
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

    let cold_events = native_library_events(&cold_root);
    let manifest = soldr_cli::cache_lib::save::read_manifest_from_archive(&archive)
        .expect("read production save manifest");
    let archived_indexes: Vec<_> = manifest
        .cache_files
        .iter()
        .filter(|entry| entry.path.ends_with("/index.bin"))
        .map(|entry| &entry.path)
        .collect();
    println!("archived compiler indexes: {archived_indexes:?}");
    assert!(
        cold_events.iter().any(|event| event["outcome"] == "miss"),
        "cold fixture must actually compile a cacheable library: {cold_events:#?}"
    );

    let load_output = load_real_compiler_archive(&archive, &warm_root, &workspace);
    println!(
        "load transport receipt: stdout={} stderr={}",
        String::from_utf8_lossy(&load_output.stdout),
        String::from_utf8_lossy(&load_output.stderr)
    );
    assert!(
        !warm_cache.join("zccache/runtime-binaries/zccache").exists(),
        "ci load must not restore zccache runtime binaries"
    );

    fs::rename(&target_dir, root.join("target-cold-retained"))
        .expect("retain cold products outside the warm compiler output path");

    let mut warm_build = soldr_command(&["cargo", "build", "--release"]);
    warm_build
        .current_dir(&workspace)
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("SOLDR_CACHE_DIR", &warm_root);
    run_command(warm_build, "warm soldr cargo build");

    let mut flush = soldr_command(&["cache", "flush", "--json"]);
    flush.env("SOLDR_CACHE_DIR", &warm_root);
    run_command(flush, "warm cache flush");
    let mut shutdown = soldr_command(&["cache", "shutdown", "--json"]);
    shutdown.env("SOLDR_CACHE_DIR", &warm_root);
    run_command(shutdown, "warm cache shutdown before assertions");
    let warm_events = native_library_events(&warm_root);
    println!(
        "native library events: {}",
        serde_json::json!({"cold": cold_events, "warm": warm_events})
    );
    assert!(
        warm_events.iter().any(|event| event["outcome"] == "hit"),
        "warm cacheable library must hit the relocated archive: {warm_events:#?}"
    );
    assert!(
        warm_events.iter().all(|event| event["outcome"] == "hit"),
        "every warm cacheable-library invocation must hit: {warm_events:#?}"
    );
    // A second restore must preserve the already populated, quiescent store.
    let repeated = load_real_compiler_archive(&archive, &warm_root, &workspace);
    println!(
        "same-root load receipt: stdout={} stderr={}",
        String::from_utf8_lossy(&repeated.stdout),
        String::from_utf8_lossy(&repeated.stderr)
    );
}
