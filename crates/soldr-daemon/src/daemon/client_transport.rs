/// Submit one daemon request with an explicit reply timeout.
pub fn submit_request_with_timeout(
    sock_path: &Path,
    req: &Request,
    timeout: Duration,
) -> Result<Response, ClientError> {
    if let Some(mut stream) = connect_through_override(sock_path, timeout)? {
        write_frame_sync(&mut stream, req)?;
        return read_frame_sync(&mut stream).map_err(ClientError::from);
    }
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        submit_request_windows_with_timeout(sock_path, req, timeout)
    } else {
        let mut stream = connect(sock_path, timeout)?;
        write_frame_sync(&mut stream, req)?;
        let resp: Response = read_frame_sync(&mut stream)?;
        Ok(resp)
    }
}

/// Note a hot-path submission whose receipt ack never arrived.
///
/// soldr#2785 asks which of two things happened when a target-registry row is
/// missing: the touch was never delivered, or it was delivered and the write
/// half dropped it. The daemon already answers the second -- it prints
/// `target-touch dropped` or `target-touch upsert failed` to its own stderr,
/// and `cli_gc` reads those lines back on failure. The first had no signal at
/// all: the ack exists to prove receipt, and both platforms discarded its
/// outcome, so "never acked" and "acked then lost the row" were identical
/// silence. A failing msvc run showed exactly that -- no drop, no upsert
/// failure, and no way to tell whether the frame ever arrived.
///
/// Still best-effort. A missing ack must not fail the call: an older daemon
/// that never acks is still delivering the touch everywhere it always did,
/// which is why soldr#2558 made the wait bounded rather than required. It
/// only stops being invisible.
fn note_missing_ack(req: &Request, reason: &str) {
    // Named rather than `{req:?}`: the Debug of a touch carries the full
    // target path, which is the one thing a reader already knows and the one
    // thing that makes the line long.
    eprintln!("{}", missing_ack_message(req, reason));
}

/// The line [`note_missing_ack`] prints.
///
/// Split out so it can be asserted on. The emitter cannot be: it runs in the
/// wrapper process, and a test that captured stderr in-process would be
/// testing the harness rather than the message.
pub(crate) fn missing_ack_message(req: &Request, reason: &str) -> String {
    // Named rather than `{req:?}`: the Debug of a touch carries the full
    // target path, which is the one thing a reader already knows and the one
    // thing that makes the line long.
    let request = match req {
        Request::RecordTargetTouch { .. } => "RecordTargetTouch",
        Request::CookTouch { .. } => "CookTouch",
        _ => "other",
    };
    format!(
        concat!(
            "soldr: daemon did not acknowledge receipt of {request} within the ",
            "bounded wait ({reason}); delivery is unconfirmed (soldr#2785)"
        ),
        request = request,
        reason = reason,
    )
}

/// Wrapper-side target touch. State is daemon-owned: an unavailable daemon
/// leaves the touch unrecorded rather than opening `state.sqlite3` in this process.
pub fn record_target_touch_or_fallback(paths: &SoldrPaths, target: &Path) {
    let unix_seconds = match current_unix_seconds() {
        Ok(s) => s,
        Err(_) => return,
    };
    let sock = default_sock_path(paths);
    let req = Request::RecordTargetTouch {
        path: target.display().to_string(),
        unix_seconds,
    };
    if let Err(error) = submit_fire_and_forget(&sock, &req) {
        tracing::warn!(
            event = "target_touch_daemon_unavailable",
            target = %target.display(),
            error = ?error,
            "target-registry touch was skipped because soldr-daemon is unavailable"
        );
    }
}

fn connect(sock_path: &Path, timeout: Duration) -> Result<UnixOrPipe, ClientError> {
    // AF_UNIX socket with the caller's deadline as the write timeout and a
    // read timeout of at least 200ms so a short reply deadline never starves
    // a frame read (see platform::ipc::connect::connect_unix).
    let stream = crate::platform::ipc::connect::connect_unix(sock_path, timeout, timeout)?;
    Ok(UnixOrPipe(stream))
}

pub struct UnixOrPipe(crate::platform::ipc::connect::BoxedSyncStream);

impl std::io::Read for UnixOrPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl std::io::Write for UnixOrPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn windows_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
}

async fn open_compile_pipe_with_backpressure(
    sock_path: &Path,
    req: &CompileRequest,
    reply_timeout: Duration,
) -> std::io::Result<(crate::platform::ipc::connect::BoxedAsyncStream, Response)> {
    for attempt in 0..BACKPRESSURE_RETRY_LIMIT {
        let opened = crate::platform::ipc::connect::open_pipe_with_retry(sock_path).await?;
        let mut stream = opened.stream;
        let mut compile_req = req.clone();
        compile_req.ipc_busy_retries = compile_req
            .ipc_busy_retries
            .saturating_add(opened.busy_retries);
        let first = tokio::time::timeout(reply_timeout, async {
            write_frame_async(&mut stream, &Request::Compile(compile_req)).await?;
            read_frame_async(&mut stream).await
        })
        .await
        .map_err(|_| {
            crate::platform::ipc::connect::pipe_timeout_error(
                "daemon IPC compile admission",
                reply_timeout,
            )
        })??;
        match first {
            Response::Backpressure { retry_after_ms } if attempt + 1 < BACKPRESSURE_RETRY_LIMIT => {
                let jitter_ms = (u64::from(attempt) * 11 + u64::from(std::process::id())) % 4;
                tokio::time::sleep(Duration::from_millis(u64::from(retry_after_ms) + jitter_ms))
                    .await;
            }
            response => return Ok((stream, response)),
        }
    }
    unreachable!("backpressure loop returns on the final response")
}

fn run_windows_ipc<T, F>(operation: &'static str, timeout: Duration, f: F) -> Result<T, ClientError>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    crate::platform::ipc::connect::run_in_pipe_worker(operation, timeout, f)
        .map_err(ClientError::from)
}

/// The bound on waiting for the daemon's receipt ack after a hot-path
/// write (soldr#2558). Named pipes do not share the macOS pre-accept
/// drop, but acking uniformly keeps the transports' contracts identical
/// and the wait is sub-ms against a healthy daemon (the ack precedes the
/// store write).
const HOT_PATH_ACK_TIMEOUT: Duration = Duration::from_millis(200);

fn submit_fire_and_forget_windows(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc(
        "daemon IPC hot-path write",
        HOT_PATH_TIMEOUT + HOT_PATH_ACK_TIMEOUT,
        move || {
            let runtime = windows_runtime()?;
            runtime.block_on(async move {
                let mut stream = crate::platform::ipc::connect::open_pipe_with_retry(&sock_path)
                    .await?
                    .stream;
                timeout(HOT_PATH_TIMEOUT, write_frame_async(&mut stream, &req))
                    .await
                    .map_err(|_| {
                        crate::platform::ipc::connect::pipe_timeout_error(
                            "daemon IPC hot-path write",
                            HOT_PATH_TIMEOUT,
                        )
                    })??;
                // Best-effort receipt ack (soldr#2558); an old daemon that
                // never acks costs only this bounded wait.
                //
                // soldr#2785: still best-effort, but no longer silent. The ack
                // is what proves the frame arrived, so discarding its outcome
                // made "never delivered" indistinguishable from "delivered and
                // the write half lost it" -- the two answers that issue is
                // trying to separate.
                match timeout(
                    HOT_PATH_ACK_TIMEOUT,
                    read_frame_async::<_, Response>(&mut stream),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        note_missing_ack(&req, &format!("{error}"));
                    }
                    Err(_) => note_missing_ack(
                        &req,
                        &format!("no ack within {HOT_PATH_ACK_TIMEOUT:?}"),
                    ),
                }
                Ok::<(), std::io::Error>(())
            })
        },
    )
}

fn submit_request_windows(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    submit_request_windows_with_timeout(sock_path, req, REPLY_TIMEOUT)
}

fn submit_request_windows_with_timeout(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
) -> Result<Response, ClientError> {
    submit_request_windows_with_timeout_and_version(
        sock_path,
        req,
        deadline,
        crate::daemon::protocol::PROTOCOL_VERSION,
    )
}

fn submit_request_windows_with_timeout_and_version(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC request", deadline, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = crate::platform::ipc::connect::open_pipe_with_retry(&sock_path)
                .await?
                .stream;
            timeout(deadline, async {
                write_frame_async_for_version(&mut stream, &req, protocol_version).await?;
                read_frame_async_for_version(&mut stream, protocol_version).await
            })
            .await
            .map_err(|_| {
                crate::platform::ipc::connect::pipe_timeout_error("daemon IPC request", deadline)
            })?
        })
    })
}

/// Windows variant of [`compile_streaming`]. Tunnels the chunks back
/// from a tokio runtime thread to the calling thread via a single
/// std::sync::mpsc channel; the calling thread drains the channel and
/// forwards bytes to the caller's `stdout` / `stderr` sinks (which
/// usually are `std::io::stdout()` and `std::io::stderr()` from the
/// wrapper). Keeping the user writers off the tokio thread sidesteps
/// Windows's blocking-IO-on-stdout quirks and matches the sync shape
/// the Unix branch uses.
fn compile_streaming_windows<O, E>(
    sock_path: &Path,
    req: CompileRequest,
    stdout: &mut O,
    stderr: &mut E,
    reply_timeout: Duration,
) -> Result<CompileDoneInfo, ClientError>
where
    O: Write,
    E: Write,
{
    use tokio::time::timeout;

    /// Frames forwarded from the IPC worker thread to the calling
    /// thread. `Done` carries the terminal metadata; `Err` short-
    /// circuits on protocol/io failure.
    enum StreamMsg {
        Stdout(Vec<u8>),
        Stderr(Vec<u8>),
        Done(CompileDoneInfo),
        Err(ClientError),
    }

    let sock_path = sock_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<StreamMsg>();

    let worker = std::thread::Builder::new()
        .name("soldr-daemon-client-stream".into())
        .spawn(move || {
            let runtime = match windows_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                    return;
                }
            };
            runtime.block_on(async move {
                let (mut stream, first_frame) = match open_compile_pipe_with_backpressure(
                    &sock_path, &req, reply_timeout,
                )
                .await
                {
                    Ok(connection) => connection,
                    Err(e) => {
                        let _ = tx.send(StreamMsg::Err(ClientError::from(e)));
                        return;
                    }
                };
                let mut first_frame = Some(first_frame);
                loop {
                    let frame = match first_frame.take() {
                        Some(frame) => frame,
                        None => match timeout(
                            reply_timeout,
                            read_frame_async::<_, Response>(&mut stream),
                        )
                        .await
                        {
                            Ok(Ok(f)) => f,
                            Ok(Err(e)) => {
                                let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                                return;
                            }
                            Err(_) => {
                                let _ = tx.send(StreamMsg::Err(ClientError::Io(
                                    crate::platform::ipc::connect::pipe_timeout_error(
                                        "daemon IPC compile read",
                                        reply_timeout,
                                    ),
                                )));
                                return;
                            }
                        },
                    };
                    match frame {
                        Response::CompileStdoutChunk(bytes) => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                bytes = bytes.len(),
                                "stdout chunk received",
                            );
                            if tx.send(StreamMsg::Stdout(bytes)).is_err() {
                                return;
                            }
                        }
                        Response::CompileStderrChunk(bytes) => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                bytes = bytes.len(),
                                "stderr chunk received",
                            );
                            if tx.send(StreamMsg::Stderr(bytes)).is_err() {
                                return;
                            }
                        }
                        Response::CompileDone {
                            exit_code,
                            cached,
                            cache_outcome,
                            compile_id,
                        } => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                exit_code,
                                cached,
                                cache_outcome,
                                "compile done — streaming reply complete",
                            );
                            let _ = tx.send(StreamMsg::Done(CompileDoneInfo {
                                exit_code,
                                cached,
                                cache_outcome,
                                compile_id,
                            }));
                            return;
                        }
                        Response::Error(msg) => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(msg)));
                            return;
                        }
                        // soldr#1838 Phase 2. This is the transport #1837 was
                        // about: a wrapper connecting during the Windows
                        // graceful drain reached a latched-shut compile
                        // service. #1837 narrowed that window by releasing the
                        // pipe instance early; this handles a request that
                        // still lands inside it.
                        Response::Retiring => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Retiring));
                            return;
                        }
                        Response::Backpressure { retry_after_ms } => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(format!(
                                "daemon IPC admission remained backpressured after retry ({retry_after_ms}ms)"
                            ))));
                            return;
                        }
                        other => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(format!(
                                "unexpected compile stream frame: {other:?}"
                            ))));
                            return;
                        }
                    }
                }
            });
        })
        .map_err(ClientError::Io)?;

    // soldr#1838 bullet 4 — mirrors the unix arm. This consumer is the one
    // place on the Windows transport that sees both the chunks and the
    // worker's terminal error, so the slow-vs-wedged distinction is made
    // here rather than inside the worker thread.
    let started = std::time::Instant::now();
    let mut saw_output = false;
    // soldr#1838 Phase 1: the streaming phase used to run silent -- the
    // heartbeat on the request wait only covers the reply handshake. Publish
    // chunk arrivals so each beat can say whether output is still coming.
    let progress = super::wait_heartbeat::StreamProgress::new();
    let _stream_heartbeat = super::wait_heartbeat::WaitHeartbeat::start_streaming(
        "daemon compile stream",
        reply_timeout,
        Some(REPLY_TIMEOUT_ENV),
        std::sync::Arc::clone(&progress),
    );
    let result = loop {
        match rx.recv() {
            Ok(StreamMsg::Stdout(bytes)) => {
                saw_output = true;
                progress.record_chunk();
                stdout.write_all(&bytes).map_err(ClientError::Io)?;
            }
            Ok(StreamMsg::Stderr(bytes)) => {
                saw_output = true;
                progress.record_chunk();
                stderr.write_all(&bytes).map_err(ClientError::Io)?;
            }
            Ok(StreamMsg::Done(info)) => break Ok(info),
            Ok(StreamMsg::Err(ClientError::Io(err))) if is_deadline_error(&err) => {
                break Err(ClientError::CompileStalled {
                    saw_output,
                    elapsed: started.elapsed(),
                })
            }
            Ok(StreamMsg::Err(e)) => break Err(e),
            Err(_) => {
                break Err(ClientError::Io(std::io::Error::other(
                    "soldr-daemon-client-stream worker exited without a result",
                )))
            }
        }
    };
    // Best effort join — the worker has already pushed its final
    // message at this point, so this returns promptly.
    let _ = worker.join();
    result
}

/// Returns the well-known socket path the wrapper should use. Centralized
/// here so callers don't need to import `cache_lib` directly.
pub fn default_sock_path(paths: &SoldrPaths) -> PathBuf {
    #[cfg(debug_assertions)]
    if std::env::var_os(TEST_DIRECT_CONTROL_ENV).is_some() {
        return crate::daemon::session_endpoint::resolved_control_endpoint_path(paths)
            .unwrap_or_else(|_| PathBuf::from("<missing-daemon-control-endpoint>"));
    }
    if CONTROL_CONNECTOR.get().is_some() {
        // Opaque marker only; the installed connector ignores it. Returning a
        // marker instead of deriving the private path enforces the #2476
        // boundary that user-facing clients neither learn nor dial a daemon
        // endpoint.
        return PathBuf::from("<broker-routed-daemon-control>");
    }
    crate::daemon::session_endpoint::resolved_control_endpoint_path(paths)
        .unwrap_or_else(|_| PathBuf::from("<missing-daemon-control-endpoint>"))
}
