//! Daemon SESSION `0x5350` per-connection handler (soldr#2388 Step 6d / #2386
//! Option A).
//!
//! The audit correction (soldr#2365 comment 5233624686) rules #2386 **Option
//! A**: soldr-daemon serves SESSION `0x5350` on the **separate broker-facing
//! backend endpoint** the broker binds and hands over, running the codec-bridge
//! [`serve_session_compile`](crate::daemon::session_serve::serve_session_compile)
//! behind it. `handle_connection` (the legacy endpoint) is untouched — its
//! `Payload{0x5350} → drain_then_close` stays as the defensive default.
//!
//! That endpoint carries three traffic kinds (see running-process
//! `backend_sdk::mux`): the daemon's legacy wire (none here — SESSION is a
//! dedicated endpoint), `BackendHandle` **liveness probes**, and `0x5350`
//! payload frames. So this handler drives the **full**
//! [`BackendEndpointMux`]: it answers probe frames with
//! [`MuxPoll::ProbeAnswered`] and, on a `0x5350`
//! [`MuxPoll::Payload`](running_process::broker::backend_sdk::MuxPoll::Payload),
//! **takes over** the connection as a streaming SESSION compile.
//!
//! # Handoff — replay, do not drain
//!
//! `mux.poll` is request/reply and pure: it decodes the frame and reports how
//! many bytes it *would* consume, but never advances the buffer. SESSION is a
//! streaming takeover, and [`serve_session_compile`] re-reads the opening
//! `SessionStart` itself via `session_codec`. So on a `0x5350` frame this
//! handler does **not** drain — it hands the still-buffered bytes (the
//! `SessionStart` frame plus any trailing bytes) followed by the live stream to
//! [`serve_session_compile`] through a [`ReplayReader`]. The mux and
//! `session_codec` decode the byte-identical `[1][u32 len][Frame{0x5350}]`
//! envelope, so the re-read is correct and cheap. This mirrors running-process's
//! proven `session_takeover_from_buffered`, minus its child-spawn (soldr owns
//! execution via the embedded zccache service — fable5 answer-A ruling).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{BackendEndpointMux, LegacyClassification, MuxPoll};
use running_process::broker::protocol::SESSION_PAYLOAD_PROTOCOL;

use crate::core::SoldrPaths;
use crate::daemon::session_serve::serve_session_compile;
use crate::zccache_embedded::SoldrZccacheService;

/// Concrete legacy detector type of the SESSION mux — a plain `fn` pointer so
/// the mux (and therefore the accept-loop `Arc<..>`) has a nameable type.
pub type SessionMux = BackendEndpointMux<fn(&[u8]) -> LegacyClassification>;

/// Names an explicit soldr-owned SESSION endpoint socket path.
///
/// Phase 2 (Step 8) will instead adopt the **broker-passed** listener via
/// `RUNNING_PROCESS_BROKER_LISTENER_FD` (`broker_owned_bind::recover_from_env`),
/// which is where Option A's "broker binds, daemon inherits" actually lands.
/// Until the broker is the daemon's spawner, this opt-in lets the endpoint be
/// bound and exercised while keeping production unchanged: unset → no SESSION
/// endpoint is served (see [`resolve_session_listener`]).
pub(crate) const SOLDR_SESSION_ENDPOINT_PATH_ENV: &str = "SOLDR_SESSION_ENDPOINT_PATH";

/// The mux for the SESSION endpoint: serves the `0x5350` lane and declares
/// **no** legacy wire.
///
/// Distinct from `backend_handle_adoption::soldr_backend_endpoint_mux`, which is
/// built with `served = &[]` for the *legacy* endpoint and MUST NOT be reused —
/// serving `0x5350` there would change `handle_connection`, which the audit
/// correction forbids.
pub fn soldr_session_endpoint_mux(daemon: DaemonProcess) -> SessionMux {
    BackendEndpointMux::new(daemon, &[SESSION_PAYLOAD_PROTOCOL], classify_never_legacy)
}

/// The SESSION endpoint has no legacy wire: every framed connection is either a
/// `BackendHandle` probe (handled inside the mux) or a `0x5350` SESSION frame.
///
/// `mux.poll` short-circuits an empty buffer to `NeedMoreBytes` before calling
/// this detector, so it never sees an empty slice.
fn classify_never_legacy(_buf: &[u8]) -> LegacyClassification {
    LegacyClassification::NotLegacy
}

/// Serve one accepted SESSION-endpoint connection.
///
/// Drives the mux over a growing read buffer:
/// - [`MuxPoll::ProbeAnswered`] → write the reply verbatim, advance, keep
///   reading (a prober may reuse the connection or hang up);
/// - [`MuxPoll::Payload`] carrying `0x5350` → **replay** the undrained buffer +
///   the live stream into [`serve_session_compile`] and return whatever it does;
/// - clean EOF before any frame → `Ok(())`;
/// - a legacy-wire verdict → an error (the SESSION endpoint serves no legacy
///   wire).
///
/// # Errors
///
/// A transport error, a mux framing/protocol error, or an error surfaced by
/// [`serve_session_compile`]. A compile-service failure is *not* an `Err` —
/// [`serve_session_compile`] reports it in-band as a diagnostic `Stderr` + infra
/// `Exit` (no silent close).
pub(crate) async fn serve_session_connection<IO, F>(
    mut io: IO,
    service: &SoldrZccacheService,
    paths: &SoldrPaths,
    mux: &BackendEndpointMux<F>,
) -> io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
    F: Fn(&[u8]) -> LegacyClassification,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match mux.poll(&buf).map_err(io::Error::other)? {
            MuxPoll::NeedMoreBytes => {
                let n = io.read(&mut chunk).await?;
                if n == 0 {
                    // Clean EOF. If nothing is buffered the peer just hung up
                    // (e.g. a closed probe connection); a partial frame is a
                    // truncated request and is dropped the same way.
                    return Ok(());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            MuxPoll::ProbeAnswered { reply, consumed } => {
                io.write_all(&reply).await?;
                io.flush().await?;
                buf.drain(..consumed);
            }
            MuxPoll::Payload { frame, .. } => {
                debug_assert_eq!(frame.payload_protocol, SESSION_PAYLOAD_PROTOCOL);
                // SESSION takeover: DO NOT drain. Replay the whole buffer (the
                // SessionStart frame + any trailing bytes) so serve_session_compile
                // can re-read the opening SessionStart via session_codec.
                let replay = ReplayReader::new(buf, io);
                return serve_session_compile(replay, service, paths).await;
            }
            MuxPoll::Legacy => {
                return Err(io::Error::other(
                    "unexpected legacy-wire frame on the SESSION endpoint",
                ));
            }
        }
    }
}

/// An [`AsyncRead`] + [`AsyncWrite`] that first replays a leading byte buffer,
/// then delegates to an inner stream. Writes always go to the inner stream.
///
/// Used to hand [`serve_session_compile`] the mux's already-read bytes without
/// consuming them from the wire: the opening `SessionStart` frame is re-read
/// from `head`, and everything after is read straight from `inner`.
pub(crate) struct ReplayReader<S> {
    head: io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S> ReplayReader<S> {
    pub(crate) fn new(head: Vec<u8>, inner: S) -> Self {
        Self {
            head: io::Cursor::new(head),
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ReplayReader<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let pos = this.head.position() as usize;
        let data = this.head.get_ref();
        if pos < data.len() {
            let remaining = &data[pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.head.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ReplayReader<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The deterministic SESSION endpoint path this daemon serves, derived from
/// `paths` as a sibling of the legacy IPC endpoint (`daemon_sock_path` on Unix /
/// `daemon_pipe_name` on Windows) with a `-session` / `.session` suffix.
///
/// Both the daemon (which binds it) and the broker's SESSION relay (which dials
/// it as `Negotiated.backend_pipe`) compute this same value — the #2386
/// Option-A "bind-by-advertised-name" contract (mechanism ii), the portable
/// cross-platform path. (Unix fd-adopt via `broker_owned_bind` is an optional
/// optimization layered on later; Windows has no fd handover at all.)
pub fn daemon_session_endpoint_path(paths: &SoldrPaths) -> io::Result<String> {
    #[cfg(unix)]
    {
        let sock = crate::cache_lib::daemon_sock_path(paths);
        Ok(format!("{}.session", sock.display()))
    }
    #[cfg(windows)]
    {
        let pipe = crate::cache_lib::daemon_pipe_name(paths).map_err(io::Error::other)?;
        Ok(format!("{pipe}-session"))
    }
}

/// Resolve the SESSION endpoint listener the daemon serves.
///
/// Honors [`SOLDR_SESSION_ENDPOINT_PATH_ENV`] first (tests / diagnostics), then
/// falls back to the deterministic [`daemon_session_endpoint_path`] so the
/// broker's SESSION relay can always reach the daemon at the advertised name.
/// Step 8 will prepend the broker-inherited-fd adopt path
/// (`broker_owned_bind::recover_from_env`, Unix only) ahead of the bind.
///
/// # Errors
///
/// Fails only if the resolved path cannot be bound (e.g. already in use).
pub(crate) fn resolve_session_listener(paths: &SoldrPaths) -> io::Result<Option<SessionListener>> {
    if let Some(path) = std::env::var_os(SOLDR_SESSION_ENDPOINT_PATH_ENV) {
        let path = path.to_string_lossy();
        if !path.is_empty() {
            return bind_session_listener(&path).map(Some);
        }
    }
    let path = daemon_session_endpoint_path(paths)?;
    bind_session_listener(&path).map(Some)
}

/// The tokio local-socket listener type served by the SESSION endpoint.
pub type SessionListener = interprocess::local_socket::tokio::Listener;

/// Bind a tokio local-socket SESSION listener at `socket_path`.
///
/// Resolves the platform local-socket name the same way running-process's broker
/// SESSION bind does (Unix filesystem path, Windows namespaced pipe), so the
/// daemon endpoint and the broker's relay dial name the same path identically.
pub fn bind_session_listener(socket_path: &str) -> io::Result<SessionListener> {
    use interprocess::local_socket::ListenerOptions;

    // Unix local sockets are filesystem entries. The broker's runtime namespace
    // may not exist in a clean container, so create its parent before binding.
    // Windows named pipes do not have a filesystem parent.
    #[cfg(unix)]
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let name = local_session_name(socket_path)?;
    ListenerOptions::new().name(name).create_tokio()
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

/// Accept SESSION connections on `listener` and serve each through
/// [`serve_session_connection`] on its own task.
///
/// The compile service and mux are cheaply shared (`SoldrZccacheService` is an
/// `Arc` inside; the mux is immutable), so per-connection spawning never
/// serializes concurrent compiles. A per-connection error is logged and the
/// loop continues; only a fatal `accept()` error ends the loop.
///
/// # Errors
///
/// Returns the first fatal `accept()` error (the listener is unusable).
pub async fn serve_session_endpoint(
    listener: SessionListener,
    service: Arc<SoldrZccacheService>,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> io::Result<()> {
    use interprocess::local_socket::tokio::prelude::*;

    loop {
        let stream = listener.accept().await?;
        let service = Arc::clone(&service);
        let paths = paths.clone();
        let mux = Arc::clone(&mux);
        tokio::spawn(async move {
            if let Err(err) = serve_session_connection(stream, &service, &paths, &mux).await {
                eprintln!("soldr-daemon: SESSION endpoint connection ended: {err}");
            }
        });
    }
}

#[cfg(test)]
mod tests;
