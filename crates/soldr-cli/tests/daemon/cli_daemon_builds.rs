//! Integration coverage for the Phase 2 build-session pipeline.
//!
//! Seeds events + builds directly via `daemon::db` helpers (which open
//! the same `state.sqlite3` the daemon uses), then runs
//! `soldr daemon builds list --json` / `... slow --threshold-ms <ms>`
//! through the CLI and asserts the JSON payload shape and filters.

#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common;
use serde_json::Value;
use soldr_cli::cache_lib::target_registry::TargetRegistry;
use soldr_cli::daemon::client;
use soldr_cli::daemon::db::{self, Event, EventKind};
use soldr_cli::daemon::protocol::BuildRecord;
use wait_timeout::ChildExt;

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
    let stem = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn direct_sock(root: &Path) -> PathBuf {
    common::isolated_daemon::isolated_daemon_control_endpoint(&soldr_daemon_bin(), root)
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    common::isolated_daemon::configure_isolated_daemon_client(
        &mut cmd,
        &soldr_daemon_bin(),
        cache_root,
    );
    cmd.args(args)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env("SOLDR_TEST_DIRECT_DAEMON_CONTROL", "1")
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
        let mut cmd =
            common::isolated_daemon::isolated_daemon_command(&soldr_daemon_bin(), cache_root);
        // Capture stderr: when the daemon dies between the readiness check
        // and a later in-test IPC call, its own diagnostics are the only
        // evidence, and Stdio::null() was discarding them.
        let stderr_log = std::fs::File::create(cache_root.join("daemon-stderr.log"))
            .expect("create daemon stderr log");
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_log));
        let mut child = cmd.spawn().expect("spawn soldr-daemon");
        // Cold target-run hosts may hash several large test images before
        // this daemon can publish its route. The profile serializes this
        // binary, and this deadline remains below the per-test backstop.
        let deadline = Instant::now() + Duration::from_secs(90);
        let pid_file = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("broker-route-claim.pb");
        let mut ready = false;
        while Instant::now() < deadline {
            if pid_file.exists() {
                let status = run_soldr(&["daemon", "status", "--json"], cache_root, home_root);
                if status.status.success()
                    && serde_json::from_slice::<Value>(&status.stdout)
                        .ok()
                        .and_then(|body| body["running"].as_bool())
                        .unwrap_or(false)
                {
                    ready = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "isolated daemon never became ready under {}",
                cache_root.display()
            );
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
    let db_path = cache_root.join("state.sqlite3");
    db::upsert_build(
        &db_path,
        &seeded_record(session_id, started_at_ms, wall_ms, exit_code),
    )
    .expect("upsert");
}

fn seeded_record(session_id: u64, started_at_ms: i64, wall_ms: u64, exit_code: i32) -> BuildRecord {
    BuildRecord {
        session_id,
        repo_root: "/seeded".into(),
        started_at_ms,
        ended_at_ms: Some(started_at_ms + wall_ms as i64),
        exit_code: Some(exit_code),
        total_wall_ms: Some(wall_ms),
        crate_count: 3,
        slowest_crate_us: Some(wall_ms * 1000 / 2),
        slowest_crate_name: Some("seeded-crate".into()),
        cache_summary: None,
        log_paths: None,
        miss_reasons: Vec::new(),
    }
}

#[test]
fn builds_list_returns_seeded_records_via_daemon_query() {
    let cache_root = unique_temp_dir("builds-list-cache");
    let home_root = unique_temp_dir("builds-list-home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);

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
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);

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
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);
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

#[test]
fn gc_list_uses_daemon_owned_registry_while_the_daemon_holds_the_lock() {
    let cache_root = unique_temp_dir("gc-daemon-registry-cache");
    let home_root = unique_temp_dir("gc-daemon-registry-home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);
    let live_target = cache_root.join("seeded-workspace").join("target");
    std::fs::create_dir_all(&live_target).expect("create target");
    std::fs::write(live_target.join("artifact"), b"seeded").expect("write target artifact");
    {
        let registry =
            TargetRegistry::open(&cache_root.join("state.sqlite3")).expect("open registry");
        registry
            .upsert_with_time(&live_target, 1_700_000_000)
            .expect("seed live target");
    }

    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);
    let output = run_soldr(&["gc", "list", "--json"], &cache_root, &home_root);
    assert!(
        output.status.success(),
        "gc list failed while daemon owns state.sqlite3: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout).expect("gc list json");
    assert!(body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .any(|entry| entry["path"] == live_target.display().to_string()));

    let sock = direct_sock(&cache_root);
    let rows = match client::list_target_registry(&sock) {
        Ok(rows) => rows,
        Err(error) => {
            let child_state = daemon
                .child
                .as_mut()
                .map(|child| format!("{:?}", child.try_wait()));
            let daemon_stderr =
                std::fs::read_to_string(cache_root.join("daemon-stderr.log")).unwrap_or_default();
            panic!(
                "daemon registry query failed ({error:?}) dialing {}\nchild={child_state:?}\ndaemon stderr:\n{daemon_stderr}",
                sock.display()
            )
        }
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, live_target.display().to_string());
    assert_eq!(
        client::remove_target_registry(&sock, vec![live_target.display().to_string()])
            .expect("daemon registry removal"),
        1
    );
    assert!(client::list_target_registry(&sock)
        .expect("daemon registry re-query")
        .is_empty());
}

#[test]
fn logs_queries_use_the_daemon_while_it_owns_the_build_history_lock() {
    let cache_root = unique_temp_dir("logs-query-cache");
    let home_root = unique_temp_dir("logs-query-home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);
    let session_id = 0xabc_def0_1234_u64;
    seed_build(&cache_root, session_id, 1_000, 1_500, 0);
    db::append_event(
        &cache_root.join("state.sqlite3"),
        &Event {
            ts_ms: 1_500,
            session_id: Some(session_id),
            kind: EventKind::CompileEnd,
            crate_name: Some("seeded-crate".into()),
            duration_us: Some(500_000),
            target_dir: Some("/seeded/target".into()),
            exit_code: Some(0),
        },
    )
    .expect("seed event");

    let _daemon = DaemonProc::spawn(&cache_root, &home_root);
    let list = run_soldr(&["logs", "list", "--json"], &cache_root, &home_root);
    assert!(
        list.status.success(),
        "logs list failed while the daemon owns state.sqlite3: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_body: Value = serde_json::from_slice(&list.stdout).expect("logs list json");
    assert_eq!(list_body["launches"][0]["id"], session_id.to_string());

    let show = run_soldr(
        &["logs", "show", "00000abc", "--json"],
        &cache_root,
        &home_root,
    );
    assert!(
        show.status.success(),
        "logs show failed while the daemon owns state.sqlite3: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_body: Value = serde_json::from_slice(&show.stdout).expect("logs show json");
    assert_eq!(show_body["launch"]["id"], session_id.to_string());
    assert!(
        show_body["events"].is_array(),
        "logs show must source its exact-session event query from the daemon"
    );
}

#[test]
fn logs_show_exact_id_is_not_limited_by_prefix_history_page_size() {
    let cache_root = unique_temp_dir("logs-exact-cache");
    let home_root = unique_temp_dir("logs-exact-home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);
    let db_path = cache_root.join("state.sqlite3");
    let database = db::open_handle(&db_path).expect("open seed database");

    db::upsert_build_in(&database, &seeded_record(1, 1, 100, 0)).expect("seed old record");
    db::upsert_build_in(&database, &seeded_record(0x1234, 50_000, 100, 0))
        .expect("seed numeric hexadecimal prefix record");
    for session_id in 20_000..30_000 {
        db::upsert_build_in(
            &database,
            &seeded_record(session_id, session_id as i64, 100, 0),
        )
        .expect("seed newer record");
    }
    drop(database);

    let _daemon = DaemonProc::spawn(&cache_root, &home_root);
    let show = run_soldr(&["logs", "show", "1", "--json"], &cache_root, &home_root);
    assert!(
        show.status.success(),
        "exact old launch must bypass the 10,000-row prefix page: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    let body: Value = serde_json::from_slice(&show.stdout).expect("logs show json");
    assert_eq!(body["launch"]["id"], "1");

    let prefix = run_soldr(
        &["logs", "show", "000000000000123", "--json"],
        &cache_root,
        &home_root,
    );
    assert!(
        prefix.status.success(),
        "numeric hexadecimal prefix must fall through after exact-id miss: {}",
        String::from_utf8_lossy(&prefix.stderr)
    );
    let prefix_body: Value = serde_json::from_slice(&prefix.stdout).expect("prefix json");
    assert_eq!(prefix_body["launch"]["id"], 0x1234.to_string());
}

#[test]
fn logs_unavailable_daemon_never_waits_for_its_database_lock() {
    let cache_root = unique_temp_dir("logs-unavailable-cache");
    let home_root = unique_temp_dir("logs-unavailable-home");
    let _broker = common::BrokerHomeGuard::new(&cache_root, &home_root);
    seed_build(&cache_root, 77, 1_000, 500, 0);

    let _endpoint_owner: Box<dyn std::any::Any> = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        Box::new(
            db::open_handle(&cache_root.join("state.sqlite3"))
                .expect("hold the daemon-owned database lock without a named-pipe endpoint"),
        )
    } else {
        let daemon = DaemonProc::spawn(&cache_root, &home_root);
        let socket = direct_sock(&cache_root);
        std::fs::remove_file(&socket).expect("hide daemon socket while it retains the lock");
        Box::new(daemon)
    };

    let mut command = Command::new(common::soldr_bin());
    common::isolated_daemon::configure_isolated_daemon_client(
        &mut command,
        &soldr_daemon_bin(),
        &cache_root,
    );
    command
        .args(["logs", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env("SOLDR_TEST_DIRECT_DAEMON_CONTROL", "1")
        .env_remove("RUSTC_WRAPPER")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn logs list");
    // 10s, not the historical 3s: the old tight budget existed to
    // distinguish a fast IPC failure from redb's 5s exclusive-lock wait.
    // Post-SQLite there is no lock wait to detect (the CLI never opens the
    // daemon-owned store, and WAL would not block a reader anyway), so the
    // budget only needs to bound the whole invocation -- and 3s raced the
    // front door's cold broker staging under load, which is startup noise,
    // not the property this test asserts.
    let status = child
        .wait_timeout(Duration::from_secs(10))
        .expect("wait for logs list")
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("logs list hung instead of failing through IPC")
        });
    let output = child.wait_with_output().expect("collect logs list output");
    assert!(
        !status.success(),
        "unreachable daemon must return an actionable CLI error"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires the running soldr-daemon"),
        "missing daemon explanation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
