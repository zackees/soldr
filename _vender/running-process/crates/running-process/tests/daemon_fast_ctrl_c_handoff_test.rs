#![cfg(feature = "daemon")]
//! Fast Ctrl+C handoff verification (#130 milestone 4).
//!
//! The headline assertion: a client's call to `TerminateSession` completes
//! in well under 100 ms regardless of how long the child takes to clean
//! up. The daemon owns the soft-then-hard escalation on a background task;
//! the client is fire-and-forget from the moment the RPC accepts.
//!
//! These tests run two daemon-managed children whose process tree takes a
//! long time to die (sleeper sleeps for an hour). The test measures
//! wall-clock between calling `terminate_pty_session` /
//! `terminate_pipe_session` and the RPC returning. The bound is intentionally
//! tight (200 ms) — even on a slow CI runner the daemon's accept path
//! should not be anywhere close to that.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pipe_session::PipeSpawnRequest;
use running_process::daemon::pty_session::PtySpawnRequest;
use running_process::daemon::server::DaemonServer;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Wall-clock budget for the client side of `TerminateSession`. Anything
/// over this means the design has regressed: the client is blocking on
/// the child instead of handing the schedule to the daemon.
const FAST_TERMINATE_BUDGET: Duration = Duration::from_millis(200);

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
        "fast-ctrl-c-test".to_string(),
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
async fn pty_terminate_returns_under_fast_budget() {
    let scope = format!("fast-pty-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let req = PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
            .with_originator("fast-pty");
        let session = client.spawn_pty_session(&req).expect("spawn");

        // Measure ONLY the terminate RPC round-trip.
        let started = Instant::now();
        client
            .terminate_pty_session(&session.session_id, 2000)
            .expect("terminate");
        let elapsed = started.elapsed();

        assert!(
            elapsed < FAST_TERMINATE_BUDGET,
            "PTY terminate RPC took {elapsed:?}, expected < {FAST_TERMINATE_BUDGET:?}; \
             this is the #130 M4 fast-Ctrl+C invariant"
        );

        // The session should eventually transition to exited. We don't
        // assert *how fast* — only that the daemon's background schedule
        // succeeds. Use a wide deadline because hard-kill escalation
        // happens after the grace_ms window.
        let exit_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = client.list_pty_sessions("").expect("list");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session.session_id) {
                if entry.exited {
                    return;
                }
            }
            if Instant::now() >= exit_deadline {
                panic!("PTY session did not exit within 15s after terminate");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })
    .await
    .expect("blocking task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipe_terminate_returns_under_fast_budget() {
    let scope = format!("fast-pipe-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let env_reporter = tokio::task::spawn_blocking(|| testbin_path("testbin-env-reporter"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let req = PipeSpawnRequest::new([env_reporter.to_string_lossy().into_owned()])
            .with_originator("fast-pipe");
        let session = client.spawn_pipe_session(&req).expect("spawn");

        let started = Instant::now();
        client
            .terminate_pipe_session(&session.session_id, 2000)
            .expect("terminate");
        let elapsed = started.elapsed();

        assert!(
            elapsed < FAST_TERMINATE_BUDGET,
            "pipe terminate RPC took {elapsed:?}, expected < {FAST_TERMINATE_BUDGET:?}; \
             this is the #130 M4 fast-Ctrl+C invariant"
        );

        let exit_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = client.list_pipe_sessions("").expect("list");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session.session_id) {
                if entry.exited {
                    return;
                }
            }
            if Instant::now() >= exit_deadline {
                panic!("pipe session did not exit within 15s after terminate");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })
    .await
    .expect("blocking task");
}

/// Even if we ask for a very generous grace window, the client RPC
/// must return promptly — the grace_ms is the daemon's clock, not the
/// client's. This is what makes "Ctrl+C returns instantly" work
/// regardless of how patient the soft signal is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_grace_ms_does_not_block_the_client() {
    let scope = format!("fast-grace-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let session = client
            .spawn_pty_session(
                &PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
                    .with_originator("fast-grace"),
            )
            .expect("spawn");

        // 30s grace — if the client blocked on this, the test would take
        // half a minute. The assertion below would still pass against a
        // bad implementation if elapsed < 30s, so we keep the tight
        // FAST_TERMINATE_BUDGET to make the regression obvious.
        let started = Instant::now();
        client
            .terminate_pty_session(&session.session_id, 30_000)
            .expect("terminate");
        let elapsed = started.elapsed();

        assert!(
            elapsed < FAST_TERMINATE_BUDGET,
            "PTY terminate with 30s grace took {elapsed:?}, expected < {FAST_TERMINATE_BUDGET:?}"
        );

        // We do not wait for the session to exit here — the 30s grace
        // means hard kill happens at t+30s. Force-terminate with grace=0
        // to clean up quickly.
        client
            .terminate_pty_session(&session.session_id, 0)
            .expect("force terminate");
    })
    .await
    .expect("blocking task");
}
