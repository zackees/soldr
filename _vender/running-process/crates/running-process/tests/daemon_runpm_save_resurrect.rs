#![cfg(feature = "daemon")]
//! Phase 4 integration tests for the runpm save/resurrect snapshot
//! pipeline (#427).
//!
//! These tests exercise [`save_snapshot`] and [`resurrect_from_snapshot`]
//! directly against an in-process `ServiceRegistry`. We bypass the IPC
//! layer because the focus here is the persistence pipeline — the daemon
//! handlers themselves are unit-thin wrappers and are exercised in
//! `daemon_runpm_service_stubs.rs`.

use std::path::Path;

use running_process::daemon::services::{ServiceDef, ServiceRegistry, ServiceStatus};
use running_process::daemon::services_snapshot::{
    resurrect_from_snapshot, save_snapshot, SNAPSHOT_FILE_NAME, SNAPSHOT_VERSION,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn registry() -> (ServiceRegistry, TempDir) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("svc.sqlite3");
    let logs = tmp.path().join("services");
    let reg = ServiceRegistry::open(&db, logs).unwrap();
    (reg, tmp)
}

/// A cross-platform command that runs ~forever so the service stays
/// `online` long enough to be observed mid-test.
fn long_lived_cmd() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            "cmd".into(),
            "/C".into(),
            "ping -n 300 127.0.0.1 > NUL".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec!["sleep".into(), "300".into()]
    }
}

fn def(name: &str, cmd: Vec<String>) -> ServiceDef {
    ServiceDef {
        name: name.to_string(),
        cmd,
        cwd: String::new(),
        env: Vec::new(),
        autorestart: false,
        max_restarts: 0,
        restart_delay_ms: 0,
        kill_timeout_ms: 500,
        min_uptime_ms: 0,
    }
}

/// Read the snapshot JSON file and return the parsed envelope value.
fn read_snapshot(path: &Path) -> serde_json::Value {
    let bytes = std::fs::read(path).expect("snapshot file should exist");
    serde_json::from_slice(&bytes).expect("snapshot should be valid JSON")
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

#[test]
fn save_writes_snapshot_to_local_scope() {
    let (reg, tmp) = registry();
    let _ = reg.start(def("alpha", long_lived_cmd())).unwrap();
    let _ = reg.start(def("beta", long_lived_cmd())).unwrap();

    let (path, count) = save_snapshot(&reg).expect("save_snapshot should succeed");
    assert_eq!(count, 2, "save should report exactly 2 services");
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        SNAPSHOT_FILE_NAME,
        "snapshot must use the canonical filename"
    );
    // Snapshot lives next to the SQLite DB.
    let expected_parent = tmp.path();
    assert_eq!(
        path.parent().unwrap(),
        expected_parent,
        "snapshot must be co-located with the sqlite db"
    );
    assert!(path.is_file(), "{} must exist on disk", path.display());

    let envelope = read_snapshot(&path);
    assert_eq!(
        envelope["version"].as_u64().unwrap_or(0),
        SNAPSHOT_VERSION as u64,
        "version field must be the supported snapshot version"
    );
    let services = envelope["services"].as_array().expect("services array");
    assert_eq!(services.len(), 2, "snapshot should carry both services");
    let mut names: Vec<&str> = services
        .iter()
        .map(|s| s["name"].as_str().unwrap_or(""))
        .collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta"]);

    // Clean up the live children before the temp dir disappears.
    let _ = reg.stop("all");
}

// ---------------------------------------------------------------------------
// Resurrect — definitions only
// ---------------------------------------------------------------------------

#[test]
fn resurrect_restores_definitions_from_snapshot() {
    let (reg, _tmp) = registry();
    let _ = reg.start(def("alpha", long_lived_cmd())).unwrap();
    let _ = reg.start(def("beta", long_lived_cmd())).unwrap();
    save_snapshot(&reg).expect("save should succeed");
    // Drop the services from the table so resurrect has work to do.
    let _ = reg.delete("all").unwrap();
    assert_eq!(reg.list().unwrap().len(), 0, "table should be empty");

    let (restored, _restarted) = resurrect_from_snapshot(&reg).expect("resurrect should succeed");
    assert_eq!(restored, 2, "exactly 2 definitions should be restored");

    let all = reg.list().unwrap();
    let mut names: Vec<&str> = all.iter().map(|r| r.def.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["alpha", "beta"]);

    let _ = reg.stop("all");
}

// ---------------------------------------------------------------------------
// Resurrect — restart policy
// ---------------------------------------------------------------------------

#[test]
fn resurrect_restarts_only_previously_online_services() {
    let (reg, _tmp) = registry();
    // A is online at save time.
    let _ = reg.start(def("a-online", long_lived_cmd())).unwrap();
    // B exists in the table but is not started.
    reg.register_def(def("b-stopped", long_lived_cmd()))
        .unwrap();

    save_snapshot(&reg).expect("save should succeed");
    let _ = reg.delete("all").unwrap();

    let (restored, restarted) = resurrect_from_snapshot(&reg).expect("resurrect should succeed");
    assert_eq!(restored, 2, "both definitions should be restored");
    assert_eq!(
        restarted, 1,
        "only the previously-online service should be restarted"
    );

    let a = reg.describe("a-online").unwrap();
    let b = reg.describe("b-stopped").unwrap();
    assert_eq!(
        a.status,
        ServiceStatus::Online,
        "a-online should resume in online state"
    );
    assert_eq!(
        b.status,
        ServiceStatus::Stopped,
        "b-stopped should stay stopped"
    );
    let _ = reg.stop("all");
}

// ---------------------------------------------------------------------------
// Resurrect — missing snapshot
// ---------------------------------------------------------------------------

#[test]
fn resurrect_with_no_snapshot_returns_not_found() {
    let (reg, _tmp) = registry();
    let err = resurrect_from_snapshot(&reg).expect_err("should fail with no snapshot");
    let msg = err.to_string();
    let snapshot_path = reg.snapshot_path().to_string_lossy().into_owned();
    assert!(
        msg.contains("no snapshot"),
        "error message should explain the cause; got {msg:?}"
    );
    assert!(
        msg.contains(&snapshot_path) || msg.contains(SNAPSHOT_FILE_NAME),
        "error should name the missing path; got {msg:?}"
    );
}

// ---------------------------------------------------------------------------
// Resurrect — idempotency
// ---------------------------------------------------------------------------

#[test]
fn resurrect_is_idempotent() {
    let (reg, _tmp) = registry();
    // Register two definitions without launching either, so the second
    // resurrect doesn't have to deal with already-live children.
    reg.register_def(def("svc-a", long_lived_cmd())).unwrap();
    reg.register_def(def("svc-b", long_lived_cmd())).unwrap();
    save_snapshot(&reg).expect("save should succeed");

    let _ = resurrect_from_snapshot(&reg).expect("first resurrect should succeed");
    let after_first = reg.list().unwrap();
    assert_eq!(after_first.len(), 2);

    let _ = resurrect_from_snapshot(&reg).expect("second resurrect should succeed");
    let after_second = reg.list().unwrap();
    assert_eq!(
        after_second.len(),
        2,
        "second resurrect should not duplicate rows"
    );
    let mut names: Vec<&str> = after_second.iter().map(|r| r.def.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["svc-a", "svc-b"]);
}

// ---------------------------------------------------------------------------
// Resurrect — bad version
// ---------------------------------------------------------------------------

#[test]
fn resurrect_rejects_unknown_snapshot_version() {
    let (reg, _tmp) = registry();
    // Write a snapshot file by hand with a bogus version.
    let path = reg.snapshot_path().to_path_buf();
    let payload = serde_json::json!({
        "version": 999_999_u32,
        "saved_at_ms": 0u64,
        "services": []
    });
    std::fs::write(&path, payload.to_string()).unwrap();
    let err = resurrect_from_snapshot(&reg).expect_err("unknown version must not resurrect");
    let msg = err.to_string();
    assert!(
        msg.contains("999999") || msg.contains("not supported"),
        "error should explain version mismatch; got {msg:?}"
    );
}
