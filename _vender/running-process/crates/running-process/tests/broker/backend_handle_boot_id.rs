#![cfg(feature = "client")]

use running_process::broker::backend_handle::BackendHandle;

use crate::backend_handle_common::current_daemon;

#[test]
fn probe_rejects_prior_boot_identity() {
    let mut daemon = current_daemon();
    daemon.boot_id = format!("{}-prior-boot", daemon.boot_id);

    assert!(BackendHandle::probe(&daemon.ipc_endpoint, &daemon).is_none());
}
