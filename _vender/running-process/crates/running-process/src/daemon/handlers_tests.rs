use super::*;
use crate::proto::daemon::{
    BulkTerminateSessionsRequest, DetachPipeStreamRequest, DetachPtySessionRequest,
    GetSessionBacklogRequest, ListByOriginatorRequest, PingRequest, RegisterRequest, RequestType,
    ResizePtySessionRequest, ShutdownRequest, SpawnDaemonRequest, StatusRequest,
    TerminatePipeSessionRequest, TerminatePtySessionRequest, UnregisterRequest,
    WritePipeStdinRequest,
};

/// Build a minimal `DaemonState` for testing.
fn test_state() -> (DaemonState, tempfile::TempDir) {
    let (shutdown_tx, _rx) = watch::channel(false);
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let db_path = tmp_dir.path().join("test-handlers.db");
    let registry = Arc::new(Registry::open(&db_path).unwrap());
    let pty_sessions = Arc::new(crate::daemon::pty_sessions::PtySessionRegistry::new());
    let pipe_sessions = Arc::new(crate::daemon::pipe_sessions::PipeSessionRegistry::new());
    let services = Arc::new(
        crate::daemon::services::ServiceRegistry::open(&db_path, tmp_dir.path().join("services"))
            .unwrap(),
    );
    let state = DaemonState {
        start_time: Instant::now(),
        version: "0.0.0-test".to_string(),
        socket_path: "/tmp/test.sock".to_string(),
        db_path: "/tmp/test.db".to_string(),
        scope: "global".to_string(),
        scope_hash: "0000000000000000".to_string(),
        scope_cwd: "/tmp".to_string(),
        shutdown_tx,
        active_connections: AtomicU32::new(3),
        registry,
        pty_sessions,
        pipe_sessions,
        services,
        emergency_reserve: Arc::new(
            crate::daemon::emergency_reserve::EmergencyReserve::initialize_at(
                tmp_dir.path().join("emergency-reserve.bin"),
                4096,
            ),
        ),
    };
    (state, tmp_dir)
}

fn make_request(id: u64, rtype: RequestType) -> DaemonRequest {
    DaemonRequest {
        id,
        r#type: rtype as i32,
        protocol_version: 1,
        client_name: "test".to_string(),
        ..Default::default()
    }
}

#[test]
fn ping_returns_ok_with_server_time() {
    let (state, _tmp) = test_state();
    let mut req = make_request(1, RequestType::Ping);
    req.ping = Some(PingRequest {});

    let resp = handle_ping(&req, &state);

    assert_eq!(resp.request_id, 1);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    assert!(resp.ping.is_some());
    assert!(resp.ping.unwrap().server_time_ms > 0);
}

#[test]
fn status_returns_daemon_info() {
    let (state, _tmp) = test_state();
    let mut req = make_request(2, RequestType::Status);
    req.status = Some(StatusRequest {});

    let resp = handle_status(&req, &state);

    assert_eq!(resp.request_id, 2);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    let status = resp.status.unwrap();
    assert_eq!(status.version, "0.0.0-test");
    assert_eq!(status.active_connections, 3);
    assert_eq!(status.socket_path, "/tmp/test.sock");
    assert_eq!(status.db_path, "/tmp/test.db");
    assert_eq!(status.scope, "global");
    assert_eq!(status.scope_hash, "0000000000000000");
    assert_eq!(status.scope_cwd, "/tmp");
}

#[test]
fn shutdown_signals_channel() {
    let (state, _tmp) = test_state();
    // Keep a receiver to check the shutdown signal.
    let rx = state.shutdown_tx.subscribe();
    let mut req = make_request(3, RequestType::Shutdown);
    req.shutdown = Some(ShutdownRequest {
        graceful: true,
        timeout_seconds: 5.0,
    });

    let resp = handle_shutdown(&req, &state);

    assert_eq!(resp.request_id, 3);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    assert_eq!(resp.message, "shutting down");
    assert!(resp.shutdown.is_some());
    // The channel should now hold `true`.
    assert!(rx.has_changed().unwrap_or(false) || *rx.borrow());
}

// -----------------------------------------------------------------------
// Register / Unregister / List handler tests
// -----------------------------------------------------------------------

#[test]
fn test_register_and_list_active() {
    let (state, _tmp) = test_state();
    let mut req = make_request(10, RequestType::Register);
    req.register = Some(RegisterRequest {
        pid: 12345,
        created_at: 1000.5,
        kind: "subprocess".into(),
        command: "sleep 100".into(),
        cwd: "/tmp".into(),
        originator: "test:unit".into(),
        containment: "contained".into(),
    });

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    assert!(resp.register.is_some());

    // Now list active and verify the registered process appears.
    let list_req = make_request(11, RequestType::ListActive);
    let list_resp = handle_list_active(&list_req, &state);
    assert_eq!(list_resp.code, StatusCode::Ok as i32);

    let active = list_resp.list_active.unwrap();
    assert_eq!(active.processes.len(), 1);

    let proc = &active.processes[0];
    assert_eq!(proc.pid, 12345);
    assert_eq!(proc.command, "sleep 100");
    assert_eq!(proc.kind, "subprocess");
    assert_eq!(proc.originator, "test:unit");
    assert_eq!(proc.containment, "contained");
    assert_eq!(proc.state, ProcessState::Alive as i32);
}

#[test]
fn test_register_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(20, RequestType::Register);
    // No register payload set.

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing register payload"));
}

#[test]
fn test_register_invalid_pid() {
    let (state, _tmp) = test_state();
    let mut req = make_request(21, RequestType::Register);
    req.register = Some(RegisterRequest {
        pid: 0,
        created_at: 1000.0,
        kind: "subprocess".into(),
        command: "ls".into(),
        ..Default::default()
    });

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("pid must be > 0"));
}

#[test]
fn test_register_empty_command() {
    let (state, _tmp) = test_state();
    let mut req = make_request(22, RequestType::Register);
    req.register = Some(RegisterRequest {
        pid: 1,
        created_at: 1000.0,
        kind: "subprocess".into(),
        command: String::new(),
        ..Default::default()
    });

    let resp = handle_register(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("command must not be empty"));
}

#[test]
fn test_unregister_not_found() {
    let (state, _tmp) = test_state();
    let mut req = make_request(30, RequestType::Unregister);
    req.unregister = Some(UnregisterRequest { pid: 99999 });

    let resp = handle_unregister(&req, &state);
    assert_eq!(resp.code, StatusCode::NotFound as i32);
    assert!(resp.message.contains("99999"));
}

#[test]
fn test_unregister_success() {
    let (state, _tmp) = test_state();

    // Register first.
    let mut reg_req = make_request(31, RequestType::Register);
    reg_req.register = Some(RegisterRequest {
        pid: 5555,
        created_at: 2000.0,
        kind: "subprocess".into(),
        command: "echo hi".into(),
        cwd: "/tmp".into(),
        originator: "test:unit".into(),
        containment: "contained".into(),
    });
    let reg_resp = handle_register(&reg_req, &state);
    assert_eq!(reg_resp.code, StatusCode::Ok as i32);

    // Now unregister.
    let mut unreg_req = make_request(32, RequestType::Unregister);
    unreg_req.unregister = Some(UnregisterRequest { pid: 5555 });

    let unreg_resp = handle_unregister(&unreg_req, &state);
    assert_eq!(unreg_resp.code, StatusCode::Ok as i32);
    assert!(unreg_resp.unregister.is_some());

    // Verify list is now empty.
    let list_req = make_request(33, RequestType::ListActive);
    let list_resp = handle_list_active(&list_req, &state);
    assert_eq!(list_resp.list_active.unwrap().processes.len(), 0);
}

#[test]
fn test_spawn_daemon_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(34, RequestType::SpawnDaemon);

    let resp = handle_spawn_daemon(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing spawn_daemon payload"));
}

#[test]
fn test_spawn_daemon_empty_command() {
    let (state, _tmp) = test_state();
    let mut req = make_request(35, RequestType::SpawnDaemon);
    req.spawn_daemon = Some(SpawnDaemonRequest {
        command: "   ".into(),
        ..Default::default()
    });

    let resp = handle_spawn_daemon(&req, &state);
    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("command must not be empty"));
}

#[test]
fn request_type_enum_values_match_convention() {
    // Verify the enum values we use in dispatch match expected values.
    assert_eq!(RequestType::Register as i32, 10);
    assert_eq!(RequestType::Unregister as i32, 11);
    assert_eq!(RequestType::SpawnDaemon as i32, 12);
    assert_eq!(RequestType::ListActive as i32, 20);
    assert_eq!(RequestType::ListByOriginator as i32, 21);
    assert_eq!(RequestType::GetProcessTree as i32, 22);
    assert_eq!(RequestType::KillZombies as i32, 30);
    assert_eq!(RequestType::KillTree as i32, 31);
    assert_eq!(RequestType::Ping as i32, 40);
    assert_eq!(RequestType::Shutdown as i32, 41);
    assert_eq!(RequestType::Status as i32, 42);
}

#[test]
fn test_list_by_originator_filters() {
    let (state, _tmp) = test_state();

    // Register two processes with different originators.
    let mut req1 = make_request(40, RequestType::Register);
    req1.register = Some(RegisterRequest {
        pid: 1001,
        created_at: 1000.0,
        kind: "subprocess".into(),
        command: "cmd1".into(),
        cwd: "/tmp".into(),
        originator: "codeup:session-a".into(),
        containment: "contained".into(),
    });
    assert_eq!(handle_register(&req1, &state).code, StatusCode::Ok as i32);

    let mut req2 = make_request(41, RequestType::Register);
    req2.register = Some(RegisterRequest {
        pid: 1002,
        created_at: 2000.0,
        kind: "pty".into(),
        command: "cmd2".into(),
        cwd: "/home".into(),
        originator: "other:session-b".into(),
        containment: "detached".into(),
    });
    assert_eq!(handle_register(&req2, &state).code, StatusCode::Ok as i32);

    // Filter by "codeup" — should return 1 process.
    let mut list_req = make_request(42, RequestType::ListByOriginator);
    list_req.list_by_originator = Some(ListByOriginatorRequest {
        tool: "codeup".into(),
    });
    let resp = handle_list_by_originator(&list_req, &state);
    assert_eq!(resp.code, StatusCode::Ok as i32);
    let procs = resp.list_by_originator.unwrap().processes;
    assert_eq!(procs.len(), 1);
    assert_eq!(procs[0].pid, 1001);

    // Filter by "other" — should return 1 process.
    let mut list_req2 = make_request(43, RequestType::ListByOriginator);
    list_req2.list_by_originator = Some(ListByOriginatorRequest {
        tool: "other".into(),
    });
    let resp2 = handle_list_by_originator(&list_req2, &state);
    let procs2 = resp2.list_by_originator.unwrap().processes;
    assert_eq!(procs2.len(), 1);
    assert_eq!(procs2[0].pid, 1002);

    // Filter by "nonexistent" — should return 0.
    let mut list_req3 = make_request(44, RequestType::ListByOriginator);
    list_req3.list_by_originator = Some(ListByOriginatorRequest {
        tool: "nonexistent".into(),
    });
    let resp3 = handle_list_by_originator(&list_req3, &state);
    assert_eq!(resp3.list_by_originator.unwrap().processes.len(), 0);
}

#[test]
fn test_list_by_originator_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(45, RequestType::ListByOriginator);

    let resp = handle_list_by_originator(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing list_by_originator payload"));
}

#[test]
fn test_get_process_tree_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(46, RequestType::GetProcessTree);

    let resp = handle_get_process_tree(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing get_process_tree payload"));
}

#[test]
fn test_kill_tree_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(47, RequestType::KillTree);

    let resp = handle_kill_tree(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing kill_tree payload"));
}

#[test]
fn test_kill_zombies_missing_payload() {
    let (state, _tmp) = test_state();
    let req = make_request(48, RequestType::KillZombies);

    let resp = handle_kill_zombies(&req, &state);

    assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
    assert!(resp.message.contains("missing kill_zombies payload"));
}

#[test]
fn test_pty_session_handlers_missing_payloads() {
    let (state, _tmp) = test_state();

    let spawn = handle_spawn_pty_session(&make_request(49, RequestType::SpawnPtySession), &state);
    assert_eq!(spawn.code, StatusCode::InvalidArgument as i32);
    assert!(spawn.message.contains("spawn_pty_session payload missing"));

    let detach =
        handle_detach_pty_session(&make_request(50, RequestType::DetachPtySession), &state);
    assert_eq!(detach.code, StatusCode::InvalidArgument as i32);
    assert!(detach
        .message
        .contains("detach_pty_session payload missing"));

    let terminate =
        handle_terminate_pty_session(&make_request(51, RequestType::TerminatePtySession), &state);
    assert_eq!(terminate.code, StatusCode::InvalidArgument as i32);
    assert!(terminate
        .message
        .contains("terminate_pty_session payload missing"));

    let resize =
        handle_resize_pty_session(&make_request(52, RequestType::ResizePtySession), &state);
    assert_eq!(resize.code, StatusCode::InvalidArgument as i32);
    assert!(resize
        .message
        .contains("resize_pty_session payload missing"));
}

#[test]
fn test_pty_session_handlers_report_not_found() {
    let (state, _tmp) = test_state();

    let mut detach_req = make_request(53, RequestType::DetachPtySession);
    detach_req.detach_pty_session = Some(DetachPtySessionRequest {
        session_id: "missing-pty".into(),
    });
    let detach = handle_detach_pty_session(&detach_req, &state);
    assert_eq!(detach.code, StatusCode::NotFound as i32);
    assert!(detach.message.contains("missing-pty"));

    let mut terminate_req = make_request(54, RequestType::TerminatePtySession);
    terminate_req.terminate_pty_session = Some(TerminatePtySessionRequest {
        session_id: "missing-pty".into(),
        grace_ms: 0,
    });
    let terminate = handle_terminate_pty_session(&terminate_req, &state);
    assert_eq!(terminate.code, StatusCode::NotFound as i32);
    assert!(terminate.message.contains("missing-pty"));

    let mut resize_req = make_request(55, RequestType::ResizePtySession);
    resize_req.resize_pty_session = Some(ResizePtySessionRequest {
        session_id: "missing-pty".into(),
        rows: 40,
        cols: 100,
    });
    let resize = handle_resize_pty_session(&resize_req, &state);
    assert_eq!(resize.code, StatusCode::NotFound as i32);
    assert!(resize.message.contains("missing-pty"));
}

#[test]
fn test_pty_attach_stub_returns_internal_error() {
    let (state, _tmp) = test_state();
    let resp = handle_attach_pty_session(&make_request(56, RequestType::AttachPtySession), &state);

    assert_eq!(resp.code, StatusCode::Internal as i32);
    assert!(resp
        .message
        .contains("attach_pty_session must be intercepted"));
    assert!(resp.attach_pty_session.is_some());
}

#[test]
fn test_pipe_session_handlers_missing_payloads() {
    let (state, _tmp) = test_state();

    let spawn = handle_spawn_pipe_session(&make_request(57, RequestType::SpawnPipeSession), &state);
    assert_eq!(spawn.code, StatusCode::InvalidArgument as i32);
    assert!(spawn.message.contains("spawn_pipe_session payload missing"));

    let detach =
        handle_detach_pipe_stream(&make_request(58, RequestType::DetachPipeStream), &state);
    assert_eq!(detach.code, StatusCode::InvalidArgument as i32);
    assert!(detach
        .message
        .contains("detach_pipe_stream payload missing"));

    let terminate =
        handle_terminate_pipe_session(&make_request(59, RequestType::TerminatePipeSession), &state);
    assert_eq!(terminate.code, StatusCode::InvalidArgument as i32);
    assert!(terminate
        .message
        .contains("terminate_pipe_session payload missing"));

    let write = handle_write_pipe_stdin(&make_request(60, RequestType::WritePipeStdin), &state);
    assert_eq!(write.code, StatusCode::InvalidArgument as i32);
    assert!(write.message.contains("write_pipe_stdin payload missing"));
}

#[test]
fn test_pipe_session_handlers_report_not_found() {
    let (state, _tmp) = test_state();

    let mut detach_req = make_request(61, RequestType::DetachPipeStream);
    detach_req.detach_pipe_stream = Some(DetachPipeStreamRequest {
        session_id: "missing-pipe".into(),
        stream: 1,
    });
    let detach = handle_detach_pipe_stream(&detach_req, &state);
    assert_eq!(detach.code, StatusCode::NotFound as i32);
    assert!(detach.message.contains("missing-pipe"));

    let mut terminate_req = make_request(62, RequestType::TerminatePipeSession);
    terminate_req.terminate_pipe_session = Some(TerminatePipeSessionRequest {
        session_id: "missing-pipe".into(),
        grace_ms: 0,
    });
    let terminate = handle_terminate_pipe_session(&terminate_req, &state);
    assert_eq!(terminate.code, StatusCode::NotFound as i32);
    assert!(terminate.message.contains("missing-pipe"));

    let mut write_req = make_request(63, RequestType::WritePipeStdin);
    write_req.write_pipe_stdin = Some(WritePipeStdinRequest {
        session_id: "missing-pipe".into(),
        data: b"hello".to_vec(),
        close: false,
    });
    let write = handle_write_pipe_stdin(&write_req, &state);
    assert_eq!(write.code, StatusCode::NotFound as i32);
    assert!(write.message.contains("missing-pipe"));
}

#[test]
fn test_pipe_attach_stub_returns_internal_error() {
    let (state, _tmp) = test_state();
    let resp = handle_attach_pipe_stream(&make_request(64, RequestType::AttachPipeStream), &state);

    assert_eq!(resp.code, StatusCode::Internal as i32);
    assert!(resp
        .message
        .contains("attach_pipe_stream must be intercepted"));
    assert!(resp.attach_pipe_stream.is_some());
}

#[test]
fn test_session_maintenance_missing_payloads_and_not_found() {
    let (state, _tmp) = test_state();

    let bulk = handle_bulk_terminate_sessions(
        &make_request(65, RequestType::BulkTerminateSessions),
        &state,
    );
    assert_eq!(bulk.code, StatusCode::InvalidArgument as i32);
    assert!(bulk
        .message
        .contains("bulk_terminate_sessions payload missing"));

    let backlog_missing =
        handle_get_session_backlog(&make_request(66, RequestType::GetSessionBacklog), &state);
    assert_eq!(backlog_missing.code, StatusCode::InvalidArgument as i32);
    assert!(backlog_missing
        .message
        .contains("get_session_backlog payload missing"));

    let mut backlog_req = make_request(67, RequestType::GetSessionBacklog);
    backlog_req.get_session_backlog = Some(GetSessionBacklogRequest {
        session_id: "missing-session".into(),
        pipe_stream: 1,
    });
    let backlog = handle_get_session_backlog(&backlog_req, &state);
    assert_eq!(backlog.code, StatusCode::NotFound as i32);
    assert!(backlog.message.contains("missing-session"));

    let mut bulk_req = make_request(68, RequestType::BulkTerminateSessions);
    bulk_req.bulk_terminate_sessions = Some(BulkTerminateSessionsRequest {
        older_than_secs: 0,
        originator: String::new(),
        grace_ms: 0,
    });
    let bulk_ok = handle_bulk_terminate_sessions(&bulk_req, &state);
    assert_eq!(bulk_ok.code, StatusCode::Ok as i32);
    let payload = bulk_ok.bulk_terminate_sessions.unwrap();
    assert_eq!(payload.pty_terminated, 0);
    assert_eq!(payload.pipe_terminated, 0);
}
