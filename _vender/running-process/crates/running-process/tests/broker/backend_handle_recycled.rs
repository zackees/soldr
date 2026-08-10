#![cfg(feature = "client")]

use running_process::broker::backend_handle::BackendHandle;

use crate::backend_handle_common::current_daemon;

#[test]
fn probe_rejects_executable_sha_mismatch() {
    let mut daemon = current_daemon();
    daemon.exe_sha256[0] ^= 0xFF;

    assert!(BackendHandle::probe(&daemon.ipc_endpoint, &daemon).is_none());
}
