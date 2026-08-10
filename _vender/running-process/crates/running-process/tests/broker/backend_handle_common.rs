#![allow(dead_code)]

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::backend_lifecycle::identity::DaemonProcess;
use running_process::broker::backend_lifecycle::probe::handle_endpoint_probe;
use running_process::broker::protocol::Endpoint;
use running_process::broker::server::local_socket_name;

pub fn test_endpoint() -> Endpoint {
    Endpoint {
        namespace_id: "test-namespace".to_string(),
        path: test_endpoint_path(),
    }
}

pub fn current_daemon() -> DaemonProcess {
    DaemonProcess::current_process(test_endpoint(), Some(30)).unwrap()
}

pub fn verified_current_backend(service_name: &str, service_version: &str) -> BackendHandle {
    let daemon = current_daemon();
    verified_backend_from_daemon(service_name, service_version, &daemon)
}

pub fn verified_backend_from_daemon(
    service_name: &str,
    service_version: &str,
    daemon: &DaemonProcess,
) -> BackendHandle {
    let server = spawn_endpoint_probe_once(daemon.clone());
    let handle = BackendHandle::probe_with_service(
        service_name,
        service_version,
        &daemon.ipc_endpoint,
        daemon,
    )
    .unwrap();
    server.join().unwrap().unwrap();
    handle
}

pub fn spawn_endpoint_probe_once(daemon: DaemonProcess) -> thread::JoinHandle<io::Result<()>> {
    let endpoint_path = daemon.ipc_endpoint.path.clone();
    spawn_endpoint_probe_response_once(endpoint_path, daemon)
}

pub fn spawn_endpoint_probe_response_once(
    endpoint_path: String,
    response_daemon: DaemonProcess,
) -> thread::JoinHandle<io::Result<()>> {
    let display_path = endpoint_path.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&endpoint_path, &ready_tx)?;
        let mut stream = listener.accept()?;
        handle_endpoint_probe(&mut stream, &response_daemon)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&endpoint_path);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_path);
    handle
}

pub fn spawn_endpoint_accept_then_close_once(
    endpoint_path: String,
) -> thread::JoinHandle<io::Result<()>> {
    let display_path = endpoint_path.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&endpoint_path, &ready_tx)?;
        let _stream = listener.accept()?;
        cleanup_test_socket(&endpoint_path);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_path);
    handle
}

/// Bind the test listener and report setup success/failure through the
/// readiness channel so the spawning test panics with the real bind error
/// instead of an opaque `Disconnected` from a dropped sender.
fn bind_ready_test_socket(
    socket_name: &str,
    ready_tx: &mpsc::Sender<Result<(), String>>,
) -> io::Result<interprocess::local_socket::Listener> {
    match bind_test_socket(socket_name) {
        Ok(listener) => {
            ready_tx.send(Ok(())).unwrap();
            Ok(listener)
        }
        Err(err) => {
            let _ = ready_tx.send(Err(err.to_string()));
            Err(err)
        }
    }
}

fn await_test_socket_ready(ready_rx: &mpsc::Receiver<Result<(), String>>, display_path: &str) {
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("failed to bind backend-handle test socket {display_path}: {err}"),
        Err(err) => {
            panic!("timed out waiting for backend-handle test socket {display_path}: {err}")
        }
    }
}

pub fn impossible_pid() -> u32 {
    i32::MAX as u32
}

fn test_endpoint_path() -> String {
    #[cfg(windows)]
    {
        format!(
            r"\\.\pipe\running-process-backend-handle-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )
    }

    #[cfg(unix)]
    {
        // Keep the leaf name short: macOS temp dirs live under deep
        // `/var/folders/...` paths and `sun_path` tops out around 104
        // bytes, so a verbose name makes the bind fail deterministically.
        std::env::temp_dir()
            .join(format!(
                "rpb-v1-bht-{}-{}.sock",
                std::process::id(),
                unique_suffix()
            ))
            .to_string_lossy()
            .into_owned()
    }
}

fn bind_test_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    prepare_test_socket(socket_name)?;
    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

fn prepare_test_socket(socket_name: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }

    #[cfg(windows)]
    let _ = socket_name;

    Ok(())
}

fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
