#![cfg(feature = "daemon")]
//! Integration test for `GetSessionBacklog` / `sessions log`
//! (#130 milestone 7 B4).
//!
//! Asserts the daemon can snapshot a session's captured output without
//! consuming it: two back-to-back snapshots see the same bytes, and a
//! subsequent attach (which DOES drain) still receives the same backlog.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pipe_session::{PipeSpawnRequest, PipeStreamAttachment};
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::PipeStreamKind;

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
        "sessions-log-test".to_string(),
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
async fn snapshot_does_not_consume_backlog_for_pipe_sessions() {
    let scope = format!("snapshot-pipe-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let session = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
                    .with_originator("snapshot-test"),
            )
            .expect("spawn");

        // Give env-reporter time to print PID=, ORIGINATOR=, READY.
        std::thread::sleep(Duration::from_millis(500));

        // First snapshot: backlog should contain READY.
        let snap1 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snapshot 1")
            .expect("session present");
        assert_eq!(snap1.session_kind, "pipe");
        assert!(!snap1.exited, "child should still be running");
        let text1 = String::from_utf8_lossy(&snap1.backlog).into_owned();
        assert!(
            text1.contains("READY"),
            "first snapshot should include READY, got: {text1:?}"
        );

        // Second snapshot: same bytes still present (no draining).
        let snap2 = client
            .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
            .expect("snapshot 2")
            .expect("session present");
        let text2 = String::from_utf8_lossy(&snap2.backlog).into_owned();
        assert!(
            text2.contains("READY"),
            "second snapshot should still include READY (no consume), got: {text2:?}"
        );

        // Attach (which DOES drain) — initial_backlog should ALSO contain
        // READY, proving the snapshot did not consume it for the attach
        // path.
        let attachment = PipeStreamAttachment::attach_to(
            &socket_for_test,
            &session.session_id,
            PipeStreamKind::Stdout,
            false,
        )
        .expect("attach");
        let attach_text = String::from_utf8_lossy(&attachment.initial_backlog).into_owned();
        assert!(
            attach_text.contains("READY"),
            "attach initial_backlog should still see READY after two snapshots, got: {attach_text:?}"
        );
        drop(attachment);

        // Unknown session id → NotFound surfaces as Ok(None).
        let missing = client
            .get_session_backlog("does-not-exist", PipeStreamKind::Stdout)
            .expect("snapshot for missing id");
        assert!(missing.is_none());

        // Cleanup.
        client
            .terminate_pipe_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
