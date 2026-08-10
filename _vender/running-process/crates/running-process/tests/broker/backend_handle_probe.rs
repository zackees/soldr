#![cfg(feature = "client")]

use running_process::broker::backend_handle::{BackendHandle, BackendHandleError};
use running_process::broker::backend_lifecycle::probe::{EndpointProbeError, ProbeError};
use running_process::broker::backend_lifecycle::verify_pid::VerifyPidError;

use crate::backend_handle_common::{
    current_daemon, spawn_endpoint_accept_then_close_once, spawn_endpoint_probe_response_once,
    verified_backend_from_daemon,
};

#[test]
fn probe_current_process_returns_live_handle() {
    let daemon = current_daemon();
    let handle = verified_backend_from_daemon("zccache", "1.2.3", &daemon);

    assert_eq!(handle.service_name, "zccache");
    assert_eq!(handle.service_version, "1.2.3");
    assert_eq!(handle.daemon_process.pid, std::process::id());
    assert!(handle.is_alive());
}

#[test]
fn probe_rejects_endpoint_that_does_not_answer_identity_probe() {
    let daemon = current_daemon();
    let endpoint_path = daemon.ipc_endpoint.path.clone();
    let server = spawn_endpoint_accept_then_close_once(endpoint_path);

    let result =
        BackendHandle::probe_with_service("zccache", "1.2.3", &daemon.ipc_endpoint, &daemon);

    server.join().unwrap().unwrap();
    assert!(matches!(
        result,
        Err(BackendHandleError::Probe(ProbeError::EndpointResponse(
            EndpointProbeError::Timeout | EndpointProbeError::Io(_)
        )))
    ));
}

#[test]
fn probe_rejects_endpoint_response_identity_mismatch() {
    let expected = current_daemon();
    let mut response = expected.clone();
    response.boot_id = "different-boot-id".into();
    let server = spawn_endpoint_probe_response_once(expected.ipc_endpoint.path.clone(), response);

    let result =
        BackendHandle::probe_with_service("zccache", "1.2.3", &expected.ipc_endpoint, &expected);

    server.join().unwrap().unwrap();
    assert!(matches!(
        result,
        Err(BackendHandleError::Probe(ProbeError::EndpointResponse(
            EndpointProbeError::IdentityMismatch { field: "boot_id" }
        )))
    ));
}

#[test]
fn probe_rejects_executable_path_mismatch_even_when_hash_matches() {
    let mut daemon = current_daemon();
    let fake_exe = std::env::temp_dir().join(format!(
        "running-process-backend-handle-copy-{}{}",
        std::process::id(),
        std::env::consts::EXE_SUFFIX
    ));
    let _ = std::fs::remove_file(&fake_exe);
    std::fs::copy(&daemon.exe_path, &fake_exe).unwrap();

    daemon.exe_path = fake_exe.clone();
    let result =
        BackendHandle::probe_with_service("zccache", "1.2.3", &daemon.ipc_endpoint, &daemon);
    let _ = std::fs::remove_file(&fake_exe);

    assert!(matches!(
        result,
        Err(BackendHandleError::Probe(ProbeError::VerifyPid(
            VerifyPidError::ExePathMismatch { .. }
        )))
    ));
}
