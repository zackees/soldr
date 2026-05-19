#![allow(unused_imports)]

mod common;

use common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[test]
fn status_reports_cache_control_defaults() {
    let cache_root = unique_temp_dir("status");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("status")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr status");

    assert!(output.status.success(), "status command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cache dir:"),
        "status missing cache dir: {stdout}"
    );
    assert!(
        stdout.contains("cache default: enabled"),
        "status missing cache default: {stdout}"
    );
    assert!(
        stdout.contains("zccache version:"),
        "status missing zccache version: {stdout}"
    );
    assert!(
        stdout.contains("soldr zccache cache dir:"),
        "status missing effective zccache cache dir: {stdout}"
    );
    assert!(
        stdout.contains("not fetched yet"),
        "status should explain unfetched managed zccache state: {stdout}"
    );
}

#[test]
fn status_json_reports_stable_machine_fields() {
    let cache_root = unique_temp_dir("status-json");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["status", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr status --json");

    assert!(output.status.success(), "status --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("status --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "status");
    assert_eq!(json["soldr_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["cache_default_enabled"], true);
    assert_eq!(json["cache_enabled_for_invocation"], true);
    assert_eq!(json["managed_zccache_version"], "1.7.0");
    assert_eq!(json["root_dir"], cache_root.display().to_string());
    assert_eq!(
        json["cache_dir"],
        cache_root.join("cache").display().to_string()
    );
    assert_eq!(json["zccache"]["binary_fetched"], false);
    assert_eq!(json["zccache"]["session_log_present"], false);
    assert_eq!(json["zccache"]["journal_present"], false);
    assert_eq!(json["zccache"]["session_stats_present"], false);
    assert_eq!(
        json["zccache"]["cache_dir"],
        cache_root
            .join("cache")
            .join("zccache")
            .display()
            .to_string()
    );
    assert!(
        json["target"].as_str().is_some(),
        "status JSON missing target"
    );
    assert_eq!(
        json["zccache"]["session_log_path"],
        cache_root
            .join("cache")
            .join("zccache")
            .join("logs")
            .join("last-session.log")
            .display()
            .to_string()
    );
    assert_eq!(
        json["zccache"]["session_stats_path"],
        cache_root
            .join("cache")
            .join("zccache")
            .join("logs")
            .join("last-session-stats.json")
            .display()
            .to_string()
    );
}

#[test]
fn cache_command_reports_managed_zccache_status() {
    let cache_root = unique_temp_dir("cache-command");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let journal = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.jsonl");
    let session_log = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.log");
    let session_stats = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session-stats.json");
    fs::create_dir_all(journal.parent().expect("journal parent missing"))
        .expect("failed to create journal dir");
    fs::write(&session_log, "compile line\n").expect("failed to seed session log");
    fs::write(&journal, "{\"event\":\"hit\"}\n").expect("failed to seed journal");
    fs::write(
        &session_stats,
        "{\"status\":\"ok\",\"hits\":7,\"misses\":3}\n",
    )
    .expect("failed to seed session stats");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("cache")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache");

    assert!(output.status.success(), "cache command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("soldr zccache cache dir:"),
        "cache command missing cache dir: {stdout}"
    );
    assert!(
        stdout.contains("soldr zccache state dir:"),
        "cache command missing state dir: {stdout}"
    );
    assert!(
        stdout.contains("last session log:"),
        "cache command missing session log path: {stdout}"
    );
    assert!(
        stdout.contains("last session journal:"),
        "cache command missing journal path: {stdout}"
    );
    assert!(
        stdout.contains("last session stats:"),
        "cache command missing session stats path: {stdout}"
    );
    assert!(
        stdout.contains(&format!("{}", session_log.display())) && stdout.contains("(present)"),
        "cache command should report present session log: {stdout}"
    );
    assert!(
        stdout.contains("zccache: hits=7"),
        "cache command should surface managed zccache status output: {stdout}"
    );
}

#[test]
fn cache_json_reports_managed_zccache_status() {
    let cache_root = unique_temp_dir("cache-command-json");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let journal = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.jsonl");
    let session_log = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.log");
    let session_stats = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session-stats.json");
    fs::create_dir_all(journal.parent().expect("journal parent missing"))
        .expect("failed to create journal dir");
    fs::write(&session_log, "compile line\n").expect("failed to seed session log");
    fs::write(&journal, "{\"event\":\"hit\"}\n").expect("failed to seed journal");
    fs::write(
        &session_stats,
        "{\"status\":\"ok\",\"hits\":7,\"misses\":3}\n",
    )
    .expect("failed to seed session stats");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache --json");

    assert!(output.status.success(), "cache --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("cache --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache");
    assert_eq!(json["managed_zccache_version"], "1.7.0");
    assert_eq!(json["zccache"]["session_log_present"], true);
    assert_eq!(json["zccache"]["journal_present"], true);
    assert_eq!(json["zccache"]["session_stats_present"], true);
    assert_eq!(json["zccache"]["binary_fetched"], true);
    assert_eq!(
        json["zccache"]["cache_dir"],
        cache_root
            .join("cache")
            .join("zccache")
            .display()
            .to_string()
    );
    assert_eq!(
        json["zccache"]["session_log_path"],
        session_log.display().to_string()
    );
    assert_eq!(
        json["zccache"]["journal_path"],
        journal.display().to_string()
    );
    assert_eq!(
        json["zccache"]["session_stats_path"],
        session_stats.display().to_string()
    );
    assert_eq!(
        json["zccache"]["status_lines"][0],
        Value::String("hits=7".to_string())
    );
}

#[test]
fn cache_report_json_emits_stable_schema_when_files_missing() {
    let cache_root = unique_temp_dir("cache-report-empty");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "report", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cache report --json");

    assert!(
        output.status.success(),
        "cache report --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache report --json must produce parseable JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache report");
    assert_eq!(json["managed_zccache_version"], "1.7.0");
    assert_eq!(json["session_stats_present"], false);
    assert_eq!(json["journal_present"], false);
    assert!(json["last_session"].is_null());
    assert!(json["rollups"].is_null());
    assert!(
        json["diagnoses"]
            .as_array()
            .expect("diagnoses array")
            .is_empty(),
        "diagnoses should start empty before rule passes are wired"
    );
    let notes = json["notes"]
        .as_array()
        .expect("notes should always be an array");
    assert!(
        !notes.is_empty(),
        "missing files should produce explanatory notes"
    );
}

#[test]
fn cache_report_json_surfaces_persisted_session_stats() {
    let cache_root = unique_temp_dir("cache-report-stats");
    let zccache_dir = cache_root.join("cache").join("zccache");
    let stats_path = zccache_dir.join("logs").join("last-session-stats.json");
    fs::create_dir_all(stats_path.parent().expect("stats parent missing"))
        .expect("failed to create logs dir");
    fs::write(
        &stats_path,
        r#"{"status":"ok","session_id":"abc","hits":7,"misses":3,"compilations":10,"hit_rate":0.7,"time_saved_ms":12345}"#,
    )
    .expect("failed to seed session stats");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "report", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cache report --json");

    assert!(
        output.status.success(),
        "cache report --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("cache report --json must produce JSON");
    assert_eq!(json["session_stats_present"], true);
    assert_eq!(json["last_session"]["status"], "ok");
    assert_eq!(json["last_session"]["hits"], 7);
    assert_eq!(json["last_session"]["misses"], 3);
    assert_eq!(json["last_session"]["hit_rate"].as_f64(), Some(0.7));
}

#[test]
fn clean_clears_managed_zccache_and_state_dir() {
    let cache_root = unique_temp_dir("clean-command");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let state_dir = cache_root.join("cache").join("zccache");
    let user_home = cache_root.join("user-home");
    let user_global_zccache = user_home.join(".zccache");
    fs::create_dir_all(&user_global_zccache).expect("failed to seed user-global zccache");
    fs::write(user_global_zccache.join("index.redb"), "user cache")
        .expect("failed to seed user-global zccache file");
    let journal = state_dir.join("logs").join("last-session.jsonl");
    fs::create_dir_all(journal.parent().expect("journal parent missing"))
        .expect("failed to create journal dir");
    fs::write(&journal, "{\"event\":\"hit\"}\n").expect("failed to seed journal");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("clean")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &user_home)
        .env("USERPROFILE", &user_home)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr clean");

    assert!(output.status.success(), "clean command failed");
    assert!(
        !state_dir.exists(),
        "clean should remove soldr zccache state dir at {}",
        state_dir.display()
    );
    assert!(
        user_global_zccache.join("index.redb").exists(),
        "clean must not remove user-global zccache state at {}",
        user_global_zccache.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cleared zccache artifact cache"),
        "clean should report artifact cache cleanup: {stdout}"
    );
    assert!(
        stdout.contains("removed soldr zccache state dir:"),
        "clean should report state dir cleanup: {stdout}"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("zccache clear"),
        "clean should call managed zccache clear: {log}"
    );
}

#[test]
fn purge_removes_soldr_artifact_dirs_and_keeps_config() {
    let cache_root = unique_temp_dir("purge-command");
    let bin_dir = cache_root.join("bin");
    let cache_dir = cache_root.join("cache");
    let zccache_state_dir = cache_dir.join("zccache").join("logs");
    let config_file = cache_root.join("config.toml");
    fs::create_dir_all(&bin_dir).expect("failed to create bin dir");
    fs::create_dir_all(&zccache_state_dir).expect("failed to create zccache state dir");
    fs::write(bin_dir.join("soldr-tool"), "cached binary").expect("failed to seed bin cache");
    fs::write(zccache_state_dir.join("last-session.jsonl"), "{}\n")
        .expect("failed to seed zccache state");
    fs::write(&config_file, "cache = true\n").expect("failed to seed config");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("purge")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr purge");

    assert!(output.status.success(), "purge command failed");
    assert!(
        !bin_dir.exists(),
        "purge should remove soldr-managed fetched tool artifacts at {}",
        bin_dir.display()
    );
    assert!(
        !cache_dir.exists(),
        "purge should remove soldr-managed cache artifacts at {}",
        cache_dir.display()
    );
    assert!(
        config_file.exists(),
        "purge should keep non-artifact config at {}",
        config_file.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("removed soldr cache dir:"),
        "purge should report cache cleanup: {stdout}"
    );
    assert!(
        stdout.contains("removed soldr bin dir:"),
        "purge should report bin cleanup: {stdout}"
    );
}

#[test]
fn purge_reports_empty_cache_without_creating_dirs() {
    let cache_root = unique_temp_dir("purge-empty-command");
    let bin_dir = cache_root.join("bin");
    let cache_dir = cache_root.join("cache");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("purge")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr purge");

    assert!(output.status.success(), "purge command failed");
    assert!(
        !bin_dir.exists(),
        "purge should not create missing bin dir at {}",
        bin_dir.display()
    );
    assert!(
        !cache_dir.exists(),
        "purge should not create missing cache dir at {}",
        cache_dir.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("soldr cache is already empty:"),
        "purge should report empty cache: {stdout}"
    );
}

#[test]
fn purge_removes_corrupt_artifact_paths() {
    let cache_root = unique_temp_dir("purge-corrupt-command");
    let bin_path = cache_root.join("bin");
    let cache_path = cache_root.join("cache");
    fs::write(&bin_path, "not a dir").expect("failed to seed corrupt bin path");
    fs::write(&cache_path, "not a dir").expect("failed to seed corrupt cache path");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("purge")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr purge");

    assert!(output.status.success(), "purge command failed");
    assert!(
        !bin_path.exists(),
        "purge should remove corrupt soldr bin path at {}",
        bin_path.display()
    );
    assert!(
        !cache_path.exists(),
        "purge should remove corrupt soldr cache path at {}",
        cache_path.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("removed soldr cache entry:"),
        "purge should report corrupt cache path cleanup: {stdout}"
    );
    assert!(
        stdout.contains("removed soldr bin entry:"),
        "purge should report corrupt bin path cleanup: {stdout}"
    );
}

#[test]
fn purge_rejects_json_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["purge", "--json"])
        .output()
        .expect("failed to run soldr purge --json");

    assert!(
        !output.status.success(),
        "purge --json should be rejected because JSON is not supported there"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--json"),
        "expected clap to reject purge --json: {stderr}"
    );
}

#[test]
fn clean_rejects_json_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["clean", "--json"])
        .output()
        .expect("failed to run soldr clean --json");

    assert!(
        !output.status.success(),
        "clean --json should be rejected because JSON is not supported there"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--json"),
        "expected clap to reject clean --json: {stderr}"
    );
}
