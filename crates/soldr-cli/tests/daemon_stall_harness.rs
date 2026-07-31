//! Fault-injection harness: a deliberately wedged daemon (soldr#1838 Phase 4).
//!
//! Phase 4 asks for "a fault-injection test harness: a deliberately
//! slow/wedged daemon, to exercise the warning paths without waiting out real
//! backstops." Everything else in the stall story is unit-tested against
//! synthesized errors — the degrade policy table, the `CompileStalled`
//! slow-vs-wedged split, the heartbeat's firing and wording. What none of
//! those cover is the *transport*: that a daemon which accepts a connection
//! and then never answers really does surface as `CompileStalled` with
//! `saw_output: false`, rather than a connection error or a hang.
//!
//! # Why this is its own test binary
//!
//! `client::compile_reply_timeout()` caches its value in a `OnceLock`, so the
//! first caller in a process fixes the deadline for that whole binary. Cargo
//! gives every integration-test file its own process, so a file containing
//! exactly one stall test can set `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` before
//! anything reads it and get a deterministic short budget — no test-only seam
//! in the production timeout function, and no ordering hazard for other tests.
//! That is the whole reason this lives here rather than beside the other
//! client tests.
//!
//! # Both transports
//!
//! The wedge is set up per platform — a named pipe on Windows (mirroring the
//! precedent in `daemon::ipc_peer`), a `UnixListener` elsewhere — but the
//! assertions are shared, because the contract they check is the transport's
//! *observable behaviour*, which must be identical on both.

use soldr_cli::timed_test;
use std::time::Duration;

/// Short enough that the test finishes quickly, long enough that a loaded
/// runner does not mistake normal scheduling for the stall being tested.
const STALL_BUDGET_SECS: &str = "3";

/// How long the fake daemon stays silent. Must outlive the client's budget so
/// the client times out rather than seeing a closed endpoint, which would be a
/// different error entirely.
const WEDGE_HOLD: Duration = Duration::from_secs(30);

/// A short process-unique suffix for unix socket paths, which are length
/// limited (see `spawn_wedged_daemon`). Keeps enough entropy to avoid a
/// collision with a leftover directory from an earlier run.
#[cfg(unix)]
fn terse_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos()
            % 100_000
    )
}

/// A process-unique endpoint name, so concurrent test binaries never collide.
#[cfg(windows)]
fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    )
}

/// Stand up a daemon that accepts a connection and then answers nothing.
///
/// Returns the endpoint the client should dial, plus a guard that keeps the
/// server alive (and, on Unix, cleans up the socket directory).
#[cfg(windows)]
fn spawn_wedged_daemon() -> (std::path::PathBuf, WedgeGuard) {
    let pipe_name = format!(r"\\.\pipe\soldr-stall-harness-{}", unique_suffix());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let server_pipe = pipe_name.clone();
    let handle = std::thread::spawn(move || {
        // Own the pipe inside one runtime on one thread, so the tokio
        // resource stays on the reactor that created it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        runtime.block_on(async move {
            let server = tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .create(&server_pipe)
                .expect("create wedged-daemon pipe");
            // Signal only once the pipe exists, so the client cannot race
            // ahead and see "no such pipe" instead of the stall.
            ready_tx.send(()).expect("signal ready");
            server.connect().await.expect("accept client");
            tokio::time::sleep(WEDGE_HOLD).await;
        });
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("wedged daemon pipe never became ready");
    (
        std::path::PathBuf::from(pipe_name),
        WedgeGuard {
            handle: Some(handle),
            dir: None,
        },
    )
}

#[cfg(unix)]
fn spawn_wedged_daemon() -> (std::path::PathBuf, WedgeGuard) {
    use std::os::unix::net::UnixListener;

    // A unix socket path must fit `sun_path`, which is ~104 bytes on macOS.
    // `env::temp_dir()` there is `$TMPDIR` -- a long `/var/folders/../T/`
    // path -- so a descriptive directory name under it overflows and `bind`
    // fails with "path must be shorter than SUN_LEN". Bind under `/tmp` with
    // a terse name instead; it is short and writable on every unix CI image.
    let dir = std::path::PathBuf::from("/tmp").join(format!("sldr-st-{}", terse_suffix()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create wedge dir");
    let sock = dir.join("d.sock");
    assert!(
        sock.as_os_str().len() < 100,
        "socket path must fit sun_path on every unix, got {} bytes: {sock:?}",
        sock.as_os_str().len()
    );
    // Bind on this thread so the socket exists before the client dials; the
    // listener's backlog then holds the connection until the thread accepts.
    let listener = UnixListener::bind(&sock).expect("bind wedged-daemon socket");
    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            // Deliberately answer nothing: this is the wedge. Holding the
            // stream keeps the peer connected rather than seeing EOF.
            std::thread::sleep(WEDGE_HOLD);
            drop(stream);
        }
    });
    (
        sock,
        WedgeGuard {
            handle: Some(handle),
            dir: Some(dir),
        },
    )
}

/// Keeps the fake daemon's thread owned by the test and removes any temp
/// directory it created. The thread parks in a bounded sleep, so it is left
/// detached rather than joined — joining would make the test wait out
/// `WEDGE_HOLD` for no benefit.
struct WedgeGuard {
    handle: Option<std::thread::JoinHandle<()>>,
    dir: Option<std::path::PathBuf>,
}

impl Drop for WedgeGuard {
    fn drop(&mut self) {
        self.handle.take();
        if let Some(dir) = self.dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

timed_test!(
    a_daemon_that_accepts_then_never_answers_reports_a_wedged_stall,
    Duration::from_secs(60),
    {
        // Must happen before anything touches `compile_reply_timeout()`.
        std::env::set_var("SOLDR_COMPILE_REPLY_TIMEOUT_SECS", STALL_BUDGET_SECS);

        let (endpoint, _wedge) = spawn_wedged_daemon();

        let request = soldr_cli::daemon::protocol::CompileRequest {
            args: vec!["rustc".to_string(), "--version".to_string()],
            cwd: std::env::current_dir().expect("cwd").display().to_string(),
            env: Vec::new(),
            stdin: Vec::new(),
            lifecycle: None,
            ipc_busy_retries: 0,
        };

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let started = std::time::Instant::now();
        let result = soldr_cli::daemon::client::compile_streaming(
            &endpoint,
            request,
            &mut stdout,
            &mut stderr,
        );
        let elapsed = started.elapsed();

        let error = result.expect_err("a daemon that never answers must not report success");
        match error {
            soldr_cli::daemon::client::ClientError::CompileStalled {
                saw_output,
                elapsed: reported,
            } => {
                // The whole point of the variant: nothing arrived, so this is
                // the wedge case, whose remedy is to bypass the cache -- not
                // the slow-build case, whose remedy is a longer deadline.
                assert!(
                    !saw_output,
                    "a silent daemon must report saw_output=false, or the \
                     wrapper will advise raising the timeout instead of \
                     bypassing a wedged cache"
                );
                assert!(
                    reported >= Duration::from_secs(1),
                    "the reported elapsed should reflect the real wait, got {reported:?}"
                );
            }
            other => panic!("expected CompileStalled from a wedged daemon, got {other:?}"),
        }

        // Without the budget being honoured this would sit for the 30-minute
        // default, which is exactly what Phase 4 wants provable.
        assert!(
            elapsed < WEDGE_HOLD,
            "the stall must be bounded by SOLDR_COMPILE_REPLY_TIMEOUT_SECS, took {elapsed:?}"
        );
        assert!(
            stdout.is_empty() && stderr.is_empty(),
            "a wedged daemon produced no bytes, so nothing should have been relayed"
        );
    }
);
