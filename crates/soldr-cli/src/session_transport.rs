//! SESSION `0x5350` client + broker-relay wiring (soldr#2388 Step 7 / #2386
//! Option A, topology (c) — two sockets).
//!
//! Per the advisor ruling on the broker-serve composition:
//! - the **broker** keeps its sync control socket (`serve_launching_backends`,
//!   launch + legacy adopt) and adds a **companion SESSION socket** running the
//!   proven async relay [`serve_broker_session_socket`] on its own tokio runtime
//!   thread. A negotiated Hello relays (`copy_bidirectional`) to the daemon's
//!   SESSION endpoint (`backend_pipe`);
//! - the **client** dials that companion socket, sends the standard v2 Hello
//!   (`CONTROL_PAYLOAD_PROTOCOL` — identical to legacy), reads `Negotiated`, then
//!   drives the SESSION wire directly with the sans-io `session_codec` (no
//!   `daemon`-gated `run_session_client`): `SessionStart` out, then
//!   stdout/stderr/exit frames in.
//!
//! `backend_pipe` is the daemon's deterministic SESSION endpoint
//! ([`daemon_session_endpoint_path`](crate::daemon::session_endpoint::daemon_session_endpoint_path)),
//! the #2386 Option-A "bind-by-advertised-name" contract — portable across Unix
//! sockets and Windows named pipes (Windows has no fd handover).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Stream;
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use running_process::broker::protocol::{
    encode_framed, hello_reply::Result as HelloReplyResult, try_decode_framed, ErrorCode, Hello,
    Refused, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION,
};
use running_process::broker::protocol::{Frame, HelloReply, Negotiated};
use running_process::broker::protocol_v2::{
    session_frame, SessionEnvVar, SessionFrame, SessionStart,
};
use running_process::broker::server::connection::{HelloResponder, PeerCredentialPolicy};
use running_process::broker::server::hello_handler::PeerIdentity;
use running_process::broker::session_codec::{encode_session_frame, try_decode_session_frame};

/// Companion SESSION-socket pipe index, distinct from the control socket's `0`
/// (`broker_cmd::BROKER_PIPE_IDX`). Both the broker (bind) and the client (dial)
/// derive the same path from `broker_program()` via this index.
const SESSION_PIPE_IDX: u32 = 1;
const BROKER_ROUTE_ATTEMPT_BUDGET_ENV: &str = "SOLDR_BROKER_ROUTE_ATTEMPT_BUDGET_MS";
const BROKER_ROUTE_ATTEMPT_BUDGET_DEFAULT_MS: u64 = 5_000;
const SESSION_ATTEMPT_BUDGET_DEFAULT_MS: u64 = 6_000;
const _: () = assert!(SESSION_ATTEMPT_BUDGET_DEFAULT_MS > BROKER_ROUTE_ATTEMPT_BUDGET_DEFAULT_MS);

/// Retry cadence while the mandatory broker/daemon SESSION route comes up.
const SESSION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Outcome of the SESSION compile hot path ([`session_hot_path`]), consumed by
/// `compile_dispatch`. All bounded retry and error attribution lives here.
pub enum SessionHotPathOutcome {
    /// SESSION served the compile; return this exit code.
    Served(i32),
    /// SESSION infrastructure failed; cacheable compiles have no alternate route.
    HardFail(io::Error),
}

/// Short budget for smoothing broker-to-route startup. Broker-unreachable
/// errors return immediately; an existing broker gets a bounded window to
/// launch or reconnect the requested daemon partition.
/// Overridable for tests via `SOLDR_SESSION_ATTEMPT_BUDGET_MS`.
fn session_attempt_budget() -> std::time::Duration {
    let ms = std::env::var("SOLDR_SESSION_ATTEMPT_BUDGET_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        // The broker's route verdict is independently capped at 5s. Keep a
        // small delivery margin so the outer SESSION timeout cannot drop the
        // live pipe exactly as that bounded verdict arrives.
        .unwrap_or(SESSION_ATTEMPT_BUDGET_DEFAULT_MS)
        .max(1);
    std::time::Duration::from_millis(ms)
}

/// Mandatory SESSION compile hot path: relay client → broker → daemon.
/// A missing broker fails immediately; an existing broker gets the bounded
/// [`session_attempt_budget`] to provide the requested route. Every terminal
/// infrastructure error is hard because there is no legacy acquisition path.
pub fn session_hot_path(rustc_argv: &[String]) -> SessionHotPathOutcome {
    use std::time::Instant;

    let program = crate::daemon::backend_handle_adoption::broker_program();
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

    let session_deadline = Instant::now() + session_attempt_budget();
    loop {
        match run_session_compile_with_detailed_for_service(
            &program,
            &service_name,
            rustc_argv,
            cwd.clone(),
            env.clone(),
        ) {
            Ok(outcome) => {
                // Observability (opt-in, no production noise): a SESSION-served
                // marker the multi-process smoke greps to prove SESSION carried
                // the compile. `cache_outcome`: 1=Hit, 2=Miss, 3=Error.
                if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
                    eprintln!(
                        "soldr: SESSION compile served (cache_outcome={:?})",
                        outcome.cache_outcome
                    );
                }
                return SessionHotPathOutcome::Served(outcome.exit_code);
            }
            Err(err) if err.output_started => return SessionHotPathOutcome::HardFail(err.source),
            // No broker is bound here, so do not burn the route-start budget
            // repeatedly dialing a dead socket.
            Err(err) if err.broker_unreachable => {
                return SessionHotPathOutcome::HardFail(io::Error::other(format!(
                    "soldr broker is unreachable: {err}; invoke this compiler through `soldr cargo ...` (or another soldr build front door) so the singleton broker is started"
                )));
            }
            Err(err) if Instant::now() >= session_deadline => {
                return SessionHotPathOutcome::HardFail(io::Error::other(format!(
                    "soldr broker could not provide daemon route {service_name} within {}ms: {err}; inspect `soldr logs paths` and `soldr daemon status`",
                    session_attempt_budget().as_millis()
                )));
            }
            Err(_) => std::thread::sleep(SESSION_RETRY_INTERVAL),
        }
    }
}

/// Resolve the companion SESSION socket path for `program` — the same
/// derivation the broker binds and the client dials.
pub fn session_socket_path(program: &str) -> io::Result<String> {
    use running_process::broker::lifecycle::names_v2::v2_broker_path_pipe;
    use running_process::broker::server::singleton_bind::resolve_path_scoped_socket_path;

    let broker = crate::installed_broker_identity::installed_broker_executable()?;
    let pipe = v2_broker_path_pipe(program, &broker, SESSION_PIPE_IDX)
        .map_err(|e| io::Error::other(format!("path-scoped broker pipe: {e}")))?;
    resolve_path_scoped_socket_path(&pipe)
        .map_err(|e| io::Error::other(format!("resolve path-scoped socket: {e}")))
}

pub(crate) fn local_session_name(
    socket_path: &str,
) -> io::Result<interprocess::local_socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        socket_path.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        socket_path.to_ns_name::<GenericNamespaced>()
    }
}

/// A permissive responder that negotiates every Hello to a fixed
/// `backend_pipe` — the daemon's SESSION endpoint. For Step 7a the SESSION
/// socket serves exactly one daemon, so a fixed target is correct; 7b swaps this
/// for a `HelloRouter` that launches-on-miss. Peer credentials still gate every
/// connection before this runs (`serve_broker_session_socket`).
enum SessionRelayRoute {
    Fixed(String),
    Broker { program: String },
}

struct SessionRelayResponder {
    route: SessionRelayRoute,
}

impl HelloResponder for SessionRelayResponder {
    fn handle_frame(&self, frame: Frame, _peer: PeerIdentity) -> HelloReply {
        match &self.route {
            SessionRelayRoute::Fixed(backend_pipe) => HelloReply {
                result: Some(HelloReplyResult::Negotiated(Negotiated {
                    backend_pipe: backend_pipe.clone(),
                    ..Default::default()
                })),
            },
            SessionRelayRoute::Broker { program } => route_hello_via_control(program, &frame),
        }
    }
}

fn route_hello_via_control(program: &str, frame: &Frame) -> HelloReply {
    let hello = match Hello::decode(frame.payload.as_slice()) {
        Ok(hello) => hello,
        Err(err) => return refused_session_hello(format!("invalid SESSION Hello: {err}")),
    };
    let broker = match crate::installed_broker_identity::installed_broker_executable() {
        Ok(path) => path,
        Err(err) => {
            return refused_session_hello(format!("broker install identity unavailable: {err}"))
        }
    };
    match running_process::broker::client_v2::connect_service_for_broker_path_with_deadline(
        program,
        &broker,
        &hello.service_name,
        &hello.wanted_version,
        broker_route_attempt_budget(),
    ) {
        Ok(session) => HelloReply {
            result: Some(HelloReplyResult::Negotiated(session.negotiated().clone())),
        },
        Err(err) => refused_session_hello(format!("daemon route unavailable: {err}")),
    }
}

fn broker_route_attempt_budget() -> std::time::Duration {
    let milliseconds = std::env::var(BROKER_ROUTE_ATTEMPT_BUDGET_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(BROKER_ROUTE_ATTEMPT_BUDGET_DEFAULT_MS)
        .max(1);
    std::time::Duration::from_millis(milliseconds)
}

pub(crate) fn connect_default_daemon_route(
    service_name: &str,
) -> Result<running_process::broker::client_v2::ClientSession, String> {
    let broker = crate::installed_broker_identity::installed_broker_executable()
        .map_err(|err| err.to_string())?;
    running_process::broker::client_v2::connect_service_for_broker_path_with_deadline(
        &crate::daemon::backend_handle_adoption::broker_program(),
        broker,
        service_name,
        crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION,
        broker_route_attempt_budget(),
    )
    .map_err(|err| err.to_string())
}

fn refused_session_hello(reason: String) -> HelloReply {
    HelloReply {
        result: Some(HelloReplyResult::Refused(Refused {
            reason,
            code: ErrorCode::ErrorBackendSpawnFailed as i32,
            ..Default::default()
        })),
    }
}

/// Spawn the broker's companion SESSION relay on its own thread + tokio runtime.
///
/// Binds the SESSION endpoint before returning, then serves it on a dedicated
/// thread until the process exits. Synchronous bind is the broker's first
/// ownership point: only that winner may proceed to bind the control endpoint,
/// so concurrent starters cannot split the two pipes across processes.
pub fn spawn_session_relay(program: &str, backend_pipe: String) -> io::Result<()> {
    spawn_session_relay_with(program, SessionRelayRoute::Fixed(backend_pipe))
}

pub fn spawn_routed_session_relay(program: &str) -> io::Result<()> {
    spawn_session_relay_with(
        program,
        SessionRelayRoute::Broker {
            program: program.to_string(),
        },
    )
}

fn spawn_session_relay_with(program: &str, route: SessionRelayRoute) -> io::Result<()> {
    use running_process::broker::server::session_serve_async::serve_broker_session_endpoint_concurrently;

    let session_socket = session_socket_path(program)?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("soldr-broker-session".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = ready_tx.send(Err(io::Error::other(format!(
                        "build SESSION relay runtime: {err}"
                    ))));
                    return;
                }
            };
            let Some(peer_policy) = PeerCredentialPolicy::current_user() else {
                let _ = ready_tx.send(Err(io::Error::other(
                    "SESSION relay peer policy unavailable",
                )));
                return;
            };
            let listener = {
                let _runtime_guard = runtime.enter();
                match crate::daemon::session_endpoint::bind_session_listener(&session_socket) {
                    Ok(listener) => listener,
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                }
            };
            let responder = Arc::new(SessionRelayResponder { route });
            println!("soldr broker: SESSION relay bound at {session_socket}");
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            runtime.block_on(async move {
                if let Err(err) =
                    serve_broker_session_endpoint_concurrently(listener, responder, &peer_policy)
                        .await
                {
                    eprintln!("soldr broker: SESSION relay ended: {err}");
                }
            });
        })
        .map_err(|e| io::Error::other(format!("spawn SESSION relay thread: {e}")))?;
    ready_rx
        .recv()
        .map_err(|err| io::Error::other(format!("SESSION relay exited before bind: {err}")))?
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

/// Run one compile over the SESSION path using this process's cwd + environment.
///
/// `rustc_argv[0]` is the compiler path; `rustc_argv[1..]` its arguments.
pub fn run_session_compile(program: &str, rustc_argv: &[String]) -> Result<i32, SessionError> {
    let cwd = std::env::current_dir()
        .map_err(SessionError::pre_output)?
        .display()
        .to_string();
    let env: Vec<SessionEnvVar> = std::env::vars()
        .map(|(key, value)| SessionEnvVar { key, value })
        .collect();
    run_session_compile_with(program, rustc_argv, cwd, env)
}

/// The result of a SESSION compile: the compiler exit code plus the daemon's
/// `cache_outcome` (`CacheOutcome` discriminant: 1=Hit, 2=Miss, 3=Error) carried
/// on the terminal `Exit` frame's metadata — `None` if the daemon did not report
/// one (e.g. an infra exit).
#[derive(Debug, Clone)]
pub struct SessionCompileOutcome {
    /// The compiler's exit code.
    pub exit_code: i32,
    /// The daemon's cache outcome discriminant, if reported.
    pub cache_outcome: Option<i32>,
}

/// [`run_session_compile`] with an explicit `cwd` + `env` (the carried
/// `SessionStart` fields) — the daemon filters the env itself. Explicit so the
/// SESSION e2e can drive a deterministic compile without mutating process state.
pub fn run_session_compile_with(
    program: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<i32, SessionError> {
    run_session_compile_with_detailed(program, rustc_argv, cwd, env).map(|o| o.exit_code)
}

/// [`run_session_compile_with`] returning the full [`SessionCompileOutcome`]
/// (exit code + `cache_outcome`), for callers that assert or log the cache
/// decision (the anchor e2e; hot-path observability).
pub fn run_session_compile_with_detailed(
    program: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    run_session_compile_with_detailed_for_service(program, program, rustc_argv, cwd, env)
}

fn run_session_compile_with_detailed_for_service(
    program: &str,
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SessionError::pre_output)?
        .block_on(run_session_compile_async(
            program,
            service_name,
            rustc_argv,
            cwd,
            env,
        ))
}

async fn run_session_compile_async(
    program: &str,
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    let setup_timeout = session_attempt_budget();
    let mut stream = match tokio::time::timeout(
        setup_timeout,
        establish_session(program, service_name, rustc_argv, cwd, env),
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
    program: &str,
    service_name: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<Stream, SessionError> {
    // Setup — connect / Hello / negotiate / SessionStart send. Failures here
    // are tagged pre-output for precise diagnostics.
    let session_socket = session_socket_path(program).map_err(SessionError::broker_unreachable)?;
    if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
        eprintln!("soldr: SESSION dialing program={program} socket={session_socket}");
    }
    let name = local_session_name(&session_socket).map_err(SessionError::broker_unreachable)?;
    // A failed dial means no broker is bound at this socket — fall through to
    // legacy immediately rather than retrying (soldr#2388 Step 3).
    let mut stream = Stream::connect(name)
        .await
        .map_err(SessionError::broker_unreachable)?;

    // v2 Hello — identical to legacy. The relay's responder ignores the payload,
    // so an empty Hello suffices to negotiate.
    let hello_payload = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: service_name.to_string(),
        wanted_version: crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION
            .to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        request_id: format!("soldr-session-{}", std::process::id()),
        peer_pid: std::process::id(),
        client_lib_name: "soldr-session".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    }
    .encode_to_vec();
    let hello = encode_framed(&Frame::request(CONTROL_PAYLOAD_PROTOCOL, hello_payload))
        .map_err(|e| SessionError::pre_output(io::Error::other(e)))?;
    stream
        .write_all(&hello)
        .await
        .map_err(SessionError::pre_output)?;
    stream.flush().await.map_err(SessionError::pre_output)?;
    read_negotiated(&mut stream)
        .await
        .map_err(SessionError::pre_output)?;

    // From here the connection is a transparent SESSION relay to the daemon.
    let start = SessionStart {
        program: rustc_argv.first().cloned().unwrap_or_default(),
        args: rustc_argv.get(1..).unwrap_or_default().to_vec(),
        cwd,
        env,
        clear_inherited_env: false,
    };
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

    // Output phase — a failure after the first byte is printed is a hard error
    // (a legacy retry would double-print).
    Ok(stream)
}

/// Read and validate the broker's framed `Negotiated` reply.
async fn read_negotiated<S>(stream: &mut S) -> io::Result<()>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use prost::Message as _;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(decoded) = try_decode_framed(&buf).map_err(io::Error::other)? {
            let reply =
                HelloReply::decode(decoded.frame.payload.as_slice()).map_err(io::Error::other)?;
            return match reply.result {
                Some(HelloReplyResult::Negotiated(_)) => Ok(()),
                other => Err(io::Error::other(format!(
                    "broker did not negotiate the SESSION Hello: {other:?}"
                ))),
            };
        }
        let n = stream.read(&mut chunk).await?;
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

    crate::timed_test!(pre_output_error_is_attributed_without_output, {
        let err = SessionError::pre_output(io::Error::other("connect refused"));
        assert!(
            !err.output_started,
            "a setup failure must be identified as pre-output"
        );
    });

    crate::timed_test!(relayed_diagnostic_suppresses_the_silent_fault_annotation, {
        assert!(mark_relayed_output(
            b"compiler terminated by a Unix signal\n"
        ));
        assert!(crate::exit_guard::spoke());
        assert!(!crate::exit_guard::needs_annotation(
            -1,
            crate::exit_guard::spoke()
        ));
    });

    crate::timed_test!(accepted_relay_that_never_negotiates_is_bounded, {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (mut client, _server) = tokio::io::duplex(64);
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(20),
                read_negotiated(&mut client),
            )
            .await;
            assert!(result.is_err(), "a silent relay must not wait forever");
        });
    });

    crate::timed_test!(compile_service_that_never_publishes_is_bounded, {
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
    });
}
