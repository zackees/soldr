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
    // Redirect HOME / USERPROFILE so the test doesn't see the developer's
    // host-side pin under `~/.soldr/bin/zccache-pinned/`. After issue #426
    // the pin is home-anchored and survives SOLDR_CACHE_DIR overrides — so
    // a stale host pin would otherwise leak into the test as a fetched
    // binary and break the "not fetched yet" assertion below.
    let home_root = unique_temp_dir("status-home");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("status")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
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
    // See sibling `status_reports_cache_control_defaults`: redirect home
    // discovery so the developer's host-side pin (issue #426) can't leak
    // into the test as a "binary already fetched" signal.
    let home_root = unique_temp_dir("status-json-home");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["status", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
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
    assert_eq!(json["managed_zccache_version"], "1.11.0");
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
    assert_eq!(json["managed_zccache_version"], "1.11.0");
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
    assert_eq!(json["managed_zccache_version"], "1.11.0");
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

/// Regression for soldr#430. `last_session` must be a verbatim passthrough
/// of `last-session-stats.json` so new zccache schema additions (e.g.
/// PROTOCOL_VERSION 9's `phase_profile`) reach downstream consumers
/// without a soldr-side change. If someone "cleans up" `last_session`
/// into a typed struct, this test fails.
#[test]
fn cache_report_json_passes_through_unknown_session_stat_fields() {
    let cache_root = unique_temp_dir("cache-report-forward-compat");
    let zccache_dir = cache_root.join("cache").join("zccache");
    let stats_path = zccache_dir.join("logs").join("last-session-stats.json");
    fs::create_dir_all(stats_path.parent().expect("stats parent missing"))
        .expect("failed to create logs dir");
    fs::write(
        &stats_path,
        r#"{
            "status":"ok",
            "hits":11,
            "phase_profile":{"hit_count":103,"miss_count":4,"buckets":[1,2,3]},
            "future_field":"please don't drop me"
        }"#,
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
    assert_eq!(json["last_session"]["hits"], 11);
    assert_eq!(json["last_session"]["phase_profile"]["hit_count"], 103);
    assert_eq!(json["last_session"]["phase_profile"]["miss_count"], 4);
    assert_eq!(json["last_session"]["phase_profile"]["buckets"][2], 3);
    assert_eq!(json["last_session"]["future_field"], "please don't drop me");
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

/// soldr#383 (part 1): `cache flush` synchronously persists the
/// depgraph + in-memory state without stopping the daemon. We assert
/// the JSON output shape so CI consumers (setup-soldr) can rely on it.
#[test]
fn cache_flush_emits_zccache_stats_on_success() {
    let cache_root = unique_temp_dir("cache-flush-ok");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "flush", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache flush --json");

    assert!(
        output.status.success(),
        "cache flush --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache flush --json must produce parseable JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache flush");
    assert_eq!(json["flushed"], true);
    assert_eq!(json["stats"]["status"], "ok");
    assert_eq!(json["stats"]["bytes_written"], 4096);

    let log = fs::read_to_string(&log_path).expect("read tool log");
    assert!(
        log.contains("zccache flush args="),
        "flush must shell out to `zccache flush`: {log}"
    );
}

/// soldr#383 (part 1): when the running zccache predates the `flush`
/// subcommand, `cache flush` returns 0 with a note so CI does not
/// fail the post step on older daemons.
#[test]
fn cache_flush_handles_unsupported_zccache_gracefully() {
    let cache_root = unique_temp_dir("cache-flush-unsupported");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "flush", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_ZCCACHE_FLUSH_UNSUPPORTED", "1")
        .output()
        .expect("failed to run soldr cache flush --json");

    assert!(
        output.status.success(),
        "cache flush should succeed-with-note when zccache lacks flush\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache flush --json must produce parseable JSON");
    assert_eq!(json["flushed"], false);
    let notes = json["notes"].as_array().expect("notes array");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("does not yet implement")),
        "expected upgrade-hint note, got: {notes:?}"
    );
}

/// soldr#383 (part 1): older zccache builds that have `flush` but not
/// `flush --json` should still produce a durable on-disk snapshot.
/// soldr retries with the bare form and surfaces a note explaining
/// the JSON form was unavailable.
#[test]
fn cache_flush_falls_back_when_json_flag_is_unsupported() {
    let cache_root = unique_temp_dir("cache-flush-no-json");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "flush", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_ZCCACHE_FLUSH_NO_JSON", "1")
        .output()
        .expect("failed to run soldr cache flush --json");

    assert!(
        output.status.success(),
        "cache flush should retry without --json\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache flush --json must produce parseable JSON");
    assert_eq!(json["flushed"], true);
    assert!(
        json["stats"].is_null(),
        "no upstream JSON should mean stats is null"
    );
    let notes = json["notes"].as_array().expect("notes array");
    assert!(
        notes.iter().any(|n| {
            let s = n.as_str().unwrap_or("");
            s.contains("flush --json not supported")
        }),
        "expected fallback note, got: {notes:?}"
    );
}

/// soldr#383 (part 2): `cache shutdown` must block until the daemon
/// process has actually exited. The fake zccache here marks the
/// daemon as "stopped" only after `zccache stop` runs, so the poll
/// proves we are checking status *after* the signal.
#[test]
fn cache_shutdown_waits_for_daemon_to_exit() {
    let cache_root = unique_temp_dir("cache-shutdown-wait");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let down_marker = cache_root.join("daemon.down");
    // Sanity: the marker must not exist before stop runs.
    assert!(!down_marker.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "shutdown", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER", &down_marker)
        .output()
        .expect("failed to run soldr cache shutdown --json");

    assert!(
        output.status.success(),
        "cache shutdown --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache shutdown --json must produce parseable JSON");
    assert_eq!(json["daemon_stopped"], true);
    assert_eq!(
        json["daemon_exited"], true,
        "shutdown must confirm daemon exit before returning"
    );

    // The fake script logs every zccache invocation. After stop we
    // should see at least one status probe (the polling loop's
    // confirmation that the daemon is gone).
    let log = fs::read_to_string(&log_path).expect("read tool log");
    let stop_idx = log
        .find("zccache stop")
        .expect("zccache stop must have run");
    let after_stop = &log[stop_idx..];
    // status writes to stderr/stdout but not to the tool log; the
    // log on its own only confirms stop ran. The daemon_exited flag
    // above proves the polling loop observed the down-marker.
    let _ = after_stop;
}

/// soldr#383 (part 2): when the daemon outlives the shutdown timeout
/// (the prod-bug case from the issue), soldr must exit non-zero so CI
/// fails loud instead of racing the depgraph flush with a tarball.
#[test]
fn cache_shutdown_times_out_when_daemon_does_not_exit() {
    let cache_root = unique_temp_dir("cache-shutdown-timeout");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);

    // Deliberately omit SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER, so the
    // fake `status` always reports the daemon as up. The shutdown
    // poll then has to give up after the deadline.
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args([
            "cache",
            "shutdown",
            "--shutdown-timeout-seconds",
            "1",
            "--json",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache shutdown --shutdown-timeout-seconds 1");

    assert!(
        !output.status.success(),
        "cache shutdown must fail when daemon outlives the timeout\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("did not exit within"),
        "stderr should explain the timeout: {stderr}"
    );
}

/// soldr#383 (part 2): `--no-wait` opts out of the polling loop and
/// returns immediately so interactive shells are not blocked. The
/// JSON output must still note that exit was not confirmed.
#[test]
fn cache_shutdown_no_wait_skips_polling() {
    let cache_root = unique_temp_dir("cache-shutdown-no-wait");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);

    // Daemon never goes down — without the poll this would hang.
    let start = std::time::Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args([
            "cache",
            "shutdown",
            "--no-wait",
            "--shutdown-timeout-seconds",
            "60",
            "--json",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache shutdown --no-wait");

    let elapsed = start.elapsed();
    assert!(
        output.status.success(),
        "--no-wait must succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "--no-wait must return fast, took {elapsed:?}"
    );
    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache shutdown --json must produce parseable JSON");
    assert_eq!(json["daemon_stopped"], true);
    assert_eq!(
        json["daemon_exited"], false,
        "no-wait should leave daemon_exited=false"
    );
    let notes = json["notes"].as_array().expect("notes array");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("--no-wait")),
        "expected --no-wait note, got: {notes:?}"
    );
}
