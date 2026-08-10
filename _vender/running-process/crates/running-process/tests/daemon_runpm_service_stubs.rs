#![cfg(feature = "daemon")]
//! Phase 2 + Phase 3 integration tests for the runpm `SERVICE_*` daemon
//! RPCs (#222, #426).
//!
//! Phase 1 only exercised the stub round-trip; Phase 2 made `start`, `stop`,
//! `restart`, `delete`, `list`, and `describe` real lifecycle operations.
//! Phase 3 (this file's current assertions) makes `logs` and `flush` real:
//! `logs` tails the on-disk `-out.log`/`-err.log` for the service, and
//! `flush` truncates those files to zero bytes. `save`/`resurrect`
//! (Phase 4) remain stubs and are still exercised for round-trip coverage.
//!
//! Uses `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` because
//! `DaemonClient` performs blocking synchronous I/O — a single-threaded
//! runtime would deadlock the server task.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::{ServiceConfig, StatusCode};

/// Build a unique scope string for each test to avoid socket conflicts.
macro_rules! test_scope {
    () => {
        format!("runpm-stubs-{}", line!())
    };
}

/// Helper: start a `DaemonServer` in a background task, returning the join
/// handle and the socket path it is listening on.  Mirrors the helper in
/// `tests/integration.rs` (kept local — that helper is private to its file).
fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
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

/// Pick a long-lived, cross-platform command so the service stays online
/// long enough to be observed by `list`/`describe` before we stop it.
fn long_lived_cmd() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            "cmd".into(),
            "/C".into(),
            "ping -n 300 127.0.0.1 > NUL".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec!["sleep".into(), "300".into()]
    }
}

// ---------------------------------------------------------------------------
// Phase 2 lifecycle: start -> list -> describe -> stop -> restart -> delete,
// then the still-stubbed logs/flush/save/resurrect RPCs.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_service_lifecycle_and_remaining_stubs() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    // Give the server a moment to bind the socket.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // --- ServiceStart (real spawn) ----------------------------------
        let cfg = ServiceConfig {
            name: "test-svc".into(),
            cmd: long_lived_cmd(),
            cwd: ".".into(),
            env: Default::default(),
            autorestart: false,
            max_restarts: 0,
            restart_delay_ms: 0,
            kill_timeout_ms: 500,
            min_uptime_ms: 0,
        };
        let resp = client.service_start(cfg).expect("service_start failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_start should be OK"
        );
        let svc = resp
            .service_start
            .and_then(|r| r.service)
            .expect("service_start should carry a populated service");
        assert_eq!(svc.name, "test-svc");
        assert_eq!(svc.status, "online");
        assert!(svc.pid > 0, "online service should have a pid");

        // --- ServiceList -----------------------------------------------
        let resp = client.service_list().expect("service_list failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_list should be OK"
        );
        let services = resp.service_list.expect("list payload").services;
        assert_eq!(services.len(), 1, "exactly one service should be listed");
        assert_eq!(services[0].name, "test-svc");

        // --- ServiceDescribe -------------------------------------------
        let resp = client
            .service_describe("test-svc")
            .expect("service_describe failed");
        assert_eq!(resp.code, StatusCode::Ok as i32, "describe should be OK");
        let described = resp
            .service_describe
            .and_then(|r| r.service)
            .expect("describe should carry a service");
        assert_eq!(described.name, "test-svc");

        // --- ServiceStop ------------------------------------------------
        let resp = client
            .service_stop("test-svc")
            .expect("service_stop failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_stop should be OK"
        );
        assert_eq!(
            resp.service_stop.expect("stop payload").stopped_count,
            1,
            "one service should be stopped"
        );

        // --- ServiceRestart --------------------------------------------
        let resp = client
            .service_restart("test-svc")
            .expect("service_restart failed");
        assert_eq!(resp.code, StatusCode::Ok as i32, "restart should be OK");
        assert_eq!(
            resp.service_restart
                .expect("restart payload")
                .restarted_count,
            1,
            "one service should be restarted"
        );

        // --- ServiceLogs (Phase 3 — real impl) -------------------------
        // The service exists (it's been restarted but not deleted); the
        // log files may or may not have content depending on whether the
        // child wrote anything. The handler must return OK with a
        // populated payload even when the body is empty.
        let resp = client
            .service_logs("test-svc", 100, false)
            .expect("service_logs failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_logs should return OK for an existing service"
        );
        assert!(
            resp.service_logs.is_some(),
            "service_logs response should carry a service_logs payload"
        );

        // --- ServiceFlush (Phase 3 — real impl) ------------------------
        let resp = client
            .service_flush("test-svc")
            .expect("service_flush failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_flush should return OK"
        );
        let flushed = resp.service_flush.expect("flush payload").flushed_count;
        assert_eq!(flushed, 1, "exactly one service should be flushed");

        // --- ServiceFlush "all" ----------------------------------------
        let resp = client
            .service_flush("all")
            .expect("service_flush all failed");
        assert_eq!(resp.code, StatusCode::Ok as i32);
        // One service is registered so "all" flushes exactly one.
        assert_eq!(
            resp.service_flush.expect("flush payload").flushed_count,
            1,
            "flush all should hit the one registered service"
        );

        // --- ServiceDelete ---------------------------------------------
        let resp = client
            .service_delete("test-svc")
            .expect("service_delete failed");
        assert_eq!(resp.code, StatusCode::Ok as i32, "delete should be OK");
        assert_eq!(
            resp.service_delete.expect("delete payload").deleted_count,
            1,
            "one service should be deleted"
        );

        // Deleted service should no longer be listed.
        let resp = client.service_list().expect("service_list failed");
        assert!(
            resp.service_list.expect("list payload").services.is_empty(),
            "service list should be empty after delete"
        );

        // --- ServiceLogs for missing service -> NOT_FOUND --------------
        let resp = client
            .service_logs("test-svc", 100, false)
            .expect("service_logs failed");
        assert_eq!(
            resp.code,
            StatusCode::NotFound as i32,
            "logs for a deleted service should return NOT_FOUND"
        );

        // --- ServiceSave -----------------------------------------------
        let resp = client.service_save().expect("service_save failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_save should return OK"
        );
        assert!(
            resp.service_save.is_some(),
            "service_save response should carry a service_save payload"
        );

        // --- ServiceResurrect ------------------------------------------
        let resp = client
            .service_resurrect()
            .expect("service_resurrect failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_resurrect should return OK"
        );
        assert!(
            resp.service_resurrect.is_some(),
            "service_resurrect response should carry a service_resurrect payload"
        );

        // --- Shutdown ---------------------------------------------------
        let shutdown_resp = client.shutdown(true, 5.0).expect("shutdown failed");
        assert_eq!(
            shutdown_resp.code,
            StatusCode::Ok as i32,
            "shutdown should return OK"
        );
    })
    .await;
    result.expect("client task panicked");

    // The server should exit cleanly after the shutdown RPC.
    tokio::time::timeout(std::time::Duration::from_secs(5), server_handle)
        .await
        .expect("server did not stop within 5 seconds")
        .expect("server task panicked");
}
