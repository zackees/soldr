#![cfg(feature = "daemon")]
//! Resize PTY session while detached (#130 M5 follow-up).

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pty_session::PtySpawnRequest;
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
        "resize-rpc-test".to_string(),
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
async fn resize_pty_session_without_attach_updates_rows_cols() {
    let scope = format!("resize-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Spawn with default 24x80.
        let session = client
            .spawn_pty_session(
                &PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
                    .with_originator("resize-rpc")
                    .with_size(24, 80),
            )
            .expect("spawn");

        let listed = client.list_pty_sessions("").expect("list");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert_eq!(entry.rows, 24);
        assert_eq!(entry.cols, 80);

        // Resize while no client is attached.
        client
            .resize_pty_session(&session.session_id, 50, 120)
            .expect("resize");

        let listed = client.list_pty_sessions("").expect("list after resize");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session");
        assert_eq!(entry.rows, 50, "rows should reflect the RPC resize");
        assert_eq!(entry.cols, 120, "cols should reflect the RPC resize");

        // Unknown session id returns NotFound.
        let err = client
            .resize_pty_session("does-not-exist", 10, 10)
            .expect_err("unknown id");
        match err {
            running_process::daemon::client::ClientError::Server { code, .. } => {
                assert_eq!(code, running_process::proto::daemon::StatusCode::NotFound);
            }
            other => panic!("unexpected error: {other}"),
        }

        client
            .terminate_pty_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
