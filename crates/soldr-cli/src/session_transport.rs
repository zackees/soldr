//! SESSION `0x5350` client wiring for Soldr's stable singleton broker.
//!
//! The compiler wrapper dials the one broker endpoint, sends the standard v2
//! Hello, then keeps that accepted connection for the SESSION wire. The broker
//! either hands the connection directly to the daemon or proxies that same
//! connection when handle passing is unavailable.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::tokio::Stream;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use running_process::broker::protocol::{
    encode_framed, hello_reply::Result as HelloReplyResult, try_decode_framed, FrameKind,
    HandoffAck, Hello, PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION,
};
use running_process::broker::protocol::{Frame, HelloReply};
use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionFrame, SessionStart,
};
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};

static NEXT_BROKER_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn broker_hello_frame(payload: Vec<u8>) -> Frame {
    let request_id = NEXT_BROKER_REQUEST_ID
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    Frame::request(CONTROL_PAYLOAD_PROTOCOL, payload).with_request_id(request_id)
}

/// Outcome of the SESSION compile hot path ([`session_hot_path`]), consumed by
/// `compile_dispatch`. All bounded retry and error attribution lives here.
pub enum SessionHotPathOutcome {
    /// SESSION served the compile; return this exit code.
    Served(i32),
    /// SESSION infrastructure failed; cacheable compiles have no alternate route.
    HardFail(io::Error),
}

/// Backend identity attested by the broker's successful route negotiation.
#[derive(Debug)]
pub(crate) struct ReadyRoute {
    pub(crate) backend_pipe: String,
    pub(crate) daemon_version: String,
}

/// Broker-unreachable errors return immediately; an existing broker gets the
/// bounded `BrokerDeadlines::route_ceiling` window to launch or reconnect the
/// requested daemon partition. Its default is 120 seconds and it is configured
/// by `SOLDR_ROUTE_ACQUIRE_CEILING_MS`.
/// Mandatory SESSION compile hot path: relay client → broker → daemon.
/// A missing broker fails immediately; an existing broker gets the bounded
/// route-acquisition ceiling to provide the requested route. Every terminal
/// infrastructure error is hard because there is no legacy acquisition path.
pub fn session_hot_path(rustc_argv: &[String]) -> SessionHotPathOutcome {
    let service_name = match crate::daemon::backend_handle_adoption::broker_service_name() {
        Ok(service_name) => service_name,
        Err(err) => {
            return SessionHotPathOutcome::HardFail(io::Error::other(format!(
                "cannot resolve the broker daemon route: {err}; invoke this compiler through a soldr build front door"
            )))
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => return SessionHotPathOutcome::HardFail(err),
    };
    let cwd = cwd.display().to_string();
    let env: Vec<SessionEnvVar> = std::env::vars()
        .map(|(key, value)| SessionEnvVar { key, value })
        .collect();

    match run_session_compile_for_service(&service_name, rustc_argv, cwd, env) {
        Ok(outcome) => {
            if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
                eprintln!(
                    "soldr: SESSION compile served (cache_outcome={:?})",
                    outcome.cache_outcome
                );
            }
            SessionHotPathOutcome::Served(outcome.exit_code)
        }
        Err(err) if err.broker_unreachable => SessionHotPathOutcome::HardFail(io::Error::other(
            format!(
                "soldr broker is unreachable: {err}; invoke this compiler through `soldr cargo ...` (or another soldr build front door) so the singleton broker is resurrected"
            ),
        )),
        Err(err) => SessionHotPathOutcome::HardFail(err.source),
    }
}

/// Ask the stable broker to materialize one route without starting a SESSION.
/// Used by the explicit `soldr daemon start` front door after broker
/// resurrection; dropping immediately after `Negotiated` is intentional.
pub(crate) fn ensure_broker_route(
    service_name: &str,
    timeout: std::time::Duration,
) -> io::Result<ReadyRoute> {
    let service_name = service_name.to_string();
    std::thread::scope(|scope| {
        scope
            .spawn(move || ensure_broker_route_on_runtime(&service_name, timeout))
            .join()
            .map_err(|_| io::Error::other("broker route worker panicked"))?
    })
}

fn ensure_broker_route_on_runtime(service_name: &str, timeout: Duration) -> io::Result<ReadyRoute> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(timeout, async {
            let endpoint = session_socket_path()?;
            let name = local_session_name(&endpoint)?;
            let mut stream = connect_broker_with_busy_retry(name).await?;
            let hello = Hello {
                client_min_protocol: ENVELOPE_VERSION as u32,
                client_max_protocol: ENVELOPE_VERSION as u32,
                service_name: service_name.to_string(),
                wanted_version:
                    crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION
                        .to_string(),
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                request_id: format!("soldr-route-{}", std::process::id()),
                peer_pid: std::process::id(),
                client_lib_name: "soldr".into(),
                client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
                peer_attestation_nonce: crate::broker_server::client_host_attestation(),
                // Route-only probes never need to transfer their connection.
                client_capabilities: 0,
                ..Default::default()
            };
            let frame = broker_hello_frame(hello.encode_to_vec());
            let request_id = frame.request_id;
            let bytes = encode_framed(&frame).map_err(io::Error::other)?;
            stream.write_all(&bytes).await?;
            stream.flush().await?;
            read_negotiated(&mut stream, request_id).await
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "broker route start timed out"))?
    })
}

/// Resolve the one stable broker endpoint.
pub(crate) fn session_socket_path() -> io::Result<String> {
    crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map(|endpoint| endpoint.bind_endpoint)
        .map_err(io::Error::other)
}

pub(crate) fn local_session_name(
    socket_path: &str,
) -> io::Result<interprocess::local_socket::Name<'_>> {
    running_process::broker::server::singleton_bind::wrap_socket_name(socket_path)
        .map_err(io::Error::other)
}

/// A SESSION compile failure, tagged with whether any compiler output was
/// already emitted locally.
///
/// `output_started` preserves diagnostic attribution: a failure after compiler
/// output began may have emitted a partial diagnostic, while a setup failure
/// did not. Both are hard failures on the mandatory SESSION route.
#[derive(Debug)]
pub struct SessionError {
    /// Whether compiler stdout/stderr was written locally before the failure.
    pub output_started: bool,
    /// Whether the broker's SESSION socket was unreachable (dial refused /
    /// absent) — i.e. there is no broker to talk to, as opposed to a broker
    /// that answered but whose daemon relay is not ready yet. This fails
    /// immediately instead of burning the route-start budget.
    pub broker_unreachable: bool,
    /// The underlying transport / protocol error.
    pub source: io::Error,
}

impl SessionError {
    pub(crate) fn pre_output(source: io::Error) -> Self {
        Self {
            output_started: false,
            broker_unreachable: false,
            source,
        }
    }

    /// The broker's SESSION socket could not be dialed — no broker is serving.
    pub(crate) fn broker_unreachable(source: io::Error) -> Self {
        Self {
            output_started: false,
            broker_unreachable: true,
            source,
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SESSION compile failed (output_started={}): {}",
            self.output_started, self.source
        )
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The result of a SESSION compile: the compiler exit code plus the daemon's
/// `cache_outcome` (`CacheOutcome` discriminant: 1=Hit, 2=Miss, 3=Error) carried
/// on the terminal `Exit` frame's metadata — `None` if the daemon did not report
/// one (e.g. an infra exit).
#[derive(Debug, Clone)]
struct SessionCompileOutcome {
    /// The compiler's exit code.
    pub exit_code: i32,
    /// The daemon's cache outcome discriminant, if reported.
    pub cache_outcome: Option<i32>,
}

fn run_session_compile_for_service(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    let run = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(SessionError::pre_output)?
            .block_on(run_session_compile_async(
                service_name,
                rustc_argv,
                cwd,
                env,
            ))
    };

    // A Cargo wrapper arrives before Soldr constructs its command runtime, but
    // direct compiler commands are dispatched from that runtime. Tokio rejects
    // nested `block_on` calls there. Move only that direct-command bridge to a
    // short-lived ordinary thread, leaving the hot wrapper path unchanged.
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            scope.spawn(run).join().unwrap_or_else(|_| {
                Err(SessionError::pre_output(std::io::Error::other(
                    "SESSION compile worker panicked",
                )))
            })
        })
    } else {
        run()
    }
}

async fn run_session_compile_async(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    let setup_timeout = crate::broker_deadlines::BrokerDeadlines::from_env().route_ceiling;
    let mut stream = match tokio::time::timeout(
        setup_timeout,
        establish_session(service_name, rustc_argv, cwd, env),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(SessionError::pre_output(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "SESSION setup did not complete within {}ms",
                    setup_timeout.as_millis()
                ),
            )))
        }
    };

    pump_session_output_with_timeout(&mut stream, crate::daemon::client::compile_reply_timeout())
        .await
}

async fn establish_session(
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<Stream, SessionError> {
    // Setup — connect / Hello / negotiate / SessionStart send. Failures here
    // are tagged pre-output for precise diagnostics.
    let session_socket = session_socket_path().map_err(SessionError::broker_unreachable)?;
    if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
        eprintln!("soldr: SESSION dialing service={service_name} socket={session_socket}");
    }
    let name = local_session_name(&session_socket).map_err(SessionError::broker_unreachable)?;
    // Negotiate the requested daemon route over the v2 control envelope.
    let hello_payload = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: service_name.to_string(),
        wanted_version: crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION
            .to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        request_id: format!("soldr-session-{}", std::process::id()),
        peer_pid: std::process::id(),
        client_lib_name: "soldr".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        peer_attestation_nonce: crate::broker_server::client_host_attestation(),
        client_capabilities: running_process::broker::capabilities::CAP_HANDLE_PASSING,
        ..Default::default()
    }
    .encode_to_vec();
    let hello_frame = broker_hello_frame(hello_payload);
    let request_id = hello_frame.request_id;
    let hello =
        encode_framed(&hello_frame).map_err(|e| SessionError::pre_output(io::Error::other(e)))?;
    // A same-version broker replacement can accept a pipe instance and close
    // it before replying to Hello. Retrying this pre-session handshake is safe:
    // no compiler request or output has crossed the connection yet.
    let mut hello_attempt = 0;
    let mut stream = loop {
        hello_attempt += 1;
        // A failed dial means no broker is bound at this socket.
        let mut candidate = connect_broker_with_busy_retry(name.clone())
            .await
            .map_err(SessionError::broker_unreachable)?;
        candidate
            .write_all(&hello)
            .await
            .map_err(SessionError::pre_output)?;
        candidate.flush().await.map_err(SessionError::pre_output)?;
        match read_negotiated(&mut candidate, request_id).await {
            Ok(_) => break candidate,
            Err(error) if hello_attempt < 3 && broker_hello_retryable(&error) => {
                tokio::time::sleep(busy_jitter()).await;
            }
            Err(error) => return Err(SessionError::pre_output(error)),
        }
    };

    // From here the connection is a transparent SESSION relay to the daemon.
    let start = compile_session_start(rustc_argv, cwd, env);
    let start_frame = encode_session_frame(
        &SessionFrame {
            kind: Some(session_frame::Kind::Start(start)),
        },
        0,
    )
    .map_err(|e| SessionError::pre_output(io::Error::other(e)))?;
    stream
        .write_all(&start_frame)
        .await
        .map_err(SessionError::pre_output)?;
    stream.flush().await.map_err(SessionError::pre_output)?;

    // Output phase — a failure after the first byte is printed is a hard error.
    Ok(stream)
}

fn compile_session_start(
    rustc_argv: &[String],
    cwd: String,
    env: Vec<running_process::broker::protocol_v2::SessionEnvVar>,
) -> SessionStart {
    SessionStart {
        program: rustc_argv.first().cloned().unwrap_or_default(),
        args: rustc_argv.get(1..).unwrap_or_default().to_vec(),
        cwd,
        env,
        clear_inherited_env: true,
        environment_policy: running_process::broker::protocol_v2::EnvironmentPolicy::Clear as i32,
    }
}

async fn connect_broker_with_busy_retry(
    name: interprocess::local_socket::Name<'_>,
) -> io::Result<Stream> {
    let started = tokio::time::Instant::now();
    let deadline = started + crate::broker_deadlines::BrokerDeadlines::from_env().busy_budget;
    loop {
        match crate::platform::ipc::connect::connect_local_socket(name.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if broker_connect_is_busy(&error, started.elapsed())
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(busy_jitter()).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn broker_connect_is_busy(error: &io::Error, elapsed: Duration) -> bool {
    // Windows briefly reports FILE_NOT_FOUND while a busy named-pipe listener
    // replaces one accepted instance with the next. One short retry window
    // absorbs that instance gap without turning a genuinely absent endpoint
    // into the full one-second busy wait.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
        && error.raw_os_error() == Some(2)
    {
        return elapsed < Duration::from_millis(50);
    }
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Interrupted
    ) || matches!(error.raw_os_error(), Some(231) | Some(232) | Some(233))
}

fn broker_hello_retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
    ) || error.to_string() == "broker closed before Hello reply"
}

fn busy_jitter() -> std::time::Duration {
    let mut random = [0_u8; 8];
    let value = if getrandom::fill(&mut random).is_ok() {
        u64::from_le_bytes(random)
    } else {
        0
    };
    std::time::Duration::from_millis(5 + value % 46)
}

/// Read and validate the broker's framed `Negotiated` reply.
async fn read_negotiated<S>(stream: &mut S, expected_request_id: u64) -> io::Result<ReadyRoute>
where
    S: tokio::io::AsyncRead + Unpin,
{
    read_negotiated_with_deadlines(
        stream,
        expected_request_id,
        crate::broker_deadlines::BrokerDeadlines::from_env(),
    )
    .await
}

async fn read_negotiated_with_deadlines<S>(
    stream: &mut S,
    expected_request_id: u64,
    deadlines: crate::broker_deadlines::BrokerDeadlines,
) -> io::Result<ReadyRoute>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use prost::Message as _;
    let started = tokio::time::Instant::now();
    let route_ceiling = started + deadlines.route_ceiling;
    let mut response_deadline = started + deadlines.first_response;
    let mut pending_handoff: Option<(Vec<u8>, u64, ReadyRoute)> = None;
    let mut last_progress_elapsed_ms = 0_u64;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some(decoded) = try_decode_framed(&buf).map_err(io::Error::other)? {
            let consumed = decoded.consumed;
            let frame = decoded.frame;
            buf.drain(..consumed);
            if frame.envelope_version != ENVELOPE_VERSION as u32
                || PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None)
            {
                return Err(io::Error::other(
                    "broker returned an invalid frame envelope",
                ));
            }
            if FrameKind::try_from(frame.kind) == Ok(FrameKind::Event)
                && frame.payload_protocol == crate::broker_server::ROUTE_PROGRESS_PAYLOAD_PROTOCOL
            {
                if frame.request_id != expected_request_id {
                    return Err(io::Error::other("broker progress request id mismatch"));
                }
                let progress =
                    crate::broker_server::RouteProgress::decode(frame.payload.as_slice())
                        .map_err(io::Error::other)?;
                if progress.stage.is_empty()
                    || progress.latest_result.is_empty()
                    || progress.attempt == 0
                    || progress.elapsed_ms < last_progress_elapsed_ms
                {
                    return Err(io::Error::other("broker returned invalid route progress"));
                }
                last_progress_elapsed_ms = progress.elapsed_ms;
                if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
                    eprintln!(
                        "soldr: broker route progress stage={} attempt={} elapsed_ms={} result={}",
                        progress.stage,
                        progress.attempt,
                        progress.elapsed_ms,
                        progress.latest_result
                    );
                }
                response_deadline = tokio::time::Instant::now() + deadlines.progress_silence;
                continue;
            }
            if FrameKind::try_from(frame.kind) == Ok(FrameKind::Event)
                && frame.payload_protocol
                    == running_process::broker::protocol::HANDOFF_PAYLOAD_PROTOCOL
            {
                let Some((expected_token, expected_correlation_id, route)) =
                    pending_handoff.as_ref()
                else {
                    return Err(io::Error::other("unexpected broker handoff-ready event"));
                };
                let ack = HandoffAck::decode(frame.payload.as_slice()).map_err(io::Error::other)?;
                if ack.token != *expected_token || ack.correlation_id != *expected_correlation_id {
                    return Err(io::Error::other(
                        "broker handoff event did not match its negotiation",
                    ));
                }
                return Ok(ReadyRoute {
                    backend_pipe: route.backend_pipe.clone(),
                    daemon_version: route.daemon_version.clone(),
                });
            }
            if FrameKind::try_from(frame.kind) != Ok(FrameKind::Response)
                || frame.payload_protocol != CONTROL_PAYLOAD_PROTOCOL
            {
                return Err(io::Error::other(format!(
                    "unexpected broker frame kind={} protocol={:#x}",
                    frame.kind, frame.payload_protocol
                )));
            }
            if frame.request_id != expected_request_id {
                return Err(io::Error::other("broker Hello reply request id mismatch"));
            }
            let reply = HelloReply::decode(frame.payload.as_slice()).map_err(io::Error::other)?;
            match reply.result {
                Some(HelloReplyResult::Negotiated(negotiated)) => {
                    let route = ReadyRoute {
                        backend_pipe: negotiated.backend_pipe.clone(),
                        daemon_version: negotiated.daemon_version.clone(),
                    };
                    if negotiated.server_capabilities
                        & running_process::broker::capabilities::CAP_HANDLE_PASSING
                        != 0
                        && !negotiated.handle_passed_token.is_empty()
                    {
                        pending_handoff = Some((
                            negotiated.handle_passed_token,
                            negotiated.connection_id,
                            route,
                        ));
                        response_deadline = tokio::time::Instant::now()
                            + running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE
                            + std::time::Duration::from_millis(100);
                        continue;
                    }
                    return Ok(route);
                }
                Some(HelloReplyResult::Refused(refused)) => Err(io::Error::other(format!(
                    "broker refused the daemon route: {} (code={}, retry_after_ms={}, details={:?})",
                    refused.reason, refused.code, refused.retry_after_ms, refused.details
                ))),
                None => Err(io::Error::other("broker returned an empty HelloReply")),
            }?;
        }
        let deadline = if pending_handoff.is_some() {
            response_deadline
        } else {
            response_deadline.min(route_ceiling)
        };
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                if pending_handoff.is_some() {
                    return io::Error::new(
                        io::ErrorKind::TimedOut,
                        "broker did not confirm either handoff ownership or same-connection proxy fallback",
                    );
                }
                let class = if deadline == route_ceiling {
                    "route acquisition ceiling"
                } else if response_deadline == started + deadlines.first_response {
                    "first-response deadline"
                } else {
                    "progress-silence deadline"
                };
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "broker {class} exceeded after {}ms",
                        started.elapsed().as_millis()
                    ),
                )
            });
        let n = n??;
        if n == 0 {
            return Err(io::Error::other("broker closed before Hello reply"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Metadata key on the terminal `Exit` frame carrying the daemon's cache
/// outcome — matches `soldr_daemon::daemon::session_sink::META_CACHE_OUTCOME`.
const META_CACHE_OUTCOME: &str = "cache_outcome";

fn mark_relayed_output(bytes: &[u8]) -> bool {
    let marked = !bytes.is_empty();
    if marked {
        crate::exit_guard::mark_spoke();
    }
    marked
}

async fn pump_session_output_with_timeout<S>(
    stream: &mut S,
    reply_timeout: std::time::Duration,
) -> Result<SessionCompileOutcome, SessionError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let output_started = Arc::new(AtomicBool::new(false));
    match tokio::time::timeout(
        reply_timeout,
        pump_session_output(stream, Arc::clone(&output_started)),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(SessionError {
            output_started: output_started.load(Ordering::Acquire),
            broker_unreachable: false,
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "SESSION compile reply did not complete within {}s; adjust {} only while diagnosing",
                    reply_timeout.as_secs_f64(),
                    crate::daemon::client::REPLY_TIMEOUT_ENV
                ),
            ),
        }),
    }
}

/// Pump SESSION frames from the relay: stdout/stderr to local stdio, returning
/// the compiler exit code + `cache_outcome` on the terminal `Exit` frame.
///
/// `output_started` flips to `true` the moment any stdout/stderr byte is written
/// locally; every error is tagged with it so the caller knows whether a legacy
/// fallback would double-print.
async fn pump_session_output<S>(
    stream: &mut S,
    output_seen: Arc<AtomicBool>,
) -> Result<SessionCompileOutcome, SessionError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut output_started = false;
    let tag = |output_started: bool, e: io::Error| SessionError {
        output_started,
        broker_unreachable: false,
        source: e,
    };
    loop {
        loop {
            match try_decode_session_frame(&buf) {
                Ok(Some(decoded)) => {
                    let consumed = decoded.consumed;
                    let kind = decoded.frame.kind.clone();
                    buf.drain(..consumed);
                    match kind {
                        Some(session_frame::Kind::Stdout(b)) => {
                            output_started = true;
                            output_seen.store(true, Ordering::Release);
                            stdout
                                .write_all(&b)
                                .await
                                .map_err(|e| tag(output_started, e))?;
                            mark_relayed_output(&b);
                            stdout.flush().await.map_err(|e| tag(output_started, e))?;
                        }
                        Some(session_frame::Kind::Stderr(b)) => {
                            output_started = true;
                            output_seen.store(true, Ordering::Release);
                            stderr
                                .write_all(&b)
                                .await
                                .map_err(|e| tag(output_started, e))?;
                            mark_relayed_output(&b);
                            stderr.flush().await.map_err(|e| tag(output_started, e))?;
                        }
                        Some(session_frame::Kind::Exit(exit)) => {
                            let cache_outcome = exit
                                .metadata
                                .get(META_CACHE_OUTCOME)
                                .and_then(|v| v.parse::<i32>().ok());
                            return Ok(SessionCompileOutcome {
                                exit_code: exit.code,
                                cache_outcome,
                            });
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(tag(output_started, io::Error::other(e))),
            }
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| tag(output_started, e))?;
        if n == 0 {
            return Err(tag(
                output_started,
                io::Error::other("SESSION relay closed before Exit"),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_output_error_is_attributed_without_output() {
        let err = SessionError::pre_output(io::Error::other("connect refused"));
        assert!(
            !err.output_started,
            "a setup failure must be identified as pre-output"
        );
    }

    #[test]
    fn compile_session_uses_only_the_serialized_client_environment() {
        let start = compile_session_start(
            &["rustc".into(), "--version".into()],
            "/workspace".into(),
            vec![running_process::broker::protocol_v2::SessionEnvVar {
                key: "CLIENT_ONLY".into(),
                value: "present".into(),
            }],
        );

        assert!(start.clear_inherited_env);
        assert_eq!(
            start.environment_policy,
            running_process::broker::protocol_v2::EnvironmentPolicy::Clear as i32
        );
        assert_eq!(start.env.len(), 1);
        assert_eq!(start.env[0].key, "CLIENT_ONLY");
    }

    #[test]
    fn busy_class_is_bounded_and_missing_endpoints_are_concrete() {
        assert!(broker_connect_is_busy(
            &io::Error::from_raw_os_error(231),
            Duration::from_millis(500)
        ));
        assert!(!broker_connect_is_busy(
            &io::Error::new(io::ErrorKind::NotFound, "absent"),
            Duration::ZERO
        ));
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert!(broker_connect_is_busy(
                &io::Error::from_raw_os_error(2),
                Duration::from_millis(1)
            ));
            assert!(!broker_connect_is_busy(
                &io::Error::from_raw_os_error(2),
                Duration::from_millis(51)
            ));
        }
    }

    #[test]
    fn hello_retry_is_limited_to_pre_reply_disconnects() {
        assert!(broker_hello_retryable(&io::Error::other(
            "broker closed before Hello reply"
        )));
        assert!(broker_hello_retryable(&io::Error::new(
            io::ErrorKind::ConnectionReset,
            "replaced broker"
        )));
        assert!(!broker_hello_retryable(&io::Error::other(
            "broker refused the daemon route"
        )));
        assert!(!broker_hello_retryable(&io::Error::new(
            io::ErrorKind::TimedOut,
            "route acquisition ceiling"
        )));
    }

    #[test]
    fn relayed_diagnostic_suppresses_the_silent_fault_annotation() {
        assert!(mark_relayed_output(
            b"compiler terminated by a Unix signal\n"
        ));
        assert!(crate::exit_guard::spoke());
        assert!(!crate::exit_guard::needs_annotation(
            -1,
            crate::exit_guard::spoke()
        ));
    }

    #[test]
    fn accepted_relay_that_never_negotiates_is_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (mut client, _server) = tokio::io::duplex(64);
            let error = read_negotiated_with_deadlines(
                &mut client,
                1,
                crate::broker_deadlines::BrokerDeadlines {
                    busy_budget: Duration::from_millis(10),
                    first_response: Duration::from_millis(20),
                    progress_silence: Duration::from_millis(20),
                    route_ceiling: Duration::from_secs(1),
                },
            )
            .await
            .expect_err("a silent relay must not wait forever");
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(error.to_string().contains("first-response deadline"));
        });
    }

    #[test]
    fn continuous_progress_is_still_bounded_by_route_ceiling() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (mut client, mut server) = tokio::io::duplex(4096);
            let writer = tokio::spawn(async move {
                let mut elapsed_ms = 1_u64;
                loop {
                    let progress = crate::broker_server::RouteProgress {
                        stage: "probe".into(),
                        attempt: 3,
                        elapsed_ms,
                        latest_result: "daemon still starting".into(),
                        retry_after_ms: 0,
                    };
                    let frame = Frame {
                        envelope_version: running_process::broker::protocol::PROTOCOL_VERSION,
                        kind: FrameKind::Event as i32,
                        payload_protocol: crate::broker_server::ROUTE_PROGRESS_PAYLOAD_PROTOCOL,
                        payload: progress.encode_to_vec(),
                        request_id: 2476,
                        payload_encoding: PayloadEncoding::None as i32,
                        deadline_unix_ms: 0,
                        traceparent: String::new(),
                        tracestate: String::new(),
                    };
                    let bytes = encode_framed(&frame).expect("encode progress");
                    if server.write_all(&bytes).await.is_err() {
                        break;
                    }
                    elapsed_ms += 5;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            let error = read_negotiated_with_deadlines(
                &mut client,
                2476,
                crate::broker_deadlines::BrokerDeadlines {
                    busy_budget: Duration::from_millis(100),
                    first_response: Duration::from_millis(200),
                    // 40x the 5ms progress cadence: a contended runner's late
                    // scheduler wake must never turn the expected *ceiling*
                    // timeout into a *silence* timeout (four Windows-lane
                    // failures on 2026-08-16 with the old 20ms budget).
                    progress_silence: Duration::from_millis(200),
                    route_ceiling: Duration::from_millis(400),
                },
            )
            .await
            .expect_err("progress must not defeat the absolute ceiling");
            writer.abort();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(error.to_string().contains("route acquisition ceiling"));
        });
    }

    #[test]
    fn compile_service_that_never_publishes_is_bounded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (mut client, _server) = tokio::io::duplex(64);
            let err =
                pump_session_output_with_timeout(&mut client, std::time::Duration::from_millis(20))
                    .await
                    .expect_err("a silent compile service must time out");
            assert_eq!(err.source.kind(), io::ErrorKind::TimedOut);
            assert!(!err.output_started);
        });
    }
}
