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

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use running_process::broker::protocol::{
    encode_framed, hello_reply::Result as HelloReplyResult, try_decode_framed,
    CONTROL_PAYLOAD_PROTOCOL,
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

/// Whether the client should route the compile through the SESSION transport
/// (client → broker relay → daemon) before the legacy direct-connect.
///
/// **Always true** (soldr#2388): SESSION is the compile hot path, with no
/// env-var opt-out — the broker-fronted daemon is the only supported topology.
/// A SESSION failure still falls back to legacy pre-output (see
/// `compile_dispatch` / [`session_hot_path`]), so "always attempt SESSION" is
/// safe: a broker/daemon that is not up yet degrades to legacy, never wedges.
pub fn session_enabled() -> bool {
    crate::broker_spawn::broker_enabled()
}

/// Retry cadence for the SESSION hot path while the broker/daemon come up.
const SESSION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Outcome of the SESSION compile hot path ([`session_hot_path`]), consumed by
/// `compile_dispatch`. All SESSION policy (opt-in gate, broker/daemon ensure,
/// pre-output retry, and the mid-stream-vs-fallback boundary) lives here rather
/// than in the already-oversized `compile_dispatch`.
pub enum SessionHotPathOutcome {
    /// SESSION served the compile; return this exit code.
    Served(i32),
    /// SESSION failed AFTER output began — a legacy retry would double-print, so
    /// this is a hard error carrying the cause.
    HardFail(io::Error),
    /// SESSION was disabled, or failed pre-output (nothing printed); the caller
    /// should run the legacy path.
    Fallthrough,
}

/// SESSION compile hot path (soldr#2388 Step 3/7b): **default ON**. Relay the
/// compile client → broker → daemon; the broker owns daemon launch.
///
/// Fallback contract (see the module + `compile_dispatch`):
/// * **broker unreachable** (no broker bound at the SESSION socket) → fall
///   through to legacy **immediately**, never spinning the spawn budget on a
///   socket nothing is listening on;
/// * broker up but relay still settling → retry within the short
///   [`session_attempt_budget`], then fall through to legacy;
/// * a **mid-output** failure → hard error (a legacy retry would double-print).
///
/// The legacy path this falls through to owns robust daemon acquisition and the
/// no-silent-uncached-fallback attribution, so a SESSION-unavailable compile is
/// always served (or hard-failed with a remedy), never silently uncached.
pub fn session_hot_path(rustc_argv: &[String]) -> SessionHotPathOutcome {
    use std::time::Instant;

    if !session_enabled() {
        return SessionHotPathOutcome::Fallthrough;
    }
    let program = crate::daemon::backend_handle_adoption::broker_program();
    let Ok(cwd) = std::env::current_dir() else {
        return SessionHotPathOutcome::Fallthrough;
    };
    let cwd = cwd.display().to_string();
    let env: Vec<SessionEnvVar> = std::env::vars()
        .map(|(key, value)| SessionEnvVar { key, value })
        .collect();

    // soldr#2388 Step 4: the broker is the sole daemon-spawner — this path never
    // spawns. A broker that is up but whose daemon is still cold-starting shows
    // up as a (non-`broker_unreachable`) pre-output error; retry it across the
    // full daemon spawn-retry budget so the broker-launched daemon has time to
    // bind. A broker that is simply ABSENT (`broker_unreachable`) falls through
    // to legacy immediately rather than waiting on a socket nothing serves.
    let session_deadline = Instant::now() + crate::compile_dispatch::resolved_spawn_retry_budget();
    loop {
        match run_session_compile_with_detailed(&program, rustc_argv, cwd.clone(), env.clone()) {
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
            // No broker bound here — don't burn the budget dialing a dead
            // socket; the legacy path is the correct home for this compile.
            Err(err) if err.broker_unreachable => {
                if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
                    eprintln!("soldr: SESSION broker unreachable ({err}); using legacy path");
                }
                return SessionHotPathOutcome::Fallthrough;
            }
            Err(err) if Instant::now() >= session_deadline => {
                if std::env::var_os("SOLDR_SESSION_DEBUG").is_some() {
                    eprintln!("soldr: SESSION unavailable ({err}); using legacy path");
                }
                return SessionHotPathOutcome::Fallthrough;
            }
            Err(_) => std::thread::sleep(SESSION_RETRY_INTERVAL),
        }
    }
}

/// Resolve the companion SESSION socket path for `program` — the same
/// derivation the broker binds and the client dials.
pub fn session_socket_path(program: &str) -> io::Result<String> {
    use running_process::broker::lifecycle::names_v2::v2_program_pipe;
    use running_process::broker::server::singleton_bind::resolve_socket_path;

    // soldr#2388: same container-safe identity the broker binds with, so a
    // machine-id-less environment still agrees on the socket name.
    let sid = crate::broker_identity::resolve_user_sid();
    let pipe = v2_program_pipe(program, &sid, SESSION_PIPE_IDX)
        .map_err(|e| io::Error::other(format!("v2_program_pipe: {e}")))?;
    resolve_socket_path(&pipe).map_err(|e| io::Error::other(format!("resolve_socket_path: {e}")))
}

fn local_session_name(socket_path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
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
struct SessionRelayResponder {
    backend_pipe: String,
}

impl HelloResponder for SessionRelayResponder {
    fn handle_frame(&self, _frame: Frame, _peer: PeerIdentity) -> HelloReply {
        HelloReply {
            result: Some(HelloReplyResult::Negotiated(Negotiated {
                backend_pipe: self.backend_pipe.clone(),
                ..Default::default()
            })),
        }
    }
}

/// Spawn the broker's companion SESSION relay on its own thread + tokio runtime.
///
/// Non-blocking: returns once the thread is spawned. The relay serves until the
/// process exits. `backend_pipe` is the daemon's SESSION endpoint the relay
/// dials per negotiated connection.
pub fn spawn_session_relay(program: &str, backend_pipe: String) -> io::Result<()> {
    use running_process::broker::server::session_serve_async::serve_broker_session_endpoint;

    let session_socket = session_socket_path(program)?;
    std::thread::Builder::new()
        .name("soldr-broker-session".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    eprintln!("soldr broker: SESSION relay runtime build failed: {err}");
                    return;
                }
            };
            let Some(peer_policy) = PeerCredentialPolicy::current_user() else {
                eprintln!(
                    "soldr broker: SESSION relay peer policy unavailable; not serving SESSION"
                );
                return;
            };
            let responder = SessionRelayResponder { backend_pipe };
            rt.block_on(async move {
                // Bind with soldr's OWN listener helper so the relay's socket name
                // is produced by the exact same conversion the client's dial uses
                // (`local_session_name`) — binding through running-process's
                // `serve_broker_session_socket` used a separate name path, which
                // could yield a pipe the client never finds (os error 2). Emit a
                // real BOUND signal only after the socket exists, so callers never
                // race the bind.
                let listener =
                    match crate::daemon::session_endpoint::bind_session_listener(&session_socket) {
                        Ok(listener) => listener,
                        Err(err) => {
                            eprintln!(
                            "soldr broker: SESSION relay could not bind {session_socket}: {err}"
                        );
                            return;
                        }
                    };
                println!("soldr broker: SESSION relay bound at {session_socket}");
                if let Err(err) =
                    serve_broker_session_endpoint(listener, &responder, &peer_policy).await
                {
                    eprintln!("soldr broker: SESSION relay ended: {err}");
                }
            });
        })
        .map(|_| ())
        .map_err(|e| io::Error::other(format!("spawn SESSION relay thread: {e}")))
}

/// A SESSION compile failure, tagged with whether any compiler output was
/// already emitted locally.
///
/// The fallback-safety boundary (see `compile_dispatch`): a **pre-output**
/// failure (connect / Hello / negotiate / `SessionStart` send) is safe to retry
/// on the legacy path — nothing was printed. Once the daemon's output has begun
/// streaming to local stdio, a legacy retry would **double-print**, so such a
/// failure must be surfaced as a hard error instead.
#[derive(Debug)]
pub struct SessionError {
    /// Whether compiler stdout/stderr was written locally before the failure.
    pub output_started: bool,
    /// Whether the broker's SESSION socket was unreachable (dial refused /
    /// absent) — i.e. there is no broker to talk to, as opposed to a broker
    /// that answered but whose daemon relay is not ready yet. The hot path
    /// treats this as "fall through to legacy immediately" rather than retrying
    /// (soldr#2388 Step 3): retrying a socket nothing is bound to just burns the
    /// spawn budget before the inevitable legacy fallback.
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(SessionError::pre_output)?
        .block_on(run_session_compile_async(program, rustc_argv, cwd, env))
}

async fn run_session_compile_async(
    program: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> Result<SessionCompileOutcome, SessionError> {
    // Setup — connect / Hello / negotiate / SessionStart send. Every failure
    // here is pre-output (nothing printed yet), so it is safe to fall back.
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
    let hello = encode_framed(&Frame::request(CONTROL_PAYLOAD_PROTOCOL, Vec::new()))
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
    pump_session_output(&mut stream).await
}

/// Read and validate the broker's framed `Negotiated` reply.
async fn read_negotiated(stream: &mut Stream) -> io::Result<()> {
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

/// Pump SESSION frames from the relay: stdout/stderr to local stdio, returning
/// the compiler exit code + `cache_outcome` on the terminal `Exit` frame.
///
/// `output_started` flips to `true` the moment any stdout/stderr byte is written
/// locally; every error is tagged with it so the caller knows whether a legacy
/// fallback would double-print.
async fn pump_session_output(stream: &mut Stream) -> Result<SessionCompileOutcome, SessionError> {
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
                            stdout
                                .write_all(&b)
                                .await
                                .map_err(|e| tag(output_started, e))?;
                            stdout.flush().await.map_err(|e| tag(output_started, e))?;
                        }
                        Some(session_frame::Kind::Stderr(b)) => {
                            output_started = true;
                            stderr
                                .write_all(&b)
                                .await
                                .map_err(|e| tag(output_started, e))?;
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

    crate::timed_test!(pre_output_error_is_safe_to_fall_back, {
        let err = SessionError::pre_output(io::Error::other("connect refused"));
        assert!(
            !err.output_started,
            "a pre-output failure must be flagged safe for legacy fallback"
        );
    });

    crate::timed_test!(session_is_unconditionally_enabled, {
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // soldr#2388: SESSION is the only supported hot path — no env-var
        // opt-out. Enabled regardless of any (now-ignored) env state.
        let _s = crate::EnvVarGuard::set("SOLDR_USE_SESSION", "0");
        let _b = crate::EnvVarGuard::set("SOLDR_USE_BROKER", "0");
        assert!(
            session_enabled(),
            "SESSION is unconditional; there is no opt-out env var"
        );
    });
}
