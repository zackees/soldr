//! Shared per-test local-socket naming and bind-readiness helpers.
//!
//! Used by the `broker` harness directly and by the `security` harness via
//! `#[path = "../broker/socket_common.rs"]`.

#![allow(dead_code)]

use std::io;
use std::sync::mpsc;
use std::time::Duration;

use interprocess::local_socket::ListenerOptions;
use running_process::broker::server::local_socket_name;

/// Build a unique per-test IPC endpoint name keyed by `label`.
///
/// On Windows the readable label survives verbatim — named-pipe names have
/// no meaningful length limit. On Unix the whole socket path must fit in
/// `sun_path`, which tops out around 104 bytes on macOS, where temp dirs
/// already live under deep `/var/folders/...` paths (~45-50 chars). A
/// verbose leaf name makes the bind fail deterministically, so the leaf
/// keeps only an 8-hex-char hash of the label plus a short pid/nanos
/// suffix (~35 chars total).
pub fn unique_socket_name(label: &str) -> String {
    #[cfg(windows)]
    {
        format!("rpb-v1-{label}-{}-{}", std::process::id(), unique_suffix())
    }

    #[cfg(unix)]
    {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        label.hash(&mut hasher);
        let label_hash = hasher.finish() as u32;
        let short_suffix = unique_suffix() % 1_000_000_000;
        std::env::temp_dir()
            .join(format!(
                "rpb-{label_hash:08x}-{}-{short_suffix}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned()
    }
}

/// Bind a fresh test listener on `socket_name`, clearing any stale Unix
/// socket file first.
pub fn bind_test_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    prepare_test_socket(socket_name)?;
    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

/// Bind the test listener and report setup success/failure through the
/// readiness channel so the spawning test panics with the real bind error
/// instead of an opaque `Disconnected` from a dropped sender.
pub fn bind_ready_test_socket(
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

/// Wait for the paired listener thread's readiness report, surfacing the
/// real bind error (and the socket path) on failure.
pub fn await_test_socket_ready(ready_rx: &mpsc::Receiver<Result<(), String>>, display_path: &str) {
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("failed to bind test socket {display_path}: {err}"),
        Err(err) => panic!("timed out waiting for test socket {display_path}: {err}"),
    }
}

pub fn prepare_test_socket(socket_name: &str) -> io::Result<()> {
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

pub fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
