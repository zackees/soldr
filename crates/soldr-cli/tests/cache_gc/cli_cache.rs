#![allow(unused_imports)]

use crate::common;

#[test]
fn isolated_commands_scrub_every_route_selector() {
    let command = common::isolated_soldr_command();
    let removals = command
        .get_envs()
        .filter_map(|(name, value)| {
            value
                .is_none()
                .then_some(name.to_string_lossy().into_owned())
        })
        .collect::<std::collections::BTreeSet<_>>();

    for name in common::OUTER_ROUTE_ENV_VARS {
        assert!(
            removals.contains(*name),
            "isolated command must scrub route-affecting variable {name}"
        );
        assert!(common::is_outer_route_env(name));
    }
    for name in [
        "RUNNING_PROCESS_BROKER_V1_INSTANCE",
        "RUNNING_PROCESS_BROKER_V1_SESSION_TOKEN",
        "RUNNING_PROCESS_BROKER_V1_BACKEND_PIPE",
    ] {
        assert!(
            common::is_outer_route_env(name),
            "broker-owned child identity must never leak into a fixture: {name}"
        );
    }
    assert!(!common::is_outer_route_env("SOLDR_BROKER_DEBUG"));
}

use crate::common::*;
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
    let output = common::isolated_soldr_command()
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
    // soldr#1593: the zccache CLI is called in-process and has no binary.
    assert!(
        stdout.contains("zccache runtime: in-process"),
        "status should report the in-process zccache backend: {stdout}"
    );
    assert!(
        stdout.contains("zccache status: embedded in soldr-daemon"),
        "status should point daemon health checks at soldr-daemon: {stdout}"
    );
    assert!(
        !stdout.contains("zccache status: no output"),
        "embedded zccache should not look like a silent external daemon: {stdout}"
    );
}

#[test]
fn status_json_reports_stable_machine_fields() {
    let cache_root = unique_temp_dir("status-json");
    // See sibling `status_reports_cache_control_defaults`: redirect home
    // discovery so the developer's host-side pin (issue #426) can't leak
    // into the test as a "binary already fetched" signal.
    let home_root = unique_temp_dir("status-json-home");
    let output = common::isolated_soldr_command()
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
    assert!(
        json["managed_zccache_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "managed_zccache_version should be the embedded zccache version string"
    );
    assert_eq!(json["root_dir"], cache_root.display().to_string());
    assert_eq!(
        json["cache_dir"],
        cache_root.join("cache").display().to_string()
    );
    assert_eq!(json["zccache"]["binary_path"], Value::Null);
    assert_eq!(json["zccache"]["binary_fetched"], false);
    assert_eq!(json["zccache"]["binary_source"], "in-process");
    // soldr#1838 Phase 4: the compile-daemon fallback rollup is always
    // present (empty on a fresh root), mirroring `soldr doctor`, so a
    // consumer can read it unconditionally.
    assert!(
        json["fallbacks"]["total"].is_number(),
        "status --json must carry a fallbacks rollup: {json}"
    );
    assert!(
        json["fallbacks"]["recent"].is_array(),
        "the fallback rollup must expose a recent[] list: {json}"
    );
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

    let output = common::isolated_soldr_command()
        .arg("cache")
        .env("SOLDR_CACHE_DIR", &cache_root)
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
    // soldr#1593: rustc compile caching lives in the embedded daemon and
    // the maintenance surface is called in-process.
    assert!(
        stdout.contains("zccache runtime: in-process"),
        "cache command should report the in-process zccache backend: {stdout}"
    );
    assert!(
        stdout.contains("zccache status: embedded in soldr-daemon"),
        "cache command should point daemon health checks at soldr-daemon: {stdout}"
    );
}

#[test]
fn cache_json_reports_managed_zccache_status() {
    let cache_root = unique_temp_dir("cache-command-json");
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

    let output = common::isolated_soldr_command()
        .args(["cache", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cache --json");

    assert!(output.status.success(), "cache --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("cache --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache");
    assert!(
        json["managed_zccache_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "managed_zccache_version should be the embedded zccache version string"
    );
    assert_eq!(json["zccache"]["session_log_present"], true);
    assert_eq!(json["zccache"]["journal_present"], true);
    assert_eq!(json["zccache"]["session_stats_present"], true);
    assert_eq!(json["zccache"]["binary_path"], Value::Null);
    assert_eq!(json["zccache"]["binary_fetched"], false);
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
    assert_eq!(json["zccache"]["binary_source"], "in-process");
}

#[test]
fn cache_report_json_emits_stable_schema_when_files_missing() {
    let cache_root = unique_temp_dir("cache-report-empty");

    let output = common::isolated_soldr_command()
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
    assert!(
        json["managed_zccache_version"]
            .as_str()
            .is_some_and(|v| !v.is_empty()),
        "managed_zccache_version should be the embedded zccache version string"
    );
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

    let output = common::isolated_soldr_command()
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

    let output = common::isolated_soldr_command()
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

    let output = common::isolated_soldr_command()
        .arg("clean")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &user_home)
        .env("USERPROFILE", &user_home)
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

    // soldr#1368: `clean` just removes soldr's on-disk zccache state dir;
    // there is no managed `zccache clear` subprocess any more.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("removed soldr zccache state dir:"),
        "clean should report state dir cleanup: {stdout}"
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

    let output = common::isolated_soldr_command()
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

    let output = common::isolated_soldr_command()
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

    let output = common::isolated_soldr_command()
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
    let output = common::isolated_soldr_command()
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
    let output = common::isolated_soldr_command()
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

/// soldr#1368: `cache flush` checkpoints the soldr-daemon EMBEDDED zccache
/// state. With no daemon reachable there is no checkpoint to certify, so the
/// command must report `flushed=false` and return non-zero instead of claiming
/// a successful no-op.
#[test]
fn cache_flush_fails_when_embedded_daemon_is_unavailable() {
    let cache_root = unique_temp_dir("cache-flush-embedded");
    let home_root = unique_temp_dir("cache-flush-home");
    let output = common::isolated_soldr_command()
        .args(["cache", "flush", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .output()
        .expect("failed to run soldr cache flush --json");
    let _ = common::isolated_soldr_command()
        .args(["broker", "stop"])
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .output();
    assert!(
        !output.status.success(),
        "cache flush --json must fail when no daemon can acknowledge persistence
stdout:
{}
stderr:
{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache flush --json must produce parseable JSON");
    assert_eq!(json["command"], "cache flush");
    assert_eq!(json["flushed"], false);
    let notes = json["notes"].as_array().expect("notes array");
    assert!(
        notes.iter().any(|n| {
            let s = n.as_str().unwrap_or("");
            s.contains("embedded") && s.contains("unavailable")
        }),
        "expected an embedded-flush-unavailable note, got: {notes:?}"
    );
}

/// `cache shutdown` targets soldr-daemon itself. With no daemon it remains
/// idempotent, but its JSON must distinguish "already absent" from a shutdown
/// request that was actually accepted.
#[test]
fn cache_shutdown_reports_already_absent_truthfully() {
    let cache_root = unique_temp_dir("cache-shutdown-embedded");
    let home_root = unique_temp_dir("cache-shutdown-home");
    let start = std::time::Instant::now();
    let output = common::isolated_soldr_command()
        .args(["cache", "shutdown", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .output()
        .expect("failed to run soldr cache shutdown --json");
    let elapsed = start.elapsed();
    let _ = common::isolated_soldr_command()
        .args(["broker", "stop"])
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .output();
    assert!(
        output.status.success(),
        "cache shutdown --json must succeed
stdout:
{}
stderr:
{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "cache shutdown must not poll/hang, took {elapsed:?}"
    );
    let json: Value = serde_json::from_slice(&output.stdout)
        .expect("cache shutdown --json must produce parseable JSON");
    assert_eq!(json["command"], "cache shutdown");
    assert_eq!(json["daemon_was_running"], false);
    assert_eq!(json["shutdown_requested"], false);
    assert_eq!(json["daemon_exited"], true);
}
