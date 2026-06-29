//! Integration coverage for the Phase 2 build-session pipeline.
//!
//! Seeds events + builds directly via `daemon::db` helpers (which open
//! the same `state.redb` the daemon uses), then runs
//! `soldr daemon builds list --json` / `... slow --threshold-ms <ms>`
//! through the CLI and asserts the JSON payload shape and filters.

#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use soldr_cli::daemon::db;
use soldr_cli::daemon::protocol::BuildRecord;
mod common;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn soldr_daemon_bin() -> PathBuf {
    let soldr = common::soldr_bin();
    let parent = soldr.parent().expect("parent");
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    cmd.args(args)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env_remove("RUSTC_WRAPPER");
    cmd.output().expect("run soldr")
}

struct DaemonProc {
    child: Option<Child>,
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl DaemonProc {
    fn spawn(cache_root: &Path, home_root: &Path) -> Self {
        let mut cmd = Command::new(soldr_daemon_bin());
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid_file = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("daemon.pid");
        while Instant::now() < deadline {
            if pid_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            child: Some(child),
            cache_root: cache_root.to_path_buf(),
            home_root: home_root.to_path_buf(),
        }
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn seed_build(
    cache_root: &Path,
    session_id: u64,
    started_at_ms: i64,
    wall_ms: u64,
    exit_code: i32,
) {
    let db_path = cache_root.join("state.redb");
    db::upsert_build(
        &db_path,
        &BuildRecord {
            session_id,
            repo_root: "/seeded".into(),
            started_at_ms,
            ended_at_ms: Some(started_at_ms + wall_ms as i64),
            exit_code: Some(exit_code),
            total_wall_ms: Some(wall_ms),
            crate_count: 3,
            slowest_crate_us: Some(wall_ms * 1000 / 2),
            slowest_crate_name: Some("seeded-crate".into()),
        },
    )
    .expect("upsert");
}

#[test]
fn builds_list_returns_seeded_records_via_daemon_query() {
    let cache_root = unique_temp_dir("builds-list-cache");
    let home_root = unique_temp_dir("builds-list-home");

    seed_build(&cache_root, 1, 1_000, 500, 0);
    seed_build(&cache_root, 2, 2_000, 250, 0);
    seed_build(&cache_root, 3, 3_000, 10_000, 1);

    // Daemon must NOT be running when we seed — redb refuses concurrent
    // multi-process opens. After seeding, start the daemon for the
    // query path.
    let _daemon = DaemonProc::spawn(&cache_root, &home_root);

    let out = run_soldr(
        &["daemon", "builds", "list", "--json", "--limit", "10"],
        &cache_root,
        &home_root,
    );
    assert!(
        out.status.success(),
        "builds list failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).expect("json");
    let builds = body["builds"].as_array().expect("builds array");
    assert_eq!(builds.len(), 3, "all three seeded rows returned");
    // Newest first → session 3 then 2 then 1
    assert_eq!(builds[0]["session_id"], 3);
    assert_eq!(builds[1]["session_id"], 2);
    assert_eq!(builds[2]["session_id"], 1);
}

#[test]
fn builds_slow_filters_by_threshold() {
    let cache_root = unique_temp_dir("builds-slow-cache");
    let home_root = unique_temp_dir("builds-slow-home");

    seed_build(&cache_root, 100, 1_000, 50, 0); // fast
    seed_build(&cache_root, 200, 2_000, 1_500, 0); // medium
    seed_build(&cache_root, 300, 3_000, 7_000, 1); // slow

    let _daemon = DaemonProc::spawn(&cache_root, &home_root);

    let out = run_soldr(
        &[
            "daemon",
            "builds",
            "slow",
            "--threshold-ms",
            "1000",
            "--json",
        ],
        &cache_root,
        &home_root,
    );
    assert!(out.status.success(), "slow failed: {out:?}");
    let body: Value = serde_json::from_slice(&out.stdout).expect("json");
    let builds = body["builds"].as_array().expect("builds array");
    assert_eq!(builds.len(), 2, "threshold filter keeps medium + slow only");
    // Sorted desc by total_wall_ms
    assert_eq!(builds[0]["session_id"], 300);
    assert_eq!(builds[1]["session_id"], 200);
}

#[test]
fn builds_list_when_daemon_absent_reports_not_running() {
    let cache_root = unique_temp_dir("builds-absent-cache");
    let home_root = unique_temp_dir("builds-absent-home");
    let out = run_soldr(
        &["daemon", "builds", "list", "--json"],
        &cache_root,
        &home_root,
    );
    assert!(
        out.status.success(),
        "absent-daemon query must exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(body["running"].as_bool(), Some(false));
    assert!(body["builds"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(false));
}
