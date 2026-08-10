//! `running-process-broker-v1 servicedef install` CLI tests (#386).
//!
//! The subcommand is the postinstall-style shell surface over
//! `write_service_definition`: package installers call it to land a
//! `.servicedef` into the platform-default (or overridden) directory.
//! These tests drive the real binary end-to-end: install → load via the
//! broker's loader → `doctor` PASS on the freshly installed definition.

#![cfg(feature = "daemon")]

use std::path::Path;
use std::process::{Command, Output};

use running_process::broker::server::ServiceDefinitionLoader;

fn broker_cli() -> &'static str {
    env!("CARGO_BIN_EXE_running-process-broker-v1")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn install_args(service: &str, dir: &Path) -> Vec<String> {
    vec![
        "servicedef".into(),
        "install".into(),
        "--service".into(),
        service.into(),
        "--binary-path".into(),
        broker_cli().into(),
        "--min-version".into(),
        "1.2.3".into(),
        "--service-def-dir".into(),
        dir.to_string_lossy().into_owned(),
    ]
}

fn doctor_checks(dir: &Path) -> (i32, serde_json::Value) {
    let output = Command::new(broker_cli())
        .args([
            "doctor",
            "--json",
            "--service-def-dir",
            &dir.to_string_lossy(),
        ])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_str(stdout(&output).trim())
        .unwrap_or_else(|err| panic!("doctor emitted invalid JSON: {err}\n{}", stdout(&output)));
    (output.status.code().unwrap_or(-1), report)
}

fn check_status(report: &serde_json::Value, name: &str) -> String {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["check"] == name)
        .unwrap_or_else(|| panic!("doctor report has no check {name:?}: {report}"))["status"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn servicedef_install_writes_loadable_definition_and_doctor_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");

    let output = Command::new(broker_cli())
        .args(install_args("rp-cli-proof", &root))
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install failed: {}",
        stderr(&output)
    );
    let payload: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(payload["service_name"], "rp-cli-proof");
    assert_eq!(payload["dir_source"], "flag:--service-def-dir");
    assert_eq!(
        Path::new(payload["path"].as_str().unwrap()),
        root.join("rp-cli-proof.servicedef")
    );

    // The broker's own Hello-path loader accepts the installed file.
    let loaded = ServiceDefinitionLoader::new(&root)
        .lookup_or_reload("rp-cli-proof")
        .unwrap();
    assert_eq!(loaded.service_name, "rp-cli-proof");
    assert_eq!(loaded.min_version, "1.2.3");

    // doctor reports the directory and the fresh definition as healthy.
    let (_, report) = doctor_checks(&root);
    assert_eq!(check_status(&report, "servicedef:dir"), "PASS");
    assert_eq!(
        check_status(&report, "servicedef:rp-cli-proof.servicedef"),
        "PASS"
    );
}

#[test]
fn servicedef_install_env_override_reports_env_dir_source() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("env-services");

    let output = Command::new(broker_cli())
        .args([
            "servicedef",
            "install",
            "--service",
            "rp-env-proof",
            "--binary-path",
            broker_cli(),
            "--json",
        ])
        .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install failed: {}",
        stderr(&output)
    );
    let payload: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(payload["dir_source"], "env:RUNNING_PROCESS_SERVICE_DEF_DIR");
    assert!(root.join("rp-env-proof.servicedef").is_file());
}

#[test]
fn servicedef_install_rejects_relative_binary_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");

    let output = Command::new(broker_cli())
        .args([
            "servicedef",
            "install",
            "--service",
            "rp-bad-proof",
            "--binary-path",
            "relative/backend",
            "--service-def-dir",
            &root.to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("must be absolute"));
    assert!(!root.join("rp-bad-proof.servicedef").exists());
}

#[test]
fn servicedef_install_requires_service_and_binary_path() {
    let output = Command::new(broker_cli())
        .args(["servicedef", "install"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--service is required"));
}

#[cfg(unix)]
#[test]
fn doctor_fails_servicedef_dir_check_when_dir_is_world_writable() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");

    let output = Command::new(broker_cli())
        .args(install_args("rp-perm-proof", &root))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install failed: {}",
        stderr(&output)
    );

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
    let (exit_code, report) = doctor_checks(&root);
    assert_eq!(check_status(&report, "servicedef:dir"), "FAIL");
    assert_eq!(exit_code, 1);
}
