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
//! # Scope
//!
//! Windows only for now, matching the existing named-pipe test precedent in
//! `daemon::ipc_peer`. The Unix arm needs a `UnixListener` twin; it is
//! deliberately not written blind on a host that cannot run it.

#![cfg(windows)]

use soldr_cli::timed_test;
use std::time::Duration;

/// Short enough that the test finishes quickly, long enough that a loaded
/// runner does not mistake normal scheduling for the stall being tested.
const STALL_BUDGET_SECS: &str = "3";

timed_test!(
    a_daemon_that_accepts_then_never_answers_reports_a_wedged_stall,
    Duration::from_secs(60),
    {
        // Must happen before anything touches `compile_reply_timeout()`.
        std::env::set_var("SOLDR_COMPILE_REPLY_TIMEOUT_SECS", STALL_BUDGET_SECS);

        let pipe_name = format!(
            r"\\.\pipe\soldr-stall-harness-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );

        // The wedged daemon: accept the connection, then go silent forever.
        // Owning the server inside one runtime on one thread keeps the tokio
        // resource on the reactor that created it.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let server_pipe = pipe_name.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            runtime.block_on(async move {
                let server = tokio::net::windows::named_pipe::ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(&server_pipe)
                    .expect("create wedged-daemon pipe");
                // Signal only after the pipe exists, so the client cannot
                // race ahead and see "no such pipe" instead of the stall.
                ready_tx.send(()).expect("signal ready");
                server.connect().await.expect("accept client");
                // Deliberately answer nothing: this is the wedge. Outlive the
                // client's budget so the client times out rather than seeing
                // a closed pipe (which would be a different error entirely).
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        });

        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("wedged daemon pipe never became ready");

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
            std::path::Path::new(&pipe_name),
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
            elapsed < Duration::from_secs(30),
            "the stall must be bounded by SOLDR_COMPILE_REPLY_TIMEOUT_SECS, took {elapsed:?}"
        );
        assert!(
            stdout.is_empty() && stderr.is_empty(),
            "a wedged daemon produced no bytes, so nothing should have been relayed"
        );

        drop(server);
    }
);
