//! Integration tests for the read-only `broker doctor` diagnostics (#354).

#![cfg(feature = "client")]

use std::io;
use std::sync::mpsc;
use std::thread;

use interprocess::local_socket::prelude::*;

use running_process::broker::client::{
    RUNNING_PROCESS_DISABLE_ENV, RUNNING_PROCESS_FAKE_BACKEND_ENV,
};
use running_process::broker::doctor::{
    broker_endpoint_check, env_var_checks, inode_pressure_check, platform_path_budget_check,
    run_doctor, service_definition_checks, systemd_killmode_check, version_check, DoctorCheck,
    DoctorOptions, DoctorReport, DoctorStatus,
};
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    handle_hello_connection, write_service_definition, HelloHandler, PeerIdentity,
};

use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, cleanup_test_socket, unique_socket_name,
};

static DOCTOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn remove(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, original }
    }

    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

fn find_check<'a>(checks: &'a [DoctorCheck], name: &str) -> &'a DoctorCheck {
    checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("missing check {name:?} in {checks:?}"))
}

fn absolute_binary_path() -> String {
    if cfg!(windows) {
        r"C:\opt\zccache\zccache.exe".into()
    } else {
        "/usr/local/bin/zccache".into()
    }
}

fn valid_service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: absolute_binary_path(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: "1.0.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Aggregation, exit codes, and JSON shape
// ---------------------------------------------------------------------------

#[test]
fn report_exit_code_fails_only_on_fail() {
    let pass_warn = DoctorReport {
        checks: vec![
            DoctorCheck {
                name: "a".into(),
                status: DoctorStatus::Pass,
                detail: "ok".into(),
            },
            DoctorCheck {
                name: "b".into(),
                status: DoctorStatus::Warn,
                detail: "meh".into(),
            },
        ],
    };
    assert_eq!(pass_warn.exit_code(), 0);

    let with_fail = DoctorReport {
        checks: vec![DoctorCheck {
            name: "c".into(),
            status: DoctorStatus::Fail,
            detail: "broken".into(),
        }],
    };
    assert_eq!(with_fail.exit_code(), 1);
}

#[test]
fn report_json_shape_is_stable() {
    let report = DoctorReport {
        checks: vec![
            DoctorCheck {
                name: "env:EXAMPLE".into(),
                status: DoctorStatus::Pass,
                detail: "unset".into(),
            },
            DoctorCheck {
                name: "servicedef:bad.servicedef".into(),
                status: DoctorStatus::Fail,
                detail: "decode failed".into(),
            },
        ],
    };
    let value: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "doctor");
    assert_eq!(value["exit_code"], 1);
    let checks = value["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["check"], "env:EXAMPLE");
    assert_eq!(checks[0]["status"], "PASS");
    assert_eq!(checks[0]["detail"], "unset");
    assert_eq!(checks[1]["status"], "FAIL");
}

#[test]
fn run_doctor_produces_named_checks_and_valid_json() {
    let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
    let _disable = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake = EnvVarGuard::remove(RUNNING_PROCESS_FAKE_BACKEND_ENV);
    let dir = tempfile::tempdir().unwrap();

    let options = DoctorOptions {
        // Unreachable endpoint: the doctor must degrade to WARN, not error.
        broker_endpoint: Some(unique_socket_name("doctor-no-broker")),
        service_definition_dir: Some(dir.path().join("missing-subdir")),
    };
    let report = run_doctor(&options);

    assert!(!report.checks.is_empty());
    for check in &report.checks {
        assert!(!check.name.is_empty());
        assert!(!check.detail.is_empty());
    }
    // Every area reported in.
    for name in [
        "env:RUNNING_PROCESS_DISABLE",
        "env:RUNNING_PROCESS_FAKE_BACKEND",
        "env:RUNNING_PROCESS_NO_TRACKING",
        "env:RUNNING_PROCESS_DAEMON_SCOPE",
        "broker:endpoint",
        "servicedef:dir",
        "sockets:runtime-dir",
        "filesystem:inodes",
        "platform:path-budget",
        "platform:systemd-killmode",
        "build:version",
    ] {
        find_check(&report.checks, name);
    }
    // No broker running and an empty servicedef dir must never FAIL.
    assert_eq!(
        report.exit_code(),
        0,
        "unexpected failure: {}",
        report.render_text()
    );
    let value: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert_eq!(value["command"], "doctor");
}

// ---------------------------------------------------------------------------
// Inode-pressure check (#390)
// ---------------------------------------------------------------------------

#[test]
fn inode_pressure_check_reports_per_platform() {
    let check = inode_pressure_check();
    assert_eq!(check.name, "filesystem:inodes");
    #[cfg(windows)]
    {
        assert_eq!(check.status, DoctorStatus::Pass);
        assert!(check.detail.contains("not applicable on Windows"));
        assert!(
            !check.detail.contains("inodes free"),
            "windows must not fake inode numbers: {}",
            check.detail
        );
    }
    #[cfg(unix)]
    {
        // A healthy dev box passes; a filesystem with no inode table
        // also passes as not-applicable. Either way it never panics and
        // never FAILs on a machine with normal headroom.
        assert_ne!(
            check.status,
            DoctorStatus::Fail,
            "unexpected inode FAIL: {}",
            check.detail
        );
    }
}

// ---------------------------------------------------------------------------
// systemd KillMode check (#391)
// ---------------------------------------------------------------------------

#[test]
fn systemd_killmode_check_never_fails() {
    let check = systemd_killmode_check();
    assert_eq!(check.name, "platform:systemd-killmode");
    // Off-Linux (and on Linux outside systemd) the check passes; under a
    // systemd unit it may WARN — but it must never FAIL.
    assert_ne!(
        check.status,
        DoctorStatus::Fail,
        "unexpected FAIL: {}",
        check.detail
    );
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(check.status, DoctorStatus::Pass);
        assert!(check.detail.contains("not applicable"));
    }
}

// ---------------------------------------------------------------------------
// Env-var checks
// ---------------------------------------------------------------------------

#[test]
fn env_checks_pass_when_unset() {
    let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
    let _disable = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake = EnvVarGuard::remove(RUNNING_PROCESS_FAKE_BACKEND_ENV);

    let checks = env_var_checks();
    let disable = find_check(&checks, "env:RUNNING_PROCESS_DISABLE");
    assert_eq!(disable.status, DoctorStatus::Pass);
    let fake = find_check(&checks, "env:RUNNING_PROCESS_FAKE_BACKEND");
    assert_eq!(fake.status, DoctorStatus::Pass);
}

#[test]
fn env_checks_warn_when_broker_disabled() {
    let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
    let _disable = EnvVarGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");
    let _fake = EnvVarGuard::remove(RUNNING_PROCESS_FAKE_BACKEND_ENV);

    let checks = env_var_checks();
    let disable = find_check(&checks, "env:RUNNING_PROCESS_DISABLE");
    assert_eq!(disable.status, DoctorStatus::Warn);
    assert!(disable.detail.contains("broker disabled"));
}

#[test]
fn env_checks_fail_on_invalid_disable_value() {
    let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
    let _disable = EnvVarGuard::set(RUNNING_PROCESS_DISABLE_ENV, "true");

    let checks = env_var_checks();
    let disable = find_check(&checks, "env:RUNNING_PROCESS_DISABLE");
    assert_eq!(disable.status, DoctorStatus::Fail);
    assert!(disable.detail.contains("true"));
}

#[test]
fn env_checks_warn_loudly_on_fake_backend_seam() {
    let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
    let _disable = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake = EnvVarGuard::set(RUNNING_PROCESS_FAKE_BACKEND_ENV, "/tmp/fake.sock");

    let checks = env_var_checks();
    let fake = find_check(&checks, "env:RUNNING_PROCESS_FAKE_BACKEND");
    assert_eq!(fake.status, DoctorStatus::Warn);
    assert!(fake.detail.contains("TEST-ONLY"));
    assert!(fake.detail.contains("never set this in production"));
}

// ---------------------------------------------------------------------------
// Service-definition checks
// ---------------------------------------------------------------------------

#[test]
fn servicedef_checks_warn_on_missing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");

    let checks = service_definition_checks(&missing);

    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].name, "servicedef:dir");
    assert_eq!(checks[0].status, DoctorStatus::Warn);
    assert!(checks[0].detail.contains("does not exist"));
}

#[test]
fn servicedef_checks_report_per_file_pass_and_fail() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("services");
    write_service_definition(&root, &valid_service_definition()).unwrap();
    std::fs::write(root.join("broken.servicedef"), [0xFF_u8, 0xFF, 0xFF, 0xFF]).unwrap();
    // Non-servicedef files are ignored entirely.
    std::fs::write(root.join("README.txt"), b"not a definition").unwrap();

    let checks = service_definition_checks(&root);

    let dir_check = find_check(&checks, "servicedef:dir");
    assert_eq!(dir_check.status, DoctorStatus::Pass);
    assert!(dir_check.detail.contains("2 .servicedef files"));

    let valid = find_check(&checks, "servicedef:zccache.servicedef");
    assert_eq!(valid.status, DoctorStatus::Pass);
    assert!(valid.detail.contains("zccache"));

    let broken = find_check(&checks, "servicedef:broken.servicedef");
    assert_eq!(broken.status, DoctorStatus::Fail);

    assert!(checks
        .iter()
        .all(|check| check.name != "servicedef:README.txt"));

    let report = DoctorReport { checks };
    assert_eq!(report.exit_code(), 1);
}

// ---------------------------------------------------------------------------
// Endpoint reachability
// ---------------------------------------------------------------------------

#[test]
fn endpoint_check_warns_when_nothing_listens() {
    let endpoint = unique_socket_name("doctor-unreachable");

    let check = broker_endpoint_check(Some(&endpoint));

    assert_eq!(check.name, "broker:endpoint");
    assert_eq!(check.status, DoctorStatus::Warn);
    assert!(check.detail.contains("no broker listening"));
    assert!(check.detail.contains(&endpoint));
}

#[test]
fn endpoint_check_probes_live_listener_via_hello() {
    let broker_endpoint = unique_socket_name("doctor-live-broker");
    let serving_endpoint = broker_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let server: thread::JoinHandle<io::Result<()>> = thread::spawn(move || {
        let listener = bind_ready_test_socket(&serving_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "doctor-test-peer".into(),
        };
        // Empty handler: the doctor's probe service is unknown, so the
        // broker refuses — which the doctor treats as proof of life.
        handle_hello_connection(&mut stream, &HelloHandler::new(), peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&serving_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &broker_endpoint);

    let check = broker_endpoint_check(Some(&broker_endpoint));

    server.join().unwrap().unwrap();
    assert_eq!(check.status, DoctorStatus::Pass, "detail: {}", check.detail);
    assert!(check.detail.contains("broker listening"));
    assert!(check.detail.contains("protocol v1"));
}

// ---------------------------------------------------------------------------
// Platform + version checks
// ---------------------------------------------------------------------------

#[test]
fn platform_path_budget_reports_length_and_limit() {
    let check = platform_path_budget_check();
    assert_eq!(check.name, "platform:path-budget");
    assert_ne!(check.status, DoctorStatus::Fail, "detail: {}", check.detail);
    assert!(check.detail.contains("bytes"));
}

#[test]
fn version_check_reports_crate_protocol_and_framing() {
    let check = version_check();
    assert_eq!(check.status, DoctorStatus::Pass);
    assert!(check.detail.contains(env!("CARGO_PKG_VERSION")));
    assert!(check.detail.contains("protocol v1"));
    assert!(check.detail.contains("framing v1"));
}
