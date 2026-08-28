//! End-to-end coverage for the `soldr doctor` standalone-zccache probe
//! (soldr#1467).
//!
//! soldr's build cache is an embedded service inside soldr-daemon;
//! standalone `zccache-daemon` processes and their per-launch
//! `*/runtime-binaries/` copies must never exist. The doctor probe
//! surfaces both. These tests drive the real binary through the
//! `SOLDR_TEST_ZCCACHE_SCAN_ROOT` / `SOLDR_TEST_PROCESS_LIST_FILE`
//! seams so no real daemon or `~/.zccache` state is touched.

use crate::common;

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Run `soldr doctor` in an isolated workspace (no rust-toolchain.toml,
/// hermetic HOME / cache dir) with the two soldr#1467 probe seams set.
fn run_doctor(workspace: &Path, scan_root: &Path, process_list: &Path, json: bool) -> Output {
    let mut command = Command::new(common::soldr_bin());
    command.arg("doctor");
    if json {
        command.arg("--json");
    }
    command
        .current_dir(workspace)
        .env("SOLDR_CACHE_DIR", workspace)
        .env("HOME", workspace)
        .env("USERPROFILE", workspace)
        .env("SOLDR_TEST_ZCCACHE_SCAN_ROOT", scan_root)
        .env("SOLDR_TEST_PROCESS_LIST_FILE", process_list)
        .output()
        .expect("failed to run soldr doctor")
}

/// Seed `<root>/v1.12.14/runtime-binaries/` with one stale daemon copy
/// and one unrelated file; return the daemon copy's path.
fn seed_stale_copy(scan_root: &Path) -> String {
    let rb = scan_root.join("v1.12.14").join("runtime-binaries");
    fs::create_dir_all(&rb).expect("create runtime-binaries");
    let stale = rb.join("zccache-daemon.772359644.exe");
    fs::write(&stale, b"stub").expect("write stale daemon copy");
    fs::write(rb.join("zccache.exe"), b"stub").expect("write unrelated file");
    stale.display().to_string()
}

#[test]
fn doctor_json_reports_stale_copies_and_daemon_processes() {
    let workspace = common::unique_temp_dir("doctor-standalone-dirty");
    let scan_root = workspace.join("zccache-root");
    let stale = seed_stale_copy(&scan_root);
    let process_list = workspace.join("procs.txt");
    fs::write(&process_list, "4242 zccache-daemon.99\n10 cargo\n").expect("write process list");

    let output = run_doctor(&workspace, &scan_root, &process_list, true);
    assert_eq!(
        output.status.code(),
        Some(0),
        "doctor must exit 0 (the probe is advisory)\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must produce JSON");
    assert_eq!(json["zccache"]["backend"], "embedded");
    assert_eq!(
        json["zccache"]["stale_runtime_binaries"],
        serde_json::json!([stale])
    );
    assert_eq!(
        json["zccache"]["standalone_daemon_processes"],
        serde_json::json!(["zccache-daemon.99 (pid 4242)"])
    );
}

#[test]
fn doctor_human_output_warns_on_standalone_leftovers() {
    let workspace = common::unique_temp_dir("doctor-standalone-human");
    let scan_root = workspace.join("zccache-root");
    seed_stale_copy(&scan_root);
    let process_list = workspace.join("procs.txt");
    fs::write(&process_list, "4242 zccache-daemon.99\n").expect("write process list");

    let output = run_doctor(&workspace, &scan_root, &process_list, false);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("standalone zccache: 1 running daemon process(es) / 1 stale"),
        "human output must warn with counts:\n{stdout}"
    );
    assert!(
        stdout.contains("soldr#1467"),
        "warning must reference the issue:\n{stdout}"
    );
    assert!(
        stdout.contains("zccache-daemon.99 (pid 4242)"),
        "warning must list the daemon process:\n{stdout}"
    );
    assert!(
        stdout.contains("zccache-daemon.772359644.exe"),
        "warning must list the stale copy:\n{stdout}"
    );
}

#[test]
fn doctor_reports_clean_baseline_when_no_leftovers() {
    let workspace = common::unique_temp_dir("doctor-standalone-clean");
    let scan_root = workspace.join("zccache-root");
    fs::create_dir_all(scan_root.join("v1.12.15").join("runtime-binaries"))
        .expect("create empty runtime-binaries");
    let process_list = workspace.join("procs.txt");
    fs::write(&process_list, "10 cargo\n11 rustc\n").expect("write process list");

    let output = run_doctor(&workspace, &scan_root, &process_list, true);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value =
        serde_json::from_slice(&output.stdout).expect("doctor --json must produce JSON");
    assert_eq!(
        json["zccache"]["stale_runtime_binaries"],
        serde_json::json!([])
    );
    assert_eq!(
        json["zccache"]["standalone_daemon_processes"],
        serde_json::json!([])
    );

    let human = run_doctor(&workspace, &scan_root, &process_list, false);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("standalone zccache: none detected"),
        "clean box gets the quiet one-liner:\n{stdout}"
    );
}
