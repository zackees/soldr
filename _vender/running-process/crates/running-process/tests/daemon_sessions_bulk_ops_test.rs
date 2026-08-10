#![cfg(feature = "daemon")]
//! Bulk session ops (#130 M9 H4 follow-up).
//!
//! Verifies `purge_exited_sessions` removes exited sessions but leaves
//! live ones, and `bulk_terminate_sessions` schedules termination for
//! sessions older than a threshold while leaving newer ones running.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pipe_session::PipeSpawnRequest;
use running_process::daemon::server::DaemonServer;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn testbin_path(name: &str) -> PathBuf {
    // Fixtures are built once, before the suite runs (see `ci/test.py`).
    //
    // This used to invoke `cargo build -p testbins` on every call. That takes
    // cargo's build-directory lock, and nextest runs each test in its own
    // process, so a full-suite run had dozens of cargo invocations contending
    // for one lock. `Command::output` waits for EOF with no deadline, and
    // cargo's "Blocking waiting for file lock" note went to inherited stderr
    // the harness only shows on failure — so it presented as an unexplained
    // 30s+ hang. See running-process#747 for the symbolized stack.
    let exe = std::env::current_exe().expect("current exe");
    // .../target/<triple>/<profile>/deps/<test-binary>
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();
    let server = DaemonServer::new(
        socket.clone(),
        db,
        "bulk-ops-test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("DaemonServer::new");
    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run");
    });
    (handle, socket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_removes_only_exited_sessions() {
    let scope = format!("bulk-purge-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn two pipe sessions.
        let alive = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("bulk-purge"),
            )
            .expect("spawn alive");
        let to_terminate = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("bulk-purge"),
            )
            .expect("spawn to_terminate");

        // Terminate one of them and wait for it to actually exit.
        client
            .terminate_pipe_session(&to_terminate.session_id, 500)
            .expect("terminate");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let listed = client.list_pipe_sessions("bulk-purge").expect("list");
            if let Some(e) = listed
                .iter()
                .find(|s| s.session_id == to_terminate.session_id)
            {
                if e.exited {
                    break;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        // Purge should remove the exited session but leave the alive one.
        let purged = client.purge_exited_sessions("bulk-purge").expect("purge");
        assert_eq!(purged.pty_purged, 0);
        assert_eq!(purged.pipe_purged, 1);

        // List confirms only the alive session remains.
        let remaining = client.list_pipe_sessions("bulk-purge").expect("list after");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].session_id, alive.session_id);

        // Cleanup.
        client
            .terminate_pipe_session(&alive.session_id, 500)
            .expect("terminate cleanup");
    })
    .await
    .expect("blocking task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_terminate_older_than_zero_terminates_everything_in_scope() {
    let scope = format!("bulk-kill-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn 3 sessions in this scope's originator.
        let mut ids = Vec::new();
        for _ in 0..3 {
            let session = client
                .spawn_pipe_session(
                    &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                        .with_originator("bulk-kill"),
                )
                .expect("spawn");
            ids.push(session.session_id);
        }
        // And one with a different originator to confirm filtering.
        let untouched = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("other"),
            )
            .expect("spawn untouched");
        ids.push(untouched.session_id.clone());

        // older_than=0 + originator="bulk-kill" terminates the 3 matching.
        let result = client
            .bulk_terminate_sessions(0, "bulk-kill", 500)
            .expect("bulk terminate");
        assert_eq!(result.pty_terminated, 0);
        assert_eq!(result.pipe_terminated, 3);

        // Wait for them to actually exit.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = client.list_pipe_sessions("bulk-kill").expect("list");
            if listed.iter().all(|s| s.exited) {
                break;
            }
            if Instant::now() >= deadline {
                panic!("bulk-killed sessions did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        // The untouched session should still be alive.
        let other = client.list_pipe_sessions("other").expect("list other");
        let untouched_entry = other
            .iter()
            .find(|s| s.session_id == untouched.session_id)
            .expect("untouched session present");
        assert!(
            !untouched_entry.exited,
            "untouched (different originator) must still be alive"
        );

        // Cleanup.
        client
            .terminate_pipe_session(&untouched.session_id, 500)
            .expect("terminate untouched");
    })
    .await
    .expect("blocking task");
}
