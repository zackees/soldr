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

/// Resolve the companion SESSION socket path for `program` — the same
/// derivation the broker binds and the client dials.
pub fn session_socket_path(program: &str) -> io::Result<String> {
    use running_process::broker::lifecycle::names_v2::v2_program_pipe;
    use running_process::broker::lifecycle::sid::user_sid_hash;
    use running_process::broker::server::singleton_bind::resolve_socket_path;

    let sid = user_sid_hash().map_err(|e| io::Error::other(format!("user_sid_hash: {e}")))?;
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
            if let Err(err) = rt.block_on(
                running_process::broker::server::session_serve_async::serve_broker_session_socket(
                    &session_socket,
                    &responder,
                    &peer_policy,
                ),
            ) {
                eprintln!("soldr broker: SESSION relay ended: {err}");
            }
        })
        .map(|_| ())
        .map_err(|e| io::Error::other(format!("spawn SESSION relay thread: {e}")))
}

/// Run one compile over the SESSION path using this process's cwd + environment.
///
/// `rustc_argv[0]` is the compiler path; `rustc_argv[1..]` its arguments.
pub fn run_session_compile(program: &str, rustc_argv: &[String]) -> io::Result<i32> {
    let cwd = std::env::current_dir()?.display().to_string();
    let env: Vec<SessionEnvVar> = std::env::vars()
        .map(|(key, value)| SessionEnvVar { key, value })
        .collect();
    run_session_compile_with(program, rustc_argv, cwd, env)
}

/// [`run_session_compile`] with an explicit `cwd` + `env` (the carried
/// `SessionStart` fields) — the daemon filters the env itself. Explicit so the
/// SESSION e2e can drive a deterministic compile without mutating process state.
pub fn run_session_compile_with(
    program: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> io::Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_session_compile_async(program, rustc_argv, cwd, env))
}

async fn run_session_compile_async(
    program: &str,
    rustc_argv: &[String],
    cwd: String,
    env: Vec<SessionEnvVar>,
) -> io::Result<i32> {
    let session_socket = session_socket_path(program)?;
    let name = local_session_name(&session_socket)?;
    let mut stream = Stream::connect(name).await?;

    // v2 Hello — identical to legacy. The relay's responder ignores the payload,
    // so an empty Hello suffices to negotiate.
    let hello = encode_framed(&Frame::request(CONTROL_PAYLOAD_PROTOCOL, Vec::new()))
        .map_err(io::Error::other)?;
    stream.write_all(&hello).await?;
    stream.flush().await?;
    read_negotiated(&mut stream).await?;

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
    .map_err(io::Error::other)?;
    stream.write_all(&start_frame).await?;
    stream.flush().await?;

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

/// Pump SESSION frames from the relay: stdout/stderr to local stdio, returning
/// the compiler exit code on the terminal `Exit` frame.
async fn pump_session_output(stream: &mut Stream) -> io::Result<i32> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    loop {
        while let Some(decoded) = try_decode_session_frame(&buf).map_err(io::Error::other)? {
            let consumed = decoded.consumed;
            let kind = decoded.frame.kind.clone();
            buf.drain(..consumed);
            match kind {
                Some(session_frame::Kind::Stdout(b)) => {
                    stdout.write_all(&b).await?;
                    stdout.flush().await?;
                }
                Some(session_frame::Kind::Stderr(b)) => {
                    stderr.write_all(&b).await?;
                    stderr.flush().await?;
                }
                Some(session_frame::Kind::Exit(exit)) => return Ok(exit.code),
                _ => {}
            }
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::other("SESSION relay closed before Exit"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
