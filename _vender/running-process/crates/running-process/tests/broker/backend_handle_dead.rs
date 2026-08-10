#![cfg(feature = "client")]

use running_process::broker::backend_handle::BackendHandle;

use crate::backend_handle_common::{current_daemon, impossible_pid};

#[test]
fn probe_dead_pid_returns_none() {
    let mut daemon = current_daemon();
    daemon.pid = impossible_pid();

    assert!(BackendHandle::probe(&daemon.ipc_endpoint, &daemon).is_none());
}
