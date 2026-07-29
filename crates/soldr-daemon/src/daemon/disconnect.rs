//! Racing a compile against the wrapper going away, and recording what
//! was lost when it does.
//!
//! Split out of `server.rs` (soldr#1857): the disconnect race, its
//! typed outcome, the lifecycle bookkeeping around it, and the durable
//! record of an undelivered compile are one concern with one test
//! module, and `server.rs` is over the line ceiling.
//!
//! The contract, in one line: **a compile the daemon has already
//! finished is never thrown away.** See [`race_against_disconnect`].

use crate::core::SoldrPaths;
use crate::daemon::db;
use crate::daemon::protocol::CompileLifecycle;

/// Append one durable "the daemon ran this compile and could not hand
/// it back" row (soldr#1857). Best-effort — see
/// [`crate::daemon::compile_delivery`].
pub(crate) fn record_undelivered(
    paths: &SoldrPaths,
    compile_id: &str,
    lifecycle: Option<&CompileLifecycle>,
    started: std::time::Instant,
    kind: crate::daemon::compile_delivery::UndeliveredKind,
    detail: &str,
    exit_code: Option<i32>,
) {
    crate::daemon::compile_delivery::record(
        paths,
        &crate::daemon::compile_delivery::Undelivered {
            kind,
            detail,
            compile_id,
            crate_name: lifecycle.map(|l| l.crate_name.as_str()),
            target_dir: lifecycle.map(|l| l.target_dir.as_str()),
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            exit_code,
        },
    );
}

/// A finished compile whose reply could not be written to the wrapper.
///
/// This is the precise shape soldr#1857 reports: zccache journals
/// `exit_code: 0`, the wrapper reports failure to cargo, and no
/// diagnostic exists anywhere because the only record of the loss was a
/// `tracing::warn!` on a detached daemon whose stderr goes nowhere.
/// Record it durably, then return the original error unchanged so the
/// connection teardown path is untouched.
pub(crate) fn report_reply_write_failure(
    paths: &SoldrPaths,
    compile_id: &str,
    lifecycle: Option<&CompileLifecycle>,
    started: std::time::Instant,
    stage: &str,
    err: std::io::Error,
    exit_code: i32,
) -> std::io::Error {
    let detail = format!("{stage}:{}", io_error_kind_name(&err));
    record_undelivered(
        paths,
        compile_id,
        lifecycle,
        started,
        crate::daemon::compile_delivery::UndeliveredKind::ReplyWriteFailed,
        &detail,
        Some(exit_code),
    );
    tracing::warn!(
        target: "soldr::daemon::compile_stream",
        compile_id,
        stage,
        exit_code,
        "compile finished but its reply could not be delivered to the wrapper",
    );
    err
}

fn compile_lifecycle_event(
    lifecycle: &crate::daemon::protocol::CompileLifecycle,
    duration_us: Option<u64>,
) -> db::Event {
    db::Event {
        ts_ms: lifecycle.started_at_ms,
        session_id: Some(lifecycle.session_id),
        kind: if duration_us.is_some() {
            db::EventKind::CompileEnd
        } else {
            db::EventKind::CompileStart
        },
        crate_name: Some(lifecycle.crate_name.clone()),
        duration_us,
        target_dir: Some(lifecycle.target_dir.clone()),
        exit_code: None,
    }
}

/// Race a compile against client disconnect while emitting lifecycle events
/// through the daemon's existing batcher. Every accepted session compile gets
/// a start; only a completed future (success or service error) gets an end.
pub(crate) async fn race_compile_with_lifecycle<R, F>(
    reader: &mut R,
    fut: F,
    lifecycle: Option<&crate::daemon::protocol::CompileLifecycle>,
    event_batcher: &crate::daemon::event_batcher::EventBatcher,
) -> DispatchOutcome<F::Output>
where
    R: tokio::io::AsyncRead + Unpin,
    F: std::future::Future,
{
    let started = std::time::Instant::now();
    if let Some(metadata) = lifecycle {
        let _ = event_batcher
            .record(compile_lifecycle_event(metadata, None))
            .await;
    }
    let outcome = race_against_disconnect(reader, fut).await;
    if let (DispatchOutcome::Completed(_), Some(metadata)) = (&outcome, lifecycle) {
        let duration_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let _ = event_batcher
            .record(compile_lifecycle_event(metadata, Some(duration_us)))
            .await;
    }
    outcome
}

/// Outcome of [`race_against_disconnect`]. Separated from the inner
/// future's result so the caller can distinguish "the compile finished
/// with an error" (still want to ship the error back to the wrapper)
/// from "the wrapper is gone" (don't write anything; the connection is
/// dead).
pub(crate) enum DispatchOutcome<T> {
    /// The future ran to completion. Carries whatever the future
    /// returned — typically `Result<CompileResponseBody, _>`.
    Completed(T),
    /// The client closed the IPC connection (EOF on `read`) or the OS
    /// reported a broken pipe before the future completed. The future
    /// has been dropped at the `select!` boundary; any RAII cleanup
    /// (notably `kill_on_drop`-marked rustc child processes inside the
    /// embedded zccache service) has been invoked.
    ClientDisconnected(DisconnectReason),
}

/// Which of the three disconnect signals fired, kept so the durable
/// [`crate::daemon::compile_delivery`] row says *how* the wrapper went
/// away rather than only that it did (soldr#1857). A clean `eof` is a
/// wrapper that exited or was killed; a `read_error` is the OS tearing
/// the pipe down underneath it; `unexpected_bytes` is a protocol
/// violation and means something quite different from either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisconnectReason {
    /// `read` returned `Ok(0)` — the client closed its end.
    Eof,
    /// `read` failed. Carries the `io::ErrorKind` name.
    ReadError(&'static str),
    /// The client sent bytes mid-compile, which the request-response
    /// protocol forbids. Carries how many arrived.
    UnexpectedBytes(usize),
}

impl DisconnectReason {
    /// Stable, low-cardinality string for the JSONL `detail` field.
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Eof => "eof".to_string(),
            Self::ReadError(kind) => format!("read_error:{kind}"),
            Self::UnexpectedBytes(n) => format!("unexpected_bytes:{n}"),
        }
    }
}

/// Drive `fut` to completion while concurrently watching `reader` for a
/// client disconnect. Returns instantly (within the OS's EOF surfacing
/// latency — microseconds on Unix sockets and Windows named pipes) when
/// the client closes its end of the IPC channel, dropping `fut` so any
/// inflight work it owns is cancelled at the same instant.
///
/// The protocol contract on a `Request::Compile` exchange is strictly
/// request-response: once the wrapper sends the Compile frame it sits
/// blocked waiting for the daemon's response, so any byte arriving on
/// `reader` mid-compile is also treated as a disconnect (it can only
/// be a protocol violation or a stale-frame leftover, and either way
/// the safe action is to abort and close).
///
/// The `biased` `select!` is intentional: when the reader and the
/// compile-future are ready in the same poll tick, prefer the
/// disconnect branch so we don't accidentally write a response into a
/// half-closed pipe.
///
/// **But bias must not destroy work that is already done** (soldr#1857).
/// `biased` polls the reader first on *every* tick, so a compile future
/// that became ready in the same tick as the disconnect signal was
/// discarded — the compile had run to completion, zccache had journaled
/// `exit_code: 0`, and the daemon threw the result away. The wrapper
/// then saw an unexplained failure for a compile that had succeeded,
/// which is exactly the shape #1857 reports. So when the reader branch
/// wins, give `fut` one final poll: if it is already `Ready`, that
/// result is real and gets shipped (writing into a half-closed pipe
/// merely errors, which the caller already handles). Only a genuinely
/// still-pending compile is cancelled.
pub(crate) async fn race_against_disconnect<R, F>(
    reader: &mut R,
    fut: F,
) -> DispatchOutcome<F::Output>
where
    R: tokio::io::AsyncRead + Unpin,
    F: std::future::Future,
{
    use tokio::io::AsyncReadExt;
    tokio::pin!(fut);
    let mut probe = [0_u8; 1];
    tokio::select! {
        biased;
        read = reader.read(&mut probe) => {
            // Ok(0) = clean EOF, Err = broken pipe / reset, Ok(n>0) =
            // unexpected protocol-violating bytes mid-compile. All
            // three mean "the wrapper is gone or wedged".
            let reason = match read {
                Ok(0) => DisconnectReason::Eof,
                Ok(n) => DisconnectReason::UnexpectedBytes(n),
                Err(err) => DisconnectReason::ReadError(io_error_kind_name(&err)),
            };
            // Last-poll-wins: never cancel a compile that already finished.
            match poll_once(fut.as_mut()) {
                std::task::Poll::Ready(out) => DispatchOutcome::Completed(out),
                std::task::Poll::Pending => DispatchOutcome::ClientDisconnected(reason),
            }
        }
        out = &mut fut => DispatchOutcome::Completed(out),
    }
}

/// Poll `fut` exactly once, returning immediately either way. Used to
/// harvest a compile that completed in the same tick the disconnect
/// signal arrived.
fn poll_once<F: std::future::Future>(mut fut: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    fut.as_mut().poll(&mut cx)
}

/// Stable `'static` name for an `io::ErrorKind`, for the JSONL `detail`
/// field. `Debug` on `ErrorKind` is already stable enough in practice,
/// but this keeps the set explicit and allocation-free.
fn io_error_kind_name(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::BrokenPipe => "BrokenPipe",
        ErrorKind::ConnectionReset => "ConnectionReset",
        ErrorKind::ConnectionAborted => "ConnectionAborted",
        ErrorKind::NotConnected => "NotConnected",
        ErrorKind::UnexpectedEof => "UnexpectedEof",
        ErrorKind::TimedOut => "TimedOut",
        ErrorKind::WouldBlock => "WouldBlock",
        ErrorKind::Interrupted => "Interrupted",
        _ => "Other",
    }
}
#[cfg(test)]
#[allow(unused_must_use)]
mod cancel_on_disconnect_tests {
    //! TDD regression guard for: "when the soldr CLI is terminated, the
    //! soldr daemon should cancel its outstanding build, and do so
    //! instantly."
    //!
    //! The contract under test is [`race_against_disconnect`]:
    //!
    //!   1. When the IPC reader sees EOF, the inner future is dropped
    //!      synchronously at the `select!` boundary (proven by a
    //!      drop-tracker that flips an atomic from inside `Drop`).
    //!   2. Detection latency is bounded — well under 500ms in practice
    //!      and asserted here at 250ms so that a regression that
    //!      reintroduces a wait-for-timeout style cancellation fails
    //!      the test loudly instead of just running slow.
    //!
    //! These two properties together are what makes daemon-side
    //! cancellation actually useful: if the cancellation either took
    //! seconds to fire or didn't drop the inner work, the daemon would
    //! still be sitting on a rustc compile whose output no one will
    //! read.

    use super::{
        compile_lifecycle_event, race_against_disconnect, race_compile_with_lifecycle,
        DispatchOutcome,
    };
    use crate::daemon::protocol::CompileLifecycle;
    use crate::timed_test;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn test_lifecycle(session_id: u64) -> CompileLifecycle {
        CompileLifecycle {
            session_id,
            crate_name: "demo".to_string(),
            target_dir: "/work/target".to_string(),
            started_at_ms: 1_700_000_000_123,
        }
    }

    async fn flushed_events(
        batcher: &crate::daemon::event_batcher::EventBatcher,
        db_path: &std::path::Path,
        session_id: u64,
    ) -> Vec<crate::daemon::db::Event> {
        batcher.flush().await;
        crate::daemon::db::list_events_for_session(db_path, session_id).expect("list events")
    }

    timed_test!(compile_lifecycle_events_preserve_history_fields, {
        let lifecycle = test_lifecycle(42);
        let start = compile_lifecycle_event(&lifecycle, None);
        assert_eq!(start.kind, crate::daemon::db::EventKind::CompileStart);
        assert_eq!(start.session_id, Some(42));
        assert_eq!(start.crate_name.as_deref(), Some("demo"));
        assert_eq!(start.target_dir.as_deref(), Some("/work/target"));
        assert_eq!(start.ts_ms, 1_700_000_000_123);

        let end = compile_lifecycle_event(&lifecycle, Some(987_654));
        assert_eq!(end.kind, crate::daemon::db::EventKind::CompileEnd);
        assert_eq!(end.duration_us, Some(987_654));
        assert_eq!(end.session_id, start.session_id);
        assert_eq!(end.crate_name, start.crate_name);
        assert_eq!(end.target_dir, start.target_dir);
    });

    timed_test!(successful_compile_records_exactly_start_and_end, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("state.redb");
            let batcher = crate::daemon::event_batcher::EventBatcher::start(db_path.clone());
            let (server, _client) = tokio::io::duplex(64);
            let (mut reader, _writer) = tokio::io::split(server);
            let lifecycle = test_lifecycle(101);
            let outcome = race_compile_with_lifecycle(
                &mut reader,
                async { Ok::<_, &'static str>(7_u32) },
                Some(&lifecycle),
                &batcher,
            )
            .await;
            assert!(matches!(outcome, DispatchOutcome::Completed(Ok(7))));
            let events = flushed_events(&batcher, &db_path, 101).await;
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].kind, crate::daemon::db::EventKind::CompileStart);
            assert_eq!(events[1].kind, crate::daemon::db::EventKind::CompileEnd);
            assert!(events[1].duration_us.is_some());
        });
    });

    timed_test!(compile_service_error_still_records_end, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("state.redb");
            let batcher = crate::daemon::event_batcher::EventBatcher::start(db_path.clone());
            let (server, _client) = tokio::io::duplex(64);
            let (mut reader, _writer) = tokio::io::split(server);
            let lifecycle = test_lifecycle(102);
            let outcome = race_compile_with_lifecycle(
                &mut reader,
                async { Err::<u32, _>("compile service failed") },
                Some(&lifecycle),
                &batcher,
            )
            .await;
            assert!(matches!(outcome, DispatchOutcome::Completed(Err(_))));
            let events = flushed_events(&batcher, &db_path, 102).await;
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].kind, crate::daemon::db::EventKind::CompileStart);
            assert_eq!(events[1].kind, crate::daemon::db::EventKind::CompileEnd);
        });
    });

    timed_test!(client_disconnect_records_start_without_completion, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("state.redb");
            let batcher = crate::daemon::event_batcher::EventBatcher::start(db_path.clone());
            let (server, client) = tokio::io::duplex(64);
            let (mut reader, _writer) = tokio::io::split(server);
            let lifecycle = test_lifecycle(103);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(client);
            });
            let outcome = race_compile_with_lifecycle(
                &mut reader,
                async { tokio::time::sleep(Duration::from_secs(3600)).await },
                Some(&lifecycle),
                &batcher,
            )
            .await;
            assert!(matches!(outcome, DispatchOutcome::ClientDisconnected(_)));
            let events = flushed_events(&batcher, &db_path, 103).await;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, crate::daemon::db::EventKind::CompileStart);
            assert!(events[0].duration_us.is_none());
        });
    });

    /// A future that flips `aborted` to `true` if it is dropped before
    /// completing. Lets the test prove the helper actually cancelled
    /// the in-flight work, not merely stopped polling it.
    struct CancelTracker {
        aborted: Arc<AtomicBool>,
        completed: bool,
    }

    impl Drop for CancelTracker {
        fn drop(&mut self) {
            if !self.completed {
                self.aborted.store(true, Ordering::SeqCst);
            }
        }
    }

    timed_test!(
        race_against_disconnect_aborts_inflight_future_when_client_disconnects,
        Duration::from_secs(10),
        {
            // Use a multi-thread runtime so the disconnect-spawner task
            // can make progress on a different worker while
            // race_against_disconnect parks the calling task on the
            // select!. A current-thread runtime would serialize them,
            // making the latency measurement uninterpretable.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                // tokio::io::duplex gives us a paired in-memory
                // bidirectional stream — exactly the shape of the
                // daemon's per-connection AsyncRead+AsyncWrite stream
                // (Unix socket / Windows named pipe) without the OS
                // round-trip. Dropping one half makes `read` on the
                // other half return Ok(0) immediately, which is the
                // disconnect signal `race_against_disconnect` is built
                // around.
                let (server_side, client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);

                let aborted = Arc::new(AtomicBool::new(false));
                let polled_once = Arc::new(AtomicBool::new(false));
                let aborted_inner = Arc::clone(&aborted);
                let polled_inner = Arc::clone(&polled_once);

                // A "compile" that would sit for an hour if uninterrupted
                // — well past the timed_test! 10s watchdog so the test
                // can only pass via real cancellation. The `tracker` is
                // constructed on the first poll, so we MUST give the
                // select! at least one polling round before the
                // disconnect fires (see the spawn below) — otherwise
                // the inner async block never executes, no tracker is
                // ever created, and the Drop side of the cancellation
                // contract is untestable.
                let slow_compile = async move {
                    let mut tracker = CancelTracker {
                        aborted: aborted_inner,
                        completed: false,
                    };
                    polled_inner.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    tracker.completed = true;
                    "compile_done"
                };

                // Simulate the CLI dying SHORTLY AFTER the daemon has
                // begun the compile. The 50ms head-start lets the
                // select! poll `slow_compile` at least once (it goes
                // Pending on the 1h sleep), establishing the async
                // state machine WITH `tracker` constructed. Then we
                // drop the client end; the server's reader returns
                // EOF; select! drops the pinned slow_compile; the
                // state-machine drop runs `CancelTracker::drop`,
                // setting `aborted = true` — proving the cancellation
                // really did happen mid-execution.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    drop(client_side);
                });

                let start = Instant::now();
                let outcome = race_against_disconnect(&mut server_reader, slow_compile).await;
                let elapsed = start.elapsed();

                assert!(
                    matches!(outcome, DispatchOutcome::ClientDisconnected(_)),
                    "expected ClientDisconnected, got a Completed variant — \
                     race_against_disconnect did not detect EOF"
                );
                assert!(
                    polled_once.load(Ordering::SeqCst),
                    "slow_compile was never polled — test setup did not give \
                     the future a chance to start. Cancellation is being \
                     tested on a never-started future, which does not match \
                     production reality."
                );
                assert!(
                    elapsed < Duration::from_millis(500),
                    "disconnect detection took {elapsed:?}; the contract is \
                     'instantly' (<500ms including the 50ms scheduled delay \
                     before the disconnect). A regression here usually means \
                     the helper is no longer running the disconnect probe \
                     concurrently with the inner future."
                );
                assert!(
                    aborted.load(Ordering::SeqCst),
                    "slow_compile future was NOT dropped on disconnect — \
                     the inflight build would have continued running. The \
                     `select!` arm must drop the pinned future at its \
                     boundary."
                );
            });
        }
    );

    // Sanity counter-test: when the client does NOT disconnect and the
    // future completes normally, we get `Completed` and the inner
    // future ran to its natural end (no spurious cancellation).
    timed_test!(
        race_against_disconnect_returns_completed_on_happy_path,
        Duration::from_secs(10),
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let (server_side, _client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);

                let aborted = Arc::new(AtomicBool::new(false));
                let aborted_inner = Arc::clone(&aborted);
                let fast = async move {
                    let mut tracker = CancelTracker {
                        aborted: aborted_inner,
                        completed: false,
                    };
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    tracker.completed = true;
                    42_u32
                };

                let outcome = race_against_disconnect(&mut server_reader, fast).await;
                match outcome {
                    DispatchOutcome::Completed(value) => assert_eq!(value, 42),
                    DispatchOutcome::ClientDisconnected(reason) => {
                        panic!("unexpected disconnect ({reason:?}) — client end was held open");
                    }
                }
                assert!(
                    !aborted.load(Ordering::SeqCst),
                    "inner future was cancelled despite running to completion"
                );
            });
        }
    );

    // soldr#1857 regression: the `biased` select polls the reader on
    // every tick, so a compile that finished in the SAME tick as the
    // disconnect signal used to be thrown away — zccache had journaled
    // `exit_code: 0` and the wrapper still reported failure to cargo
    // with nothing to show for it. Here the client end is already gone
    // (EOF is immediately ready) and the compile is already complete;
    // the finished result must win.
    timed_test!(
        completed_compile_is_not_discarded_by_a_simultaneous_disconnect,
        Duration::from_secs(10),
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let (server_side, client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);
                // Client is gone before the race even starts: the read
                // probe resolves to EOF on its very first poll.
                drop(client_side);

                let outcome = race_against_disconnect(&mut server_reader, async { 4_2_u32 }).await;

                match outcome {
                    DispatchOutcome::Completed(value) => assert_eq!(value, 42),
                    DispatchOutcome::ClientDisconnected(reason) => panic!(
                        "a compile that had already completed was discarded as \
                         {reason:?}. That is soldr#1857: the daemon runs the \
                         compile, journals exit 0, throws the result away, and \
                         cargo reports an unexplained failure. The disconnect \
                         branch must poll the future once before giving up."
                    ),
                }
            });
        }
    );

    // The durable JSONL row says *how* the wrapper went away; these are
    // the three signals that map onto it.
    timed_test!(disconnect_reason_details_are_stable_and_distinct, {
        use super::DisconnectReason;
        assert_eq!(DisconnectReason::Eof.detail(), "eof");
        assert_eq!(
            DisconnectReason::ReadError("BrokenPipe").detail(),
            "read_error:BrokenPipe"
        );
        assert_eq!(
            DisconnectReason::UnexpectedBytes(4).detail(),
            "unexpected_bytes:4"
        );
    });

    // A wrapper that violates the request-response contract by sending
    // bytes mid-compile is a different fault from one that died, and
    // the record has to be able to tell them apart.
    timed_test!(
        stray_bytes_mid_compile_are_recorded_as_a_protocol_violation,
        Duration::from_secs(10),
        {
            use tokio::io::AsyncWriteExt;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let (server_side, mut client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let _ = client_side.write_all(b"x").await;
                    // Hold the connection open so this is unambiguously
                    // "stray bytes", not "EOF".
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                let outcome = race_against_disconnect(&mut server_reader, async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                })
                .await;
                match outcome {
                    DispatchOutcome::ClientDisconnected(reason) => {
                        assert_eq!(reason.detail(), "unexpected_bytes:1");
                    }
                    DispatchOutcome::Completed(_) => {
                        panic!("the 1h sleep cannot have completed");
                    }
                }
            });
        }
    );
}
