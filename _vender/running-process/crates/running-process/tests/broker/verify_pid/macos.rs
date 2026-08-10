#![cfg(all(feature = "client", target_os = "macos"))]

use std::process::Command;

use running_process::broker::backend_lifecycle::identity::DaemonProcess;
use running_process::broker::backend_lifecycle::verify_pid::{
    process_is_alive, verify_daemon_process, VerifyPidError,
};
use running_process::broker::protocol::Endpoint;
use sha2::{Digest, Sha256};

use crate::backend_handle_common::{current_daemon, impossible_pid};

#[test]
fn macos_verify_current_process_identity() {
    assert!(process_is_alive(std::process::id()));

    let handle = verify_daemon_process(&current_daemon()).unwrap();
    assert_eq!(handle.pid(), std::process::id());
    assert!(handle.is_alive());
}

#[test]
fn macos_rejects_missing_pid() {
    let missing_pid = impossible_pid();
    assert!(!process_is_alive(missing_pid));

    let mut daemon = current_daemon();
    daemon.pid = missing_pid;

    match verify_daemon_process(&daemon) {
        Err(VerifyPidError::NotFound { pid }) => assert_eq!(pid, missing_pid),
        Err(err) => panic!("expected missing pid error, got {err:?}"),
        Ok(_) => panic!("missing pid unexpectedly verified"),
    }
}

#[test]
fn macos_process_handle_latches_exit_after_event_is_consumed() {
    let sleeper = "/bin/sleep";
    let mut child = Command::new(sleeper).arg("30").spawn().unwrap();
    let handle = verify_daemon_process(&DaemonProcess {
        pid: child.id(),
        exe_path: sleeper.into(),
        exe_sha256: sha256_file(sleeper),
        boot_id: running_process::broker::host_identity::current().boot_id,
        ipc_endpoint: Endpoint {
            namespace_id: "test-namespace".into(),
            path: "test-sleep.sock".into(),
        },
        started_at_unix_ms: 0,
        idle_timeout_secs: Some(30),
    })
    .unwrap();

    child.kill().unwrap();
    child.wait().unwrap();

    assert!(!handle.is_alive());
    assert!(!handle.is_alive());
}

fn sha256_file(path: &str) -> [u8; 32] {
    let bytes = std::fs::read(path).unwrap();
    let digest = Sha256::digest(&bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    out
}
