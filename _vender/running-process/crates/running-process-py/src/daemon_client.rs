//! Minimal fire-and-forget daemon IPC client for process registration.
//!
//! Sends Register/Unregister messages to the running-process daemon without
//! waiting for responses.  If the daemon is not running or the send fails,
//! errors are silently ignored so that the in-memory registry remains the
//! primary tracking mechanism.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use prost::Message;
use running_process::proto::daemon::{
    DaemonRequest, RegisterRequest, RequestType, UnregisterRequest,
};

// ---------------------------------------------------------------------------
// Static state
// ---------------------------------------------------------------------------

/// Cached once from the `RUNNING_PROCESS_NO_TRACKING` environment variable.
static TRACKING_CHECKED: AtomicBool = AtomicBool::new(false);
static TRACKING_DISABLED: AtomicBool = AtomicBool::new(false);
const NO_TRACKING_ENV: &str = "RUNNING_PROCESS_NO_TRACKING";

/// Monotonically increasing request id.
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Cached connection to the daemon.  `None` means either never connected or
/// the previous connection was broken and will be retried on next call.
static CONNECTION: Mutex<Option<interprocess::local_socket::Stream>> = Mutex::new(None);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_tracking_enabled() -> bool {
    if !TRACKING_CHECKED.load(Ordering::Relaxed) {
        let disabled = std::env::var(NO_TRACKING_ENV)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        TRACKING_DISABLED.store(disabled, Ordering::Relaxed);
        TRACKING_CHECKED.store(true, Ordering::Release);
    }
    !TRACKING_DISABLED.load(Ordering::Relaxed)
}

fn next_id() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Compute the daemon socket path for the global (un-scoped) daemon.
fn socket_path() -> String {
    running_process::client::paths::socket_path(None)
}

/// Build an `interprocess` local-socket [`Name`] using the same name-type
/// dispatch as the daemon server.
fn make_socket_name(path: &str) -> std::io::Result<interprocess::local_socket::Name<'_>> {
    running_process::client::paths::make_socket_name(path)
}

/// Attempt to open a connection to the daemon.
fn try_connect() -> Option<interprocess::local_socket::Stream> {
    let path = socket_path();
    let name = make_socket_name(&path).ok()?;
    use interprocess::local_socket::traits::Stream as _;
    interprocess::local_socket::Stream::connect(name).ok()
}

/// Send a length-prefixed protobuf message to the daemon.  Fire-and-forget:
/// the response (if any) is intentionally not read.
fn send_to_daemon(request: &DaemonRequest) {
    if !is_tracking_enabled() {
        return;
    }

    let mut guard = match CONNECTION.lock() {
        Ok(g) => g,
        Err(_) => return, // poisoned mutex — silently give up
    };

    // Ensure we have a connection (lazy connect / reconnect).
    if guard.is_none() {
        *guard = try_connect();
    }
    let conn = match guard.as_mut() {
        Some(c) => c,
        None => return, // daemon not running — silently skip
    };

    let payload = request.encode_to_vec();
    let len = (payload.len() as u32).to_be_bytes();

    let ok =
        conn.write_all(&len).is_ok() && conn.write_all(&payload).is_ok() && conn.flush().is_ok();

    if !ok {
        // Connection broken — drop it so the next call retries.
        *guard = None;
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Notify the daemon that a process has been registered.  Fire-and-forget.
pub fn daemon_register(pid: u32, created_at: f64, kind: &str, command: &str, cwd: Option<&str>) {
    let request = DaemonRequest {
        id: next_id(),
        r#type: RequestType::Register as i32,
        protocol_version: 1,
        client_name: "running-process-py".to_string(),
        register: Some(RegisterRequest {
            pid,
            created_at,
            kind: kind.to_string(),
            command: command.to_string(),
            cwd: cwd.unwrap_or("").to_string(),
            originator: String::new(),
            containment: "contained".to_string(),
        }),
        ..Default::default()
    };
    send_to_daemon(&request);
}

/// Notify the daemon that a process has been unregistered.  Fire-and-forget.
pub fn daemon_unregister(pid: u32) {
    let request = DaemonRequest {
        id: next_id(),
        r#type: RequestType::Unregister as i32,
        protocol_version: 1,
        client_name: "running-process-py".to_string(),
        unregister: Some(UnregisterRequest { pid }),
        ..Default::default()
    };
    send_to_daemon(&request);
}
