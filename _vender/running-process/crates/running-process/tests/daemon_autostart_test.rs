#![cfg(feature = "daemon")]
//! Direct test of `spawn_autostart_sessions` (#130 M7 B3).
//!
//! End-to-end testing of the config-file → daemon startup path would
//! require overriding `DaemonConfig::config_path()`, which is not
//! supported today. Instead, this test calls
//! `running_process::daemon::server::spawn_autostart_sessions` directly
//! with a constructed `AutostartSession` list and asserts the registry
//! contains the spawned sessions afterwards. The function under test is
//! exactly the code path that real autostart hits during
//! `DaemonServer::run`, just bypassing the file load.

use running_process::daemon::config::AutostartSession;
use running_process::daemon::handlers::DaemonState;
use running_process::daemon::paths;
use running_process::daemon::pipe_sessions::PipeSessionRegistry;
use running_process::daemon::pty_sessions::PtySessionRegistry;
use running_process::daemon::registry::Registry;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
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

fn build_test_state(scope: &str) -> (DaemonState, tempfile::TempDir) {
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;
    use tokio::sync::watch;
    let tmp = tempfile::TempDir::new().expect("tmpdir");
    let db_path = tmp.path().join("autostart-test.db");
    let registry = Arc::new(Registry::open(&db_path).expect("registry"));
    let pty_sessions = Arc::new(PtySessionRegistry::new());
    let pipe_sessions = Arc::new(PipeSessionRegistry::new());
    let services = Arc::new(
        running_process::daemon::services::ServiceRegistry::open(
            &db_path,
            tmp.path().join("services"),
        )
        .expect("service registry"),
    );
    let (shutdown_tx, _rx) = watch::channel(false);
    let state = DaemonState {
        start_time: Instant::now(),
        version: "0.0.0-test".to_string(),
        socket_path: paths::socket_path(Some(scope)),
        db_path: db_path.display().to_string(),
        scope: scope.to_string(),
        scope_hash: "0000000000000000".to_string(),
        scope_cwd: "/tmp".to_string(),
        shutdown_tx,
        active_connections: AtomicU32::new(0),
        registry,
        pty_sessions,
        pipe_sessions,
        services,
        emergency_reserve: Arc::new(
            running_process::daemon::emergency_reserve::EmergencyReserve::initialize_at(
                tmp.path().join("emergency-reserve.bin"),
                4096,
            ),
        ),
    };
    (state, tmp)
}

#[test]
fn autostart_spawns_pty_and_pipe_entries() {
    let sleeper = testbin_path("testbin-sleeper");
    let env_reporter = testbin_path("testbin-env-reporter");

    let (state, _tmp) = build_test_state("autostart-test");

    let entries = vec![
        AutostartSession {
            kind: "pty".into(),
            argv: vec![sleeper.to_string_lossy().into_owned()],
            originator: "autostart-pty".into(),
            rows: 30,
            cols: 100,
            ..Default::default()
        },
        AutostartSession {
            kind: "pipe".into(),
            argv: vec![env_reporter.to_string_lossy().into_owned()],
            originator: "autostart-pipe".into(),
            ..Default::default()
        },
        // Empty argv: should be silently skipped, not crash.
        AutostartSession {
            kind: "pty".into(),
            argv: vec![],
            ..Default::default()
        },
    ];

    running_process::daemon::server::spawn_autostart_sessions(&state, &entries);

    let pty_list = state.pty_sessions.list();
    let pipe_list = state.pipe_sessions.list();
    assert_eq!(pty_list.len(), 1, "expected one PTY autostart entry");
    assert_eq!(pipe_list.len(), 1, "expected one pipe autostart entry");

    assert_eq!(pty_list[0].originator, "autostart-pty");
    assert_eq!(pty_list[0].rows(), 30);
    assert_eq!(pty_list[0].cols(), 100);
    assert_eq!(pipe_list[0].originator, "autostart-pipe");

    // Clean up: terminate so the processes do not linger after the
    // test exits.
    pty_list[0]
        .terminate(std::time::Duration::from_millis(200))
        .ok();
    pipe_list[0]
        .terminate(std::time::Duration::from_millis(200))
        .ok();
    // Brief wait so the kill propagates before the test process exits.
    std::thread::sleep(Duration::from_millis(500));
}
