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
//! The short deadline is passed straight to
//! `client::compile_streaming_with_timeout`. It used to be requested by
//! setting `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` and relying on being the first
//! caller of `compile_reply_timeout()`, whose `OnceLock` gives the whole
//! process whatever the winner of that race asked for — which this test cannot
//! win from inside the shared `daemon` category binary, and which handed its
//! own budget to every sibling when it did (soldr#2955). Nothing here is
//! process-global now, so the test is order-independent under both plain
//! `cargo test` and nextest.
//!
//! # Both transports
//!
//! The wedge is set up per platform — a named pipe on Windows (mirroring the
//! precedent in `daemon::ipc_peer`), a `UnixListener` elsewhere — but the
//! assertions are shared, because the contract they check is the transport's
//! *observable behaviour*, which must be identical on both.

use std::time::Duration;

/// Short enough that the test finishes quickly, long enough that a loaded
/// runner does not mistake normal scheduling for the stall being tested.
const STALL_BUDGET: Duration = Duration::from_secs(3);

/// How long the fake daemon stays silent. Must outlive the client's budget so
/// the client times out rather than seeing a closed endpoint, which would be a
/// different error entirely.
const WEDGE_HOLD: Duration = Duration::from_secs(30);

/// Stand up a daemon that accepts a connection and then answers nothing.
///
/// Returns the endpoint the client should dial, plus a guard that keeps the
/// server alive and retires its host endpoint.
fn spawn_wedged_daemon() -> (std::path::PathBuf, WedgeGuard) {
    let endpoint = soldr_platform::ipc::endpoint::ephemeral("soldr-stall-harness");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let server_endpoint = endpoint.clone();
    let handle = std::thread::spawn(move || {
        // Own the listener inside one runtime on one thread, so the tokio
        // resource stays on the reactor that created it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        runtime.block_on(async move {
            use interprocess::local_socket::traits::tokio::Listener as _;
            let listener = soldr_platform::ipc::broker::bind_listener(&server_endpoint, 1)
                .expect("create wedged-daemon listener");
            ready_tx.send(()).expect("signal ready");
            let stream = listener.accept().await.expect("accept client");
            tokio::time::sleep(WEDGE_HOLD).await;
            drop(stream);
        });
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("wedged daemon pipe never became ready");
    (
        std::path::PathBuf::from(&endpoint),
        WedgeGuard {
            handle: Some(handle),
            endpoint: Some(endpoint),
        },
    )
}

/// Keeps the fake daemon's thread owned by the test and removes any temp
/// directory it created. The thread parks in a bounded sleep, so it is left
/// detached rather than joined — joining would make the test wait out
/// `WEDGE_HOLD` for no benefit.
struct WedgeGuard {
    handle: Option<std::thread::JoinHandle<()>>,
    endpoint: Option<String>,
}

impl Drop for WedgeGuard {
    fn drop(&mut self) {
        self.handle.take();
        if let Some(endpoint) = self.endpoint.take() {
            soldr_platform::ipc::broker::retire_endpoint(&endpoint);
        }
    }
}

#[test]
fn a_daemon_that_accepts_then_never_answers_reports_a_wedged_stall() {
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
    let result = soldr_cli::daemon::client::compile_streaming_with_timeout(
        &endpoint,
        request,
        &mut stdout,
        &mut stderr,
        STALL_BUDGET,
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
        "the stall must be bounded by the reply budget the caller passed in, took {elapsed:?}"
    );
    assert!(
        stdout.is_empty() && stderr.is_empty(),
        "a wedged daemon produced no bytes, so nothing should have been relayed"
    );
}
