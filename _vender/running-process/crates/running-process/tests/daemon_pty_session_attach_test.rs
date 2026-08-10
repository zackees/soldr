#![cfg(feature = "daemon")]
//! Integration test for daemon-owned detachable PTY sessions
//! (issue #130 milestone 2).
//!
//! Starts a `DaemonServer` on a unique socket path, then drives it via two
//! independent `Stream` connections to verify that:
//!   * A PTY session can be spawned and survives the lifetime of any one
//!     client connection.
//!   * The single-attachment invariant is enforced.
//!   * Detach + reattach produces a coherent stream.
//!   * Terminate produces a recorded exit state visible via list.
//!
//! These tests run two independent OS-level socket clients against the
//! daemon — that is sufficient to exercise the daemon's
//! attachment-ownership protocol. A full second-OS-process test that spawns
//! the daemon binary is added in a follow-up commit; it exercises the same
//! protocol path but additionally validates binary startup, socket-path
//! handshake, and PTY handle inheritance on Windows ConPTY.

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::pty_session::{PtyAttachment, PtySpawnRequest};
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::{pty_stream_frame::Frame as StreamOneof, StatusCode};

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// Locate (and build, if needed) the path of a `testbin-*` binary.
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
        "pty-test".to_string(),
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
async fn spawn_attach_detach_reattach_terminate_lifecycle() {
    let scope = format!("pty-{}", line!());
    let (_server_handle, socket) = start_server(&scope);

    // Allow socket bind.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper_path = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("spawn_blocking for testbin path");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut control = DaemonClient::connect_to(&socket_for_test).expect("control connect");

        // -------------------------------------------------------------------
        // 1. Spawn a PTY session owned by the daemon.
        // -------------------------------------------------------------------
        let argv = vec![sleeper_path.to_string_lossy().into_owned()];
        let spawn_req = PtySpawnRequest::new(argv)
            .with_size(24, 80)
            .with_originator("pty-session-attach-test");
        let spawned = control
            .spawn_pty_session(&spawn_req)
            .expect("spawn_pty_session");
        assert!(!spawned.session_id.is_empty());
        assert!(spawned.pid > 0);

        // -------------------------------------------------------------------
        // 2. List shows the session and its attached state.
        // -------------------------------------------------------------------
        let listed = control.list_pty_sessions("").expect("list_pty_sessions");
        let entry = listed
            .iter()
            .find(|s| s.session_id == spawned.session_id)
            .expect("spawned session not present in list");
        assert!(!entry.attached);
        assert!(!entry.exited);

        // -------------------------------------------------------------------
        // 3. Attach via a separate connection. The first attach succeeds and
        //    receives an empty initial backlog.
        // -------------------------------------------------------------------
        let mut attach_a =
            PtyAttachment::attach_to(&socket_for_test, &spawned.session_id, 30, 100, false)
                .expect("first attach");
        // For sleeper, no output is produced yet so the backlog is empty.
        // Resize-on-attach should have happened — verify via list.
        let after_attach = control.list_pty_sessions("").expect("list after attach");
        let entry = after_attach
            .iter()
            .find(|s| s.session_id == spawned.session_id)
            .expect("session disappeared");
        assert!(entry.attached, "session should report attached=true");
        assert_eq!(entry.rows, 30, "rows should reflect attach client size");
        assert_eq!(entry.cols, 100, "cols should reflect attach client size");

        // -------------------------------------------------------------------
        // 4. Single-attachment enforcement: second attach without steal
        //    returns ALREADY_ATTACHED.
        // -------------------------------------------------------------------
        match PtyAttachment::attach_to(&socket_for_test, &spawned.session_id, 24, 80, false) {
            Ok(_) => panic!("second attach should not succeed without steal"),
            Err(running_process::daemon::pty_session::AttachError::Server { code, .. }) => {
                assert_eq!(
                    code,
                    StatusCode::AlreadyAttached,
                    "expected ALREADY_ATTACHED, got {code:?}"
                );
            }
            Err(other) => panic!("unexpected error variant: {other}"),
        }

        // -------------------------------------------------------------------
        // 5. Write a few input bytes (sleeper drops them — the assertion is
        //    that the write succeeds and does not tear down the attachment).
        // -------------------------------------------------------------------
        attach_a.send_input(b"hello\n").expect("send_input");

        // -------------------------------------------------------------------
        // 6. Clean detach via the input frame, then verify the session is
        //    still alive in the registry.
        // -------------------------------------------------------------------
        attach_a.detach().expect("detach via input frame");
        std::thread::sleep(Duration::from_millis(100));
        let after_detach = control.list_pty_sessions("").expect("list after detach");
        let entry = after_detach
            .iter()
            .find(|s| s.session_id == spawned.session_id)
            .expect("session must outlive detach");
        assert!(
            !entry.attached,
            "session should report attached=false after detach"
        );
        assert!(!entry.exited, "session should still be running");

        // -------------------------------------------------------------------
        // 7. Reattach from a fresh connection; the second attach succeeds.
        // -------------------------------------------------------------------
        let _attach_b =
            PtyAttachment::attach_to(&socket_for_test, &spawned.session_id, 24, 80, false)
                .expect("reattach");

        // -------------------------------------------------------------------
        // 8. Terminate; the reader thread observes the exit, records
        //    exit_state, and notifies the attached client (drop happens
        //    when reattach object falls out of scope).
        // -------------------------------------------------------------------
        control
            .terminate_pty_session(&spawned.session_id, 1000)
            .expect("terminate_pty_session");

        // Poll until the session reports exited. sleeper terminates promptly
        // on a process-tree terminate; allow a wide budget for slow CI.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = control.list_pty_sessions("").expect("list during wait");
            if let Some(entry) = listed.iter().find(|s| s.session_id == spawned.session_id) {
                if entry.exited {
                    break;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not transition to exited within budget");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    })
    .await
    .expect("blocking task panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_filters_by_originator() {
    let scope = format!("pty-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper_path = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let argv = vec![sleeper_path.to_string_lossy().into_owned()];
        let a = client
            .spawn_pty_session(&PtySpawnRequest::new(argv.clone()).with_originator("alpha"))
            .expect("spawn a");
        let b = client
            .spawn_pty_session(&PtySpawnRequest::new(argv).with_originator("beta"))
            .expect("spawn b");

        let alpha_only = client.list_pty_sessions("alpha").expect("list alpha");
        let alpha_ids: Vec<&str> = alpha_only.iter().map(|s| s.session_id.as_str()).collect();
        assert!(alpha_ids.contains(&a.session_id.as_str()));
        assert!(!alpha_ids.contains(&b.session_id.as_str()));

        // Clean up.
        client
            .terminate_pty_session(&a.session_id, 500)
            .expect("terminate a");
        client
            .terminate_pty_session(&b.session_id, 500)
            .expect("terminate b");
    })
    .await
    .expect("blocking task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_with_steal_evicts_existing_client() {
    let scope = format!("pty-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let sleeper_path = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("testbin");

    let socket_for_test = socket.clone();
    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let argv = vec![sleeper_path.to_string_lossy().into_owned()];
        let session = client
            .spawn_pty_session(&PtySpawnRequest::new(argv))
            .expect("spawn");

        // First attach.
        let mut attach_a =
            PtyAttachment::attach_to(&socket_for_test, &session.session_id, 24, 80, false)
                .expect("attach a");

        // Steal attach.
        let _attach_b =
            PtyAttachment::attach_to(&socket_for_test, &session.session_id, 24, 80, true)
                .expect("steal attach b");

        // First attachment should eventually receive a terminal frame
        // indicating it was stolen. The exact frame variant is permissive:
        // stolen_by or error("…"). Drain any output/missed-bytes frames
        // already queued before the steal landed.
        loop {
            let frame = attach_a.recv_frame().expect("recv after steal");
            match frame.frame {
                Some(StreamOneof::Output(_)) | Some(StreamOneof::MissedBytes(_)) => continue,
                Some(StreamOneof::StolenBy(_)) | Some(StreamOneof::Error(_)) => break,
                other => panic!("expected terminal frame on stolen attachment, got {other:?}"),
            }
        }

        // Clean up.
        client
            .terminate_pty_session(&session.session_id, 500)
            .expect("terminate");
    })
    .await
    .expect("blocking task");
}
