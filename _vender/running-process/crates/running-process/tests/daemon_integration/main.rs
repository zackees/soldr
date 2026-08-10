#![cfg(feature = "daemon")]
//! Integration tests for the running-process daemon.
//!
//! Each test starts a `DaemonServer` in a background tokio task using a
//! unique socket path derived from `line!()`, exercises the server via
//! `DaemonClient`, and shuts down cleanly afterwards.
//!
//! All tests use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`
//! because the `DaemonClient` performs blocking synchronous I/O.  A single-
//! threaded runtime would deadlock (the blocking client call would prevent
//! the server task from making progress on the same thread).

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::{
    DaemonRequest, RegisterRequest, RequestType, StatusCode, UnregisterRequest,
};

/// Build a unique scope string for each test to avoid socket conflicts.
macro_rules! test_scope {
    () => {
        format!("integ-{}", line!())
    };
}

/// Helper: start a `DaemonServer` in a background task, returning the
/// join handle and the socket path it is listening on.
pub fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();

    let server = DaemonServer::new(
        socket.clone(),
        db,
        "test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("failed to create DaemonServer");

    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run() failed");
    });

    (handle, socket)
}

// ---------------------------------------------------------------------------
// Test 1: happy-path roundtrip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_ping_status_shutdown_roundtrip() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    // Give the server a moment to bind the socket.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Run blocking client calls on a dedicated thread.
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // --- Ping ---
        let ping_resp = client.ping().expect("ping failed");
        assert_eq!(
            ping_resp.code,
            StatusCode::Ok as i32,
            "ping code should be OK"
        );
        let ping_payload = ping_resp
            .ping
            .expect("ping response should have ping payload");
        assert!(
            ping_payload.server_time_ms > 0,
            "server_time_ms should be positive"
        );

        // --- Status ---
        let status_resp = client.status().expect("status failed");
        assert_eq!(
            status_resp.code,
            StatusCode::Ok as i32,
            "status code should be OK"
        );
        let status_payload = status_resp
            .status
            .expect("status response should have status payload");
        assert!(
            !status_payload.version.is_empty(),
            "version should be non-empty"
        );
        assert!(
            status_payload.uptime_seconds < 60,
            "uptime should be small in a fresh test server"
        );

        // --- Shutdown ---
        let shutdown_resp = client.shutdown(true, 5.0).expect("shutdown failed");
        assert_eq!(
            shutdown_resp.code,
            StatusCode::Ok as i32,
            "shutdown code should be OK"
        );
    })
    .await;
    result.expect("client task panicked");

    // The server task should exit cleanly after shutdown.
    tokio::time::timeout(std::time::Duration::from_secs(5), server_handle)
        .await
        .expect("server did not stop within 5 seconds")
        .expect("server task panicked");
}

// ---------------------------------------------------------------------------
// Test 2: unknown request type
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_unknown_request_type_returns_unknown_request() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Send a request with a bogus type value (999).
        let bad_request = DaemonRequest {
            id: 1,
            r#type: 999,
            protocol_version: 1,
            client_name: "test-client".to_string(),
            ..Default::default()
        };
        let resp = client
            .send_request(bad_request)
            .expect("send_request failed");

        assert_eq!(
            resp.code,
            StatusCode::UnknownRequest as i32,
            "code should be UNKNOWN_REQUEST for bogus type"
        );
        assert!(
            resp.message.contains("unknown request type"),
            "message should mention unknown type, got: {}",
            resp.message
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 3: multiple pings
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_pings() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        for i in 0..10 {
            let resp = client
                .ping()
                .unwrap_or_else(|e| panic!("ping {i} failed: {e}"));
            assert_eq!(
                resp.code,
                StatusCode::Ok as i32,
                "ping {i} should return OK"
            );
            assert!(resp.ping.is_some(), "ping {i} should have payload");
        }

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 4: status shows active connections
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_shows_active_connections() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let socket_clone = socket.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Connect TWO clients.  On Windows named pipes, the server must loop
        // back to accept() before a second client can connect, so we add a
        // brief pause between connections.
        let mut client1 =
            DaemonClient::connect_to(&socket_clone).expect("failed to connect client 1");

        // Send a ping on client 1 so the server processes its accept and
        // loops back to listen for the next connection.
        let _ = client1.ping().expect("initial ping on client 1 failed");
        std::thread::sleep(std::time::Duration::from_millis(200));

        let _client2 = DaemonClient::connect_to(&socket_clone).expect("failed to connect client 2");

        // Give the server a moment to accept the second connection.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Send a status request from client 1.
        let resp = client1.status().expect("status failed");
        assert_eq!(resp.code, StatusCode::Ok as i32);

        let status = resp.status.expect("status payload missing");
        // Both connections should be active. The server increments
        // active_connections on accept, before any frames are read.
        assert!(
            status.active_connections >= 2,
            "expected at least 2 active connections, got {}",
            status.active_connections
        );

        // Clean up.
        let _ = client1.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ===========================================================================
// Phase 2: Registry operation integration tests
// ===========================================================================

/// Multiplier for sleep / poll durations in these integration tests. On
/// GitHub-hosted runners (especially `windows-11-arm`) the daemon's
/// process tracking and the spawned shell helpers run noticeably
/// slower than on dev laptops, and asserts that pass locally in 600 ms
/// regularly miss their deadline in CI. Scale generously on GitHub
/// Actions; keep tests fast locally. `RUNNING_PROCESS_TEST_LATENCY_X`
/// overrides explicitly if a developer needs to reproduce the slow path.
pub fn latency_multiplier() -> u32 {
    if let Ok(value) = std::env::var("RUNNING_PROCESS_TEST_LATENCY_X") {
        if let Ok(n) = value.parse::<u32>() {
            return n.max(1);
        }
    }
    if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
        return 5;
    }
    1
}

/// Scale a `Duration` by [`latency_multiplier`].
pub fn scaled(d: std::time::Duration) -> std::time::Duration {
    d.saturating_mul(latency_multiplier())
}

/// Best-effort liveness check on an arbitrary PID. Used by test debug
/// instrumentation to distinguish "process exited" from "process alive
/// but not visible to the daemon's tracker".
#[cfg(windows)]
pub fn is_pid_alive(pid: u32) -> bool {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetExitCodeProcess, OpenProcess};
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Helper: start a `DaemonServer` backed by a temp directory for the SQLite DB.
///
/// Returns the join handle, socket path, and the `TempDir` (which must be kept
/// alive for the duration of the test so the directory is not deleted).
pub fn start_server_with_tempdb(
    scope: &str,
) -> (tokio::task::JoinHandle<()>, String, tempfile::TempDir) {
    let socket = paths::socket_path(Some(scope));
    let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let db = tmp_dir
        .path()
        .join("test-registry.db")
        .to_string_lossy()
        .into_owned();

    let server = DaemonServer::new(
        socket.clone(),
        db,
        "test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("failed to create DaemonServer");

    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run() failed");
    });

    (handle, socket, tmp_dir)
}

/// Build a `DaemonRequest` with a `RegisterRequest` payload.
pub fn make_register_request(
    pid: u32,
    created_at: f64,
    kind: &str,
    command: &str,
    cwd: &str,
    originator: &str,
    containment: &str,
) -> DaemonRequest {
    DaemonRequest {
        id: 0,
        r#type: RequestType::Register.into(),
        protocol_version: 1,
        client_name: "test-client".to_string(),
        register: Some(RegisterRequest {
            pid,
            created_at,
            kind: kind.to_string(),
            command: command.to_string(),
            cwd: cwd.to_string(),
            originator: originator.to_string(),
            containment: containment.to_string(),
        }),
        ..Default::default()
    }
}

/// Build a `DaemonRequest` with an `UnregisterRequest` payload.
pub fn make_unregister_request(pid: u32) -> DaemonRequest {
    DaemonRequest {
        id: 0,
        r#type: RequestType::Unregister.into(),
        protocol_version: 1,
        client_name: "test-client".to_string(),
        unregister: Some(UnregisterRequest { pid }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Test 5: register -> list -> unregister -> list roundtrip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_list_unregister_roundtrip() {
    let scope = format!("integ2-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // --- Register a process ---
        let reg_req = make_register_request(
            99999,
            1000.0,
            "subprocess",
            "test cmd",
            "/tmp",
            "TEST:1",
            "contained",
        );
        let reg_resp = client.send_request(reg_req).expect("register failed");
        assert_eq!(
            reg_resp.code,
            StatusCode::Ok as i32,
            "register should return OK"
        );

        // --- ListActive: should have 1 process ---
        let list_resp = client.list_active().expect("list_active failed");
        assert_eq!(list_resp.code, StatusCode::Ok as i32);
        let active = list_resp.list_active.expect("list_active payload missing");
        assert_eq!(
            active.processes.len(),
            1,
            "expected 1 tracked process after register"
        );

        let proc = &active.processes[0];
        assert_eq!(proc.pid, 99999);
        assert_eq!(proc.kind, "subprocess");
        assert_eq!(proc.command, "test cmd");
        assert_eq!(proc.cwd, "/tmp");
        assert_eq!(proc.originator, "TEST:1");
        assert_eq!(proc.containment, "contained");

        // --- Unregister ---
        let unreg_req = make_unregister_request(99999);
        let unreg_resp = client.send_request(unreg_req).expect("unregister failed");
        assert_eq!(
            unreg_resp.code,
            StatusCode::Ok as i32,
            "unregister should return OK"
        );

        // --- ListActive: should now be empty ---
        let list_resp2 = client
            .list_active()
            .expect("list_active after unregister failed");
        assert_eq!(list_resp2.code, StatusCode::Ok as i32);
        let active2 = list_resp2
            .list_active
            .expect("list_active payload missing after unregister");
        assert_eq!(
            active2.processes.len(),
            0,
            "expected 0 tracked processes after unregister"
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 6: register with pid=0 returns INVALID_ARGUMENT
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_register_invalid_pid_returns_error() {
    let scope = format!("integ2-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Register with pid=0 -- should be rejected.
        let reg_req = make_register_request(
            0, // invalid
            1000.0,
            "subprocess",
            "bad cmd",
            "/tmp",
            "TEST:1",
            "contained",
        );
        let resp = client.send_request(reg_req).expect("send_request failed");
        assert_eq!(
            resp.code,
            StatusCode::InvalidArgument as i32,
            "register with pid=0 should return INVALID_ARGUMENT, got code={}",
            resp.code
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 7: unregister nonexistent pid returns NOT_FOUND
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_unregister_nonexistent_returns_not_found() {
    let scope = format!("integ2-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Unregister a pid that was never registered.
        let unreg_req = make_unregister_request(88888);
        let resp = client.send_request(unreg_req).expect("send_request failed");
        assert_eq!(
            resp.code,
            StatusCode::NotFound as i32,
            "unregister of nonexistent pid should return NOT_FOUND, got code={}",
            resp.code
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 8: list_by_originator filters correctly
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_list_by_originator_filters_correctly() {
    let scope = format!("integ2-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Register pid=10001 with originator "TOOL_A:1".
        let reg1 = make_register_request(
            10001,
            1000.0,
            "subprocess",
            "cmd_a",
            "/tmp",
            "TOOL_A:1",
            "contained",
        );
        let resp1 = client.send_request(reg1).expect("register 10001 failed");
        assert_eq!(resp1.code, StatusCode::Ok as i32);

        // Register pid=10002 with originator "TOOL_B:2".
        let reg2 = make_register_request(
            10002,
            2000.0,
            "subprocess",
            "cmd_b",
            "/tmp",
            "TOOL_B:2",
            "detached",
        );
        let resp2 = client.send_request(reg2).expect("register 10002 failed");
        assert_eq!(resp2.code, StatusCode::Ok as i32);

        // ListByOriginator for "TOOL_A" -> should return 1 result with pid=10001.
        let lbo_a = client
            .list_by_originator("TOOL_A")
            .expect("list_by_originator TOOL_A failed");
        assert_eq!(lbo_a.code, StatusCode::Ok as i32);
        let procs_a = lbo_a
            .list_by_originator
            .expect("list_by_originator payload missing for TOOL_A")
            .processes;
        assert_eq!(procs_a.len(), 1, "expected 1 process for TOOL_A");
        assert_eq!(procs_a[0].pid, 10001);

        // ListByOriginator for "TOOL_B" -> should return 1 result with pid=10002.
        let lbo_b = client
            .list_by_originator("TOOL_B")
            .expect("list_by_originator TOOL_B failed");
        assert_eq!(lbo_b.code, StatusCode::Ok as i32);
        let procs_b = lbo_b
            .list_by_originator
            .expect("list_by_originator payload missing for TOOL_B")
            .processes;
        assert_eq!(procs_b.len(), 1, "expected 1 process for TOOL_B");
        assert_eq!(procs_b[0].pid, 10002);

        // ListByOriginator for "NONEXISTENT" -> should return 0 results.
        let lbo_none = client
            .list_by_originator("NONEXISTENT")
            .expect("list_by_originator NONEXISTENT failed");
        assert_eq!(lbo_none.code, StatusCode::Ok as i32);
        let procs_none = lbo_none
            .list_by_originator
            .expect("list_by_originator payload missing for NONEXISTENT")
            .processes;
        assert_eq!(procs_none.len(), 0, "expected 0 processes for NONEXISTENT");

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

mod compiler_wrap_seam_test;
mod env_replace_test;
mod more_tests;
mod stdout_seam_test;
