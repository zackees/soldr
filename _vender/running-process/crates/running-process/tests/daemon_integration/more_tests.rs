//! Continuation of integration tests — split for file-size budget.

use running_process::daemon::client::{DaemonClient, SpawnCommandRequest};
use running_process::proto::daemon::StatusCode;

use super::{make_register_request, scaled, start_server_with_tempdb};

// ---------------------------------------------------------------------------
// Test 9: status shows tracked_process_count
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_shows_tracked_count() {
    let scope = format!("integ2-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Status before any registrations -> tracked_process_count == 0.
        let status0 = client.status().expect("status failed");
        assert_eq!(status0.code, StatusCode::Ok as i32);
        let s0 = status0.status.expect("status payload missing");
        assert_eq!(
            s0.tracked_process_count, 0,
            "expected 0 tracked processes initially"
        );

        // Register 2 processes.
        let reg1 = make_register_request(
            20001,
            1000.0,
            "subprocess",
            "proc1",
            "/tmp",
            "TOOL:1",
            "contained",
        );
        let resp1 = client.send_request(reg1).expect("register 20001 failed");
        assert_eq!(resp1.code, StatusCode::Ok as i32);

        let reg2 =
            make_register_request(20002, 2000.0, "pty", "proc2", "/home", "TOOL:2", "detached");
        let resp2 = client.send_request(reg2).expect("register 20002 failed");
        assert_eq!(resp2.code, StatusCode::Ok as i32);

        // Status after registrations -> tracked_process_count == 2.
        let status2 = client.status().expect("status after register failed");
        assert_eq!(status2.code, StatusCode::Ok as i32);
        let s2 = status2
            .status
            .expect("status payload missing after register");
        assert_eq!(
            s2.tracked_process_count, 2,
            "expected 2 tracked processes after registering 2"
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 10: spawn_daemon runs a detached command under daemon control
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawn_daemon_tracks_spawned_process_and_context() {
    let scope = format!("integ-spawn-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("failed to create temp dir");
    let workdir_path = workdir.path().to_path_buf();
    let workdir_string = workdir_path.to_string_lossy().into_owned();
    let env_file = workdir_path.join("spawn-env.txt");
    let cwd_file = workdir_path.join("spawn-cwd.txt");

    let command = if cfg!(windows) {
        format!(
            "echo %RP_DAEMON_TEST_VAR%> \"{}\" & cd > \"{}\" & ping 127.0.0.1 -n 6 >NUL",
            env_file.display(),
            cwd_file.display()
        )
    } else {
        format!(
            "printf '%s' \"$RP_DAEMON_TEST_VAR\" > \"{}\"; pwd > \"{}\"; sleep 5",
            env_file.display(),
            cwd_file.display()
        )
    };

    let socket_for_client = socket.clone();
    let workdir_for_client = workdir_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client =
            DaemonClient::connect_to(&socket_for_client).expect("failed to connect to daemon");

        let request = SpawnCommandRequest::shell(command)
            .with_cwd(workdir_for_client.clone())
            .with_env("RP_DAEMON_TEST_VAR", "daemon-spawn-ok")
            .with_originator("TEST:spawn");

        let spawned = client
            .spawn_command(&request)
            .expect("spawn_command should succeed");
        assert!(spawned.pid > 0, "spawned pid should be > 0");
        assert_eq!(spawned.originator.as_deref(), Some("TEST:spawn"));
        assert_eq!(
            spawned.cwd.as_deref(),
            Some(workdir_for_client.to_string_lossy().as_ref())
        );
        assert_eq!(spawned.containment, "detached");

        std::thread::sleep(scaled(std::time::Duration::from_millis(600)));

        let list_resp = client.list_active().expect("list_active failed");
        let processes = list_resp
            .list_active
            .expect("list_active payload missing")
            .processes;
        let tracked = processes
            .iter()
            .find(|process| process.pid == spawned.pid)
            .expect("spawned process should be tracked");
        assert_eq!(tracked.command, request.command);
        assert_eq!(tracked.originator, "TEST:spawn");
        assert_eq!(tracked.containment, "detached");

        let kill_resp = client
            .kill_tree(spawned.pid, 3.0)
            .expect("kill_tree should succeed");
        assert_eq!(kill_resp.code, StatusCode::Ok as i32);

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let env_contents = std::fs::read_to_string(&env_file).expect("env file should exist");
    assert!(
        env_contents.contains("daemon-spawn-ok"),
        "expected daemon-spawn-ok in env file, got: {env_contents}"
    );

    let cwd_contents = std::fs::read_to_string(&cwd_file).expect("cwd file should exist");
    let observed_cwd = std::fs::canonicalize(cwd_contents.trim())
        .unwrap_or_else(|_| std::path::PathBuf::from(cwd_contents.trim()));
    let expected_cwd = std::fs::canonicalize(workdir.path())
        .unwrap_or_else(|_| std::path::PathBuf::from(&workdir_string));
    assert_eq!(observed_cwd, expected_cwd);

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 11: spawned commands survive daemon shutdown
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawned_process_survives_daemon_shutdown() {
    let scope = format!("integ-spawn-survive-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let workdir = tempfile::tempdir().expect("failed to create temp dir");
    let marker = workdir.path().join("survived.txt");

    let command = if cfg!(windows) {
        format!(
            "ping 127.0.0.1 -n 3 >NUL & echo survived> \"{}\" & ping 127.0.0.1 -n 3 >NUL",
            marker.display()
        )
    } else {
        format!(
            "sleep 1; printf 'survived' > \"{}\"; sleep 2",
            marker.display()
        )
    };

    let socket_for_client = socket.clone();
    let marker_for_assert = marker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut client =
            DaemonClient::connect_to(&socket_for_client).expect("failed to connect to daemon");

        let spawned = client
            .spawn_command(&SpawnCommandRequest::shell(command))
            .expect("spawn_command should succeed");
        assert!(spawned.pid > 0);
        assert!(
            spawned.originator.is_some(),
            "spawned process should record originator metadata"
        );

        let shutdown = client.shutdown(true, 5.0).expect("shutdown failed");
        assert_eq!(shutdown.code, StatusCode::Ok as i32);

        let _ = std::fs::metadata(&marker_for_assert);
    })
    .await;
    result.expect("client task panicked");

    tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle)
        .await
        .expect("server did not stop in time")
        .expect("server task panicked");

    let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(6));
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let contents = std::fs::read_to_string(&marker).expect("marker file should exist");
    assert_eq!(contents.trim(), "survived");
}

// ===========================================================================
// Phase 4: Reaper, KillTree, KillZombies, GetProcessTree integration tests
// ===========================================================================

// ---------------------------------------------------------------------------
// Test 10: kill_zombies dry_run with no zombies returns OK and empty list
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill_zombies_dry_run_with_no_zombies() {
    let scope = format!("integ4-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        let resp = client
            .kill_zombies(true)
            .expect("kill_zombies dry_run failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "kill_zombies should return OK"
        );
        let zombies = resp
            .kill_zombies
            .expect("kill_zombies payload missing")
            .zombies;
        // Filter out orphan conhost.exe entries — those are ambient system
        // state, not controlled by this test.
        let registry_zombies: Vec<_> = zombies
            .iter()
            .filter(|z| z.command != "conhost.exe")
            .collect();
        assert!(
            registry_zombies.is_empty(),
            "expected no registry zombies in empty registry, got {}",
            registry_zombies.len()
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 11: kill_zombies (non-dry-run) with no zombies returns OK and empty list
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill_zombies_with_no_zombies() {
    let scope = format!("integ4-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        let resp = client.kill_zombies(false).expect("kill_zombies failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "kill_zombies should return OK"
        );
        let zombies = resp
            .kill_zombies
            .expect("kill_zombies payload missing")
            .zombies;
        // Filter out orphan conhost.exe entries — ambient system state.
        let registry_zombies: Vec<_> = zombies
            .iter()
            .filter(|z| z.command != "conhost.exe")
            .collect();
        assert!(
            registry_zombies.is_empty(),
            "expected no registry zombies in empty registry, got {}",
            registry_zombies.len()
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 12: kill_tree for a nonexistent PID returns OK with 0 killed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill_tree_nonexistent_pid() {
    let scope = format!("integ4-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Use a PID that almost certainly does not exist.
        let resp = client.kill_tree(4_000_099, 3.0).expect("kill_tree failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "kill_tree should return OK"
        );
        let count = resp
            .kill_tree
            .expect("kill_tree payload missing")
            .processes_killed;
        assert_eq!(count, 0, "expected 0 processes killed for nonexistent PID");

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 13: get_process_tree for current process returns non-empty tree
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_get_process_tree_for_current_process() {
    let scope = format!("integ4-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let current_pid = std::process::id();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        let resp = client
            .get_process_tree(current_pid)
            .expect("get_process_tree failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "get_process_tree should return OK"
        );
        let tree_display = resp
            .get_process_tree
            .expect("get_process_tree payload missing")
            .tree_display;
        assert!(
            !tree_display.is_empty(),
            "tree display should not be empty for current process"
        );
        assert!(
            tree_display.contains(&format!("pid={current_pid}")),
            "tree display should contain current PID, got: {tree_display}"
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

// ---------------------------------------------------------------------------
// Test 14: kill_zombies finds a registered dead process via dry-run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_kill_zombies_finds_registered_dead_process() {
    let scope = format!("integ4-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        // Register a fake dead PID (4_000_050 is unlikely to be a real process).
        let reg_req = make_register_request(
            4_000_050,
            1000.0,
            "subprocess",
            "fake-dead-cmd",
            "/tmp",
            "TEST:zombie",
            "contained",
        );
        let reg_resp = client.send_request(reg_req).expect("register failed");
        assert_eq!(
            reg_resp.code,
            StatusCode::Ok as i32,
            "register should succeed"
        );

        // Dry-run: should detect the dead process as a zombie.
        let resp = client
            .kill_zombies(true)
            .expect("kill_zombies dry_run failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "kill_zombies should return OK"
        );
        let zombies = resp
            .kill_zombies
            .expect("kill_zombies payload missing")
            .zombies;
        // Filter out orphan conhost.exe entries — ambient system state.
        let registry_zombies: Vec<_> = zombies
            .iter()
            .filter(|z| z.command != "conhost.exe")
            .collect();
        assert_eq!(
            registry_zombies.len(),
            1,
            "expected 1 registry zombie for dead fake PID, got {}",
            registry_zombies.len()
        );
        assert_eq!(registry_zombies[0].pid, 4_000_050);
        assert_eq!(registry_zombies[0].command, "fake-dead-cmd");
        assert!(
            !registry_zombies[0].killed,
            "dry_run should not kill the process"
        );

        // The process should still be in the registry (dry-run does not remove).
        let list_resp = client.list_active().expect("list_active failed");
        let procs = list_resp
            .list_active
            .expect("list_active payload missing")
            .processes;
        assert_eq!(
            procs.len(),
            1,
            "process should still be tracked after dry-run"
        );

        // Clean up.
        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}
