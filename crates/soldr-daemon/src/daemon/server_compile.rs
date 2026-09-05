/// Dispatch a compile request as bounded output frames followed by `CompileDone`.
async fn dispatch_compile_streaming<S>(
    state: &Arc<State>,
    req: crate::daemon::protocol::CompileRequest,
    stream: &mut S,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Per-compile id for the JSONL phase trace (soldr#981). Cheap —
    // monotonic counter, only meaningful within a single daemon
    // lifetime, which is exactly the scope we need for offline
    // post-cold-build analysis.
    let compile_id = next_compile_id();

    let total = std::time::Instant::now();

    // soldr#1537: lifecycle telemetry uses this already-open compile
    // connection. A disconnected wrapper intentionally leaves a start-only
    // event, preserving the cancellation signal used by build history.
    let lifecycle = req.lifecycle.clone();
    let inner_started = std::time::Instant::now();
    // Keep zccache's compile future behind one heap indirection before it
    // enters the nested lifecycle/disconnect select chain. Staged-output
    // support substantially increased that future's concrete size; carrying
    // it inline through both generic async helpers exhausted Tokio's 2 MiB
    // worker stack under a parallel Cargo cold build.
    let compile_fut = Box::pin(state.compile_service.compile(req));
    let body = match crate::daemon::disconnect::race_compile_with_lifecycle(
        stream,
        compile_fut,
        lifecycle.as_ref(),
        &state.event_batcher,
    )
    .await
    {
        DispatchOutcome::Completed(result) => match result {
            Ok(body) => body,
            Err(err) => {
                crate::daemon::compile_trace::record(
                    "inner_compile_err",
                    inner_started.elapsed().as_micros() as u64,
                    &compile_id,
                );
                // During graceful shutdown, preserve the explicit Retiring reply so
                // the mandatory SESSION client can attribute the infrastructure failure
                // correctly. Other embedded-service errors remain protocol failures.
                let reply = if state.shutdown.is_requested() {
                    tracing::info!(
                        target: "soldr::daemon::compile_stream",
                        compile_id = compile_id.as_str(),
                        "compile arrived during shutdown; answering Retiring to the mandatory SESSION client",
                    );
                    Response::Retiring
                } else {
                    Response::Error(format!("embedded zccache compile failed: {err}"))
                };
                return write_frame_async(stream, &reply).await;
            }
        },
        DispatchOutcome::ClientDisconnected(reason) => {
            // The wrapper is gone — its IPC fd closed mid-compile, so
            // the embedded zccache future was dropped at the `select!`
            // boundary above. Don't attempt to write a reply (the pipe
            // is dead) and don't burn CPU finishing a rustc whose
            // output no one will read. Record the disconnect for the
            // per-compile trace so postmortems can correlate.
            crate::daemon::compile_trace::record(
                "client_disconnect_cancelled",
                inner_started.elapsed().as_micros() as u64,
                &compile_id,
            );
            // soldr#1857: the trace above is inert unless
            // SOLDR_DAEMON_TRACE is set, i.e. never in a real build.
            // This one is always on, so "the wrapper vanished mid-
            // compile" is countable after the fact instead of being a
            // hypothesis nobody can test.
            crate::daemon::disconnect::record_undelivered(
                &state.paths,
                &compile_id,
                lifecycle.as_ref(),
                inner_started,
                crate::daemon::compile_delivery::UndeliveredKind::ClientDisconnected,
                &reason.detail(),
                None,
            );
            tracing::info!(
                target: "soldr::daemon::compile_stream",
                compile_id = compile_id.as_str(),
                reason = reason.detail().as_str(),
                "client disconnected during compile — aborting in-flight work",
            );
            return Ok(());
        }
    };
    crate::daemon::compile_trace::record(
        "inner_compile",
        inner_started.elapsed().as_micros() as u64,
        &compile_id,
    );

    // Stream the captured output through a transport sink (soldr#2388 Step 5/6):
    // the legacy DaemonRequest wire here (byte-identical, `phase5_contract`), the
    // SESSION `0x5350` wire elsewhere, over one embedded-zccache execution.
    let mut sink = crate::daemon::compile_sink::LegacyDaemonSink { stream };
    stream_compile_output(
        &mut sink,
        &body,
        &state.paths,
        &compile_id,
        lifecycle.as_ref(),
        inner_started,
        total,
    )
    .await
}

/// Stream a completed compile's captured output through `sink` — the legacy
/// `Response` wire (`LegacyDaemonSink`) or the SESSION `0x5350` wire
/// (`session_sink::SessionCompileSink`), over one embedded-zccache execution
/// (soldr#2388 Step 5/6). Shared so both wires stay byte-transparent and the
/// disconnect error-attribution + per-compile telemetry are identical.
pub(crate) async fn stream_compile_output<Sink>(
    sink: &mut Sink,
    body: &crate::daemon::protocol::CompileResponseBody,
    paths: &crate::core::SoldrPaths,
    compile_id: &str,
    lifecycle: Option<&crate::daemon::protocol::CompileLifecycle>,
    inner_started: std::time::Instant,
    total: std::time::Instant,
) -> std::io::Result<()>
where
    Sink: crate::daemon::compile_sink::CompileOutputSink,
{
    let stdout_len = body.stdout.len();
    let stderr_len = body.stderr.len();
    let mut stdout_chunks = 0usize;
    let mut stderr_chunks = 0usize;

    let wire_stdout_started = std::time::Instant::now();
    for chunk in body.stdout.chunks(CHUNK_BYTES) {
        if let Err(err) = sink.emit_stdout_chunk(chunk).await {
            return Err(crate::daemon::disconnect::report_reply_write_failure(
                paths,
                compile_id,
                lifecycle,
                inner_started,
                "stdout_chunk",
                err,
                body.exit_code,
            ));
        }
        stdout_chunks += 1;
        tracing::debug!(
            target: "soldr::daemon::compile_stream",
            bytes = chunk.len(),
            chunk_index = stdout_chunks - 1,
            "stdout chunk emitted",
        );
    }
    crate::daemon::compile_trace::record(
        "wire_stdout",
        wire_stdout_started.elapsed().as_micros() as u64,
        compile_id,
    );

    let wire_stderr_started = std::time::Instant::now();
    for chunk in body.stderr.chunks(CHUNK_BYTES) {
        if let Err(err) = sink.emit_stderr_chunk(chunk).await {
            return Err(crate::daemon::disconnect::report_reply_write_failure(
                paths,
                compile_id,
                lifecycle,
                inner_started,
                "stderr_chunk",
                err,
                body.exit_code,
            ));
        }
        stderr_chunks += 1;
        tracing::debug!(
            target: "soldr::daemon::compile_stream",
            bytes = chunk.len(),
            chunk_index = stderr_chunks - 1,
            "stderr chunk emitted",
        );
    }
    crate::daemon::compile_trace::record(
        "wire_stderr",
        wire_stderr_started.elapsed().as_micros() as u64,
        compile_id,
    );

    tracing::debug!(
        target: "soldr::daemon::compile_stream",
        exit_code = body.exit_code,
        cached = body.cached,
        cache_outcome = body.cache_outcome,
        stdout_bytes = stdout_len,
        stderr_bytes = stderr_len,
        stdout_chunks,
        stderr_chunks,
        "compile done — streaming reply complete",
    );
    let wire_done_started = std::time::Instant::now();
    let res = sink
        .emit_done(body.exit_code, body.cached, body.cache_outcome, compile_id)
        .await
        .map_err(|err| {
            crate::daemon::disconnect::report_reply_write_failure(
                paths,
                compile_id,
                lifecycle,
                inner_started,
                "done",
                err,
                body.exit_code,
            )
        });
    crate::daemon::compile_trace::record(
        "wire_done",
        wire_done_started.elapsed().as_micros() as u64,
        compile_id,
    );
    crate::daemon::compile_trace::record(
        "total_dispatch",
        total.elapsed().as_micros() as u64,
        compile_id,
    );
    // Co-record per-compile output bytes for cross-axis analysis.
    crate::daemon::compile_trace::record("stdout_bytes", stdout_len as u64, compile_id);
    crate::daemon::compile_trace::record("stderr_bytes", stderr_len as u64, compile_id);
    res
}

/// Monotonic per-daemon compile counter. The id is stable within one
/// daemon process and meaningless across restarts — exactly the scope
/// the `SOLDR_DAEMON_TRACE` JSONL is designed for.
pub(crate) fn next_compile_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AOrdering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, AOrdering::Relaxed);
    format!("c{n:08x}")
}

/// Best-effort resolver for the `~/.soldr/cache/cook/<sha256>.tar.zst`
/// path. Returns the canonical content-addressed file even when the
/// file does not yet exist on disk — the path is informational only;
/// PR 3 (`Response::CookHit` consumer) is responsible for verifying
/// the sha256 of the bytes it reads.
fn cook_artifact_path(paths: &SoldrPaths, sha256: &[u8; 32]) -> PathBuf {
    paths
        .cache
        .join("cook")
        .join(format!("{}.tar.zst", hex_lower(sha256)))
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

async fn run_idle_watchdog(state: Arc<State>, idle_timeout: Duration) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if state.idle_for() >= idle_timeout {
            // Tag the exit reason BEFORE notifying so the main task's
            // post-shutdown lifecycle JSONL emit picks `died-idle`.
            state.exit_via_idle.store(true, Ordering::Relaxed);
            state.shutdown.request();
            return;
        }
    }
}

/// Exit as soon as the process that asked for this daemon is gone.
///
/// Separate from the idle watchdog on purpose: an owned daemon may be busy
/// right up to the moment its owner dies, so idleness is the wrong signal.
/// It shares `IDLE_POLL_INTERVAL` because both are coarse liveness polls and
/// a second cadence would only add a knob.
async fn run_owner_watchdog(state: Arc<State>, owner_pid: u32) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if state.shutdown.is_requested() {
            return;
        }
        if super::server::owner_has_exited(Some(owner_pid)) {
            // Tagged as an idle exit so the lifecycle JSONL records a chosen
            // shutdown rather than a crash. The two are the same class of
            // event: the daemon decided it had no further reason to run.
            state.exit_via_idle.store(true, Ordering::Relaxed);
            state.shutdown.request();
            return;
        }
    }
}

/// Used by the `soldr daemon` CLI to derive sockets and paths in one
/// place. Mirrors [`crate::daemon::client::default_sock_path`].
pub fn server_sock_path(paths: &SoldrPaths) -> PathBuf {
    crate::daemon::client::default_sock_path(paths)
}
