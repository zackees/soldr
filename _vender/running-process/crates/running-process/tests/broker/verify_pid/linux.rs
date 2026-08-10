#![cfg(all(feature = "client", target_os = "linux"))]

use running_process::broker::backend_lifecycle::verify_pid::{
    process_is_alive, verify_daemon_process,
};

use crate::backend_handle_common::{current_daemon, impossible_pid};

#[test]
fn linux_verify_current_process_identity() {
    let handle = verify_daemon_process(&current_daemon()).unwrap();
    assert_eq!(handle.pid(), std::process::id());
    assert!(handle.is_alive());
}

#[test]
fn linux_rejects_missing_pid() {
    assert!(!process_is_alive(impossible_pid()));

    let mut daemon = current_daemon();
    daemon.pid = impossible_pid();
    assert!(verify_daemon_process(&daemon).is_err());
}
