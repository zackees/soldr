#![cfg(feature = "daemon")]
//! TerminationOutcome tracking on ExitState (#130 M4 follow-up).
//!
//! When `terminate_pty_session` / `terminate_pipe_session` is called and
//! the child exits within the grace window the outcome should be
//! SOFT_EXIT; if the daemon has to fall back to the hard kill it should
//! be HARD_KILLED; if the child exits on its own the outcome is
//! NATURAL_EXIT. UNSPECIFIED is the default while the session is alive.
//!
//! The current implementation uses kill_tree as the soft signal on POSIX
//! PTY sessions and immediate hard kill on Windows / pipe sessions, so
//! the classification window for SOFT_EXIT is the grace + small slack.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pipe_session::PipeSpawnRequest;
use running_process::daemon::pty_session::PtySpawnRequest;
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::TerminationOutcome;

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
        "termination-outcome-test".to_string(),
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
async fn pty_terminate_records_soft_or_hard_outcome() {
    let scope = format!("term-out-pty-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // While alive: outcome must be UNSPECIFIED.
        let session = client
            .spawn_pty_session(
                &PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
                    .with_originator("term-out-pty"),
            )
            .expect("spawn");
        let listed = client.list_pty_sessions("").expect("list");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session.session_id)
            .expect("session present");
        assert_eq!(
            entry.termination_outcome,
            TerminationOutcome::Unspecified as i32,
            "live session must report UNSPECIFIED outcome"
        );

        // Terminate and wait for exit. We accept SOFT or HARD; what we
        // require is "not UNSPECIFIED and not NATURAL" because terminate
        // RPC fired before exit.
        client
            .terminate_pty_session(&session.session_id, 1000)
            .expect("terminate");
        let deadline = Instant::now() + Duration::from_secs(15);
        let outcome = loop {
            let listed = client.list_pty_sessions("").expect("list");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session.session_id) {
                if entry.exited {
                    break entry.termination_outcome;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        assert!(
            outcome == TerminationOutcome::SoftExit as i32
                || outcome == TerminationOutcome::HardKilled as i32,
            "post-terminate outcome must be SOFT or HARD; got {outcome}"
        );
    })
    .await
    .expect("blocking task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipe_terminate_records_soft_or_hard_outcome() {
    let scope = format!("term-out-pipe-{}", line!());
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
                    .with_originator("term-out-pipe"),
            )
            .expect("spawn");
        client
            .terminate_pipe_session(&session.session_id, 1000)
            .expect("terminate");

        let deadline = Instant::now() + Duration::from_secs(15);
        let outcome = loop {
            let listed = client.list_pipe_sessions("").expect("list");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session.session_id) {
                if entry.exited {
                    break entry.termination_outcome;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        assert!(
            outcome == TerminationOutcome::SoftExit as i32
                || outcome == TerminationOutcome::HardKilled as i32,
            "post-terminate outcome must be SOFT or HARD; got {outcome}"
        );
    })
    .await
    .expect("blocking task");
}
