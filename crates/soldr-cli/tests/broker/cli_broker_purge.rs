//! soldr#3193: `soldr broker purge`, the `doctor` inventory, the front-door
//! leak notice, the `SOLDR_BROKER_AUTOSPAWN=0` switch, and the broker's idle
//! stand-down.
//!
//! The process table is scripted through `SOLDR_TEST_BROKER_PROCESS_LIST_FILE`
//! wherever a test needs particular rows, so nothing here depends on -- or
//! signals -- whatever else is running on the host.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::common;
use crate::common::tracked_child::TrackedChild;

const PROCESS_LIST_ENV: &str = "SOLDR_TEST_BROKER_PROCESS_LIST_FILE";
const READY_TIMEOUT: Duration = Duration::from_secs(30);

fn scripted_table(dir: &Path, rows: &[serde_json::Value]) -> PathBuf {
    std::fs::create_dir_all(dir).expect("mkdir");
    let path = dir.join("process-table.json");
    std::fs::write(&path, serde_json::to_vec(rows).expect("json")).expect("write");
    path
}

fn broker_row(pid: u32, home: &Path) -> serde_json::Value {
    serde_json::json!({
        "pid": pid,
        "exe": home.join(".soldr").join("broker").join("soldr-broker"),
        "cmd": ["soldr-broker", "broker", "serve"],
        "home": home,
        "start_time": 1_700_000_000u64,
    })
}

/// `soldr broker serve` under `home` with both pipes draining from spawn
/// (soldr#3197): the broker may outlive the test body, and an undrained
/// pipe would stall it.
fn spawn_broker(home: &Path, extra_env: &[(&str, &str)]) -> TrackedChild {
    let mut command = common::isolated_soldr_command();
    command
        .args(["broker", "serve"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    common::tracked_child::spawn_tracked(&mut command).expect("spawn soldr broker serve")
}

fn soldr_under(home: &Path) -> Command {
    let mut command = common::isolated_soldr_command();
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("SOLDR_CACHE_DIR", home.join("cache"))
        // These tests are about the inventory, not about spawning brokers.
        .env("SOLDR_BROKER_AUTOSPAWN", "0")
        .stdin(Stdio::null());
    command
}

fn run(command: &mut Command) -> (String, String, i32) {
    let out = command.output().expect("run soldr");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn purge_dry_run_lists_other_homes_and_spares_the_own_one() {
    let root = common::unique_temp_dir("broker-purge-dry-run");
    let own = root.join("own");
    let other = root.join("other");
    std::fs::create_dir_all(&other).expect("mkdir");
    let gone = root.join("gone");
    // Rows use this test's own pid: a dry run must not signal anything, and
    // if it did, the test would be the casualty rather than a bystander.
    let me = std::process::id();
    let table = scripted_table(
        &root,
        &[
            broker_row(me, &own),
            broker_row(me, &other),
            broker_row(me, &gone),
        ],
    );
    let (stdout, stderr, code) = run(soldr_under(&own).env(PROCESS_LIST_ENV, &table).args([
        "broker",
        "purge",
        "--dry-run",
        "--json",
    ]));
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("json report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["stopped"], 0);
    let rows = report["processes"].as_array().expect("processes");
    let homes: Vec<&str> = rows.iter().map(|r| r["home"].as_str().unwrap()).collect();
    assert_eq!(homes, vec![other.to_str().unwrap(), gone.to_str().unwrap()]);
    assert_eq!(rows[0]["home_present"], true);
    assert_eq!(rows[1]["home_present"], false);
    assert!(rows.iter().all(|r| r["outcome"] == "would_stop"));

    let (text, _, code) =
        run(soldr_under(&own)
            .env(PROCESS_LIST_ENV, &table)
            .args(["broker", "purge", "--dry-run"]));
    assert_eq!(code, 0);
    assert!(text.contains("would stop 2 of 2"), "{text}");
    assert!(text.contains("[missing]"), "{text}");
}

#[test]
fn purge_stops_a_broker_for_another_home_and_reports_it() {
    let root = common::unique_temp_dir("broker-purge-live");
    let own = root.join("own");
    let fixture_home = root.join("fixture");
    std::fs::create_dir_all(&fixture_home).expect("mkdir");
    let mut broker = spawn_broker(&fixture_home, &[]);
    assert!(
        broker.wait_for_stdout("stable endpoint bound at", Instant::now() + READY_TIMEOUT),
        "broker never bound"
    );
    let broker_pid = broker.pid();
    let table = scripted_table(
        &root,
        &[
            broker_row(std::process::id(), &own),
            broker_row(broker_pid, &fixture_home),
        ],
    );
    let (stdout, stderr, code) = run(soldr_under(&own)
        .env(PROCESS_LIST_ENV, &table)
        .args(["broker", "purge", "--json"]));
    // Reaped here (pipes drained since spawn); the report says how it was
    // stopped.
    let _ = broker.wait_bounded(READY_TIMEOUT);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("json report");
    assert_eq!(report["stopped"], 1, "{stdout}");
    assert_eq!(report["failed"], 0, "{stdout}");
    let rows = report["processes"].as_array().expect("processes");
    assert_eq!(
        rows.len(),
        1,
        "the own-HOME row must never appear: {stdout}"
    );
    assert_eq!(rows[0]["pid"], broker_pid);
    assert!(
        matches!(rows[0]["outcome"].as_str(), Some("terminated" | "forced")),
        "{stdout}"
    );
}

#[test]
fn doctor_reports_the_inventory_in_both_renders() {
    let root = common::unique_temp_dir("broker-purge-doctor");
    let own = root.join("own");
    std::fs::create_dir_all(&own).expect("mkdir");
    let table = scripted_table(
        &root,
        &[broker_row(std::process::id(), &root.join("elsewhere"))],
    );
    let (stdout, stderr, code) = run(soldr_under(&own)
        .current_dir(&own)
        .env(PROCESS_LIST_ENV, &table)
        .args(["doctor", "--json"]));
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let doctor: serde_json::Value = serde_json::from_str(&stdout).expect("doctor json");
    let inventory = &doctor["leaked_processes"];
    assert_eq!(inventory["own_home"], own.to_str().unwrap());
    assert_eq!(inventory["leaked"].as_array().map(Vec::len), Some(1));
    assert_eq!(inventory["leaked"][0]["role"], "broker");

    let (stdout, _, code) = run(soldr_under(&own)
        .current_dir(&own)
        .env(PROCESS_LIST_ENV, &table)
        .arg("doctor"));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("soldr processes for other HOMEs:"),
        "{stdout}"
    );
    assert!(stdout.contains("1 broker(s), 0 daemon(s)"), "{stdout}");
    assert!(stdout.contains("soldr broker purge"), "{stdout}");
}

#[test]
fn the_leak_notice_fires_once_per_interval_and_never_for_machine_output() {
    let root = common::unique_temp_dir("broker-purge-toast");
    let own = root.join("own");
    std::fs::create_dir_all(&own).expect("mkdir");
    let table = scripted_table(
        &root,
        &[broker_row(std::process::id(), &root.join("elsewhere"))],
    );
    let notice = |extra: &[(&str, &str)], args: &[&str]| {
        let mut command = soldr_under(&own);
        command
            .current_dir(&own)
            .env(PROCESS_LIST_ENV, &table)
            .env("SOLDR_BROKER_LEAK_TOAST", "always")
            .args(args);
        for (key, value) in extra {
            command.env(key, value);
        }
        let (_, stderr, _) = run(&mut command);
        stderr.contains("leaked soldr-broker")
    };
    assert!(
        notice(&[], &["doctor"]),
        "first interactive invocation warns"
    );
    assert!(
        !notice(&[], &["doctor"]),
        "the stamp suppresses a repeat inside the interval"
    );
    assert!(
        notice(
            &[("SOLDR_BROKER_LEAK_TOAST_INTERVAL_SECS", "0")],
            &["doctor"]
        ),
        "a zero interval warns every time"
    );
    assert!(
        !notice(
            &[("SOLDR_BROKER_LEAK_TOAST_INTERVAL_SECS", "0")],
            &["doctor", "--json"]
        ),
        "machine-parsed output is never interrupted"
    );
    assert!(
        !notice(
            &[
                ("SOLDR_BROKER_LEAK_TOAST_INTERVAL_SECS", "0"),
                ("SOLDR_BROKER_LEAK_TOAST", "0")
            ],
            &["doctor"]
        ),
        "the silencer wins"
    );
    assert!(
        !notice(
            &[("SOLDR_BROKER_LEAK_TOAST_INTERVAL_SECS", "0")],
            &["broker", "status"]
        ),
        "a `soldr broker` verb is the remedy surface, not a place to nag"
    );
}

#[test]
fn the_autospawn_switch_leaves_no_broker_behind() {
    let home = common::unique_temp_dir("broker-purge-no-autospawn");
    std::fs::create_dir_all(&home).expect("mkdir");
    let (stdout, stderr, code) = run(soldr_under(&home).current_dir(&home).arg("doctor"));
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    let (status, _, code) = run(soldr_under(&home).args(["broker", "status"]));
    assert_eq!(code, 0);
    assert!(
        status.contains("not running"),
        "no broker may be spawned: {status}"
    );
}

#[test]
fn an_idle_broker_stands_itself_down() {
    let home = common::unique_temp_dir("broker-purge-idle");
    std::fs::create_dir_all(&home).expect("mkdir");
    let mut broker = spawn_broker(&home, &[("SOLDR_BROKER_IDLE_EXIT_SECS", "1")]);
    assert!(
        broker.wait_for_stdout("stable endpoint bound at", Instant::now() + READY_TIMEOUT),
        "broker never bound"
    );
    let status = broker.wait_for_exit(READY_TIMEOUT);
    let output = broker.wait_bounded(Duration::ZERO);
    let stderr = output.stderr_lossy();
    let status = status.unwrap_or_else(|| {
        panic!("an idle broker must stand down within the idle window\n{stderr}")
    });
    assert!(
        status.success(),
        "clean exit expected: {status:?}\n{stderr}"
    );
    assert!(stderr.contains("standing down"), "{stderr}");
}
