#![cfg(feature = "daemon")]
//! Tree-with-grandchildren + stubborn-child tests (#130 M4 follow-up).
//!
//! Two assertions about the daemon's soft-then-hard schedule:
//!
//! 1. When a pipe session's child has grandchildren, terminating the
//!    session reaps the whole tree (no orphan grandchildren survive).
//!    Holds on both POSIX (via process-group SIGTERM/SIGKILL) and
//!    Windows (via Job Object kill-on-close).
//!
//! 2. (POSIX only) A stubborn child that ignores SIGTERM is reaped via
//!    the hard-kill escalation after the grace window, and the
//!    daemon's `TerminationOutcome` records HARD_KILLED.

#[cfg(unix)]
use running_process::daemon::client::DaemonClient;
#[cfg(unix)]
use running_process::daemon::paths;
#[cfg(unix)]
use running_process::daemon::pipe_session::PipeSpawnRequest;
#[cfg(unix)]
use running_process::daemon::server::DaemonServer;
#[cfg(unix)]
use running_process::proto::daemon::PipeStreamKind;
#[cfg(unix)]
use running_process::proto::daemon::TerminationOutcome;

#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
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

#[cfg(unix)]
fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();
    let server = DaemonServer::new(
        socket.clone(),
        db,
        "tree-kill-test".to_string(),
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

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    use libc::{kill, ESRCH};
    unsafe {
        if kill(pid as i32, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
    }
}

/// POSIX-only because `testbin-spawner` deliberately spawns its
/// grandchildren via `running_process::spawn_daemon` on Windows,
/// which sets `CREATE_BREAKAWAY_FROM_JOB` so the grandchildren escape
/// the parent's Job Object (used by `testbin-spawner`'s original
/// containment-test purpose). That is intentional for the spawner
/// fixture's other use sites; for a tree-kill assertion it means
/// Windows would need a different spawner. The POSIX path puts the
/// grandchildren in the spawner's process group (inherited via
/// `Command::spawn`), so `kill(-pgid, SIGTERM/SIGKILL)` from the
/// daemon reaches the whole tree.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminate_reaps_grandchildren_along_with_spawner() {
    let scope = format!("tree-kill-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let spawner = tokio::task::spawn_blocking(|| testbin_path("testbin-spawner"))
        .await
        .expect("spawner testbin");
    let sleeper = tokio::task::spawn_blocking(|| testbin_path("testbin-sleeper"))
        .await
        .expect("sleeper testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");

        // Pipe-session-spawn the spawner with 3 grandchildren.
        let spawn_req = PipeSpawnRequest::new([
            spawner.to_string_lossy().into_owned(),
            "3".to_string(),
            sleeper.to_string_lossy().into_owned(),
        ])
        .with_originator("tree-kill");
        let session = client.spawn_pipe_session(&spawn_req).expect("spawn");

        // Read stdout to capture grandchild PIDs (and the READY marker
        // so we know the spawner finished spawning). Use the daemon's
        // snapshot RPC instead of attaching so the test does not have
        // to manage a streaming connection. Poll until READY appears.
        let mut combined = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !String::from_utf8_lossy(&combined).contains("READY") {
            if Instant::now() >= deadline {
                panic!(
                    "spawner did not print READY within budget; got: {:?}",
                    String::from_utf8_lossy(&combined)
                );
            }
            std::thread::sleep(Duration::from_millis(150));
            let snap = client
                .get_session_backlog(&session.session_id, PipeStreamKind::Stdout)
                .expect("snap")
                .expect("session");
            combined = snap.backlog;
        }
        let text = String::from_utf8_lossy(&combined).into_owned();
        let mut grandchild_pids: Vec<u32> = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("CHILD_PID=") {
                if let Ok(pid) = rest.trim().parse::<u32>() {
                    grandchild_pids.push(pid);
                }
            }
        }
        assert_eq!(
            grandchild_pids.len(),
            3,
            "expected 3 CHILD_PID lines, got: {text:?}"
        );
        for pid in &grandchild_pids {
            assert!(
                pid_is_alive(*pid),
                "grandchild PID {pid} should be alive before terminate"
            );
        }

        // Terminate and wait for the session to exit.
        client
            .terminate_pipe_session(&session.session_id, 1000)
            .expect("terminate");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = client.list_pipe_sessions("").expect("list");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session.session_id) {
                if entry.exited {
                    break;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not exit within budget");
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        // Wait for the grandchildren to die. POSIX needs the process-
        // group kill to propagate; Windows relies on the Job Object.
        let deadline = Instant::now() + Duration::from_secs(10);
        for pid in &grandchild_pids {
            loop {
                if !pid_is_alive(*pid) {
                    break;
                }
                if Instant::now() >= deadline {
                    panic!("grandchild PID {pid} survived session terminate");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })
    .await
    .expect("blocking task");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stubborn_child_is_hard_killed_after_grace() {
    let scope = format!("stubborn-{}", line!());
    let (_handle, socket) = start_server(&scope);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let stubborn = tokio::task::spawn_blocking(|| testbin_path("testbin-stubborn"))
        .await
        .expect("stubborn testbin");
    let socket_for_test = socket.clone();

    tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_test).expect("connect");
        let session = client
            .spawn_pipe_session(
                &PipeSpawnRequest::new([stubborn.to_string_lossy().into_owned()])
                    .with_originator("stubborn-child"),
            )
            .expect("spawn");

        // Give the testbin time to install its SIG_IGN handler for
        // SIGTERM. Without this, the immediate terminate below races
        // the child's main() and can hit the default SIGTERM action
        // (terminate), which would be (correctly) classified as a
        // SoftExit and break this test's HARD_KILLED assertion.
        std::thread::sleep(Duration::from_millis(500));

        // Generous grace so SoftExit would be plausible if the child
        // responded to SIGTERM. It doesn't, so HARD_KILLED is required.
        let grace_ms: u32 = 1500;
        client
            .terminate_pipe_session(&session.session_id, grace_ms)
            .expect("terminate");

        // The session should eventually exit, and the outcome should
        // be HARD_KILLED (the stubborn child ignored the SIGTERM).
        let deadline = Instant::now() + Duration::from_secs(15);
        let outcome = loop {
            let listed = client.list_pipe_sessions("").expect("list");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session.session_id) {
                if entry.exited {
                    break entry.termination_outcome;
                }
            }
            if Instant::now() >= deadline {
                panic!("stubborn session did not exit");
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        assert_eq!(
            outcome,
            TerminationOutcome::HardKilled as i32,
            "stubborn child should be HARD_KILLED; got outcome={outcome}"
        );
    })
    .await
    .expect("blocking task");
}
