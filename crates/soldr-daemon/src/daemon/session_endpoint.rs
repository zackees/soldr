//! Daemon SESSION `0x5350` per-connection handler (soldr#2388 Step 6d / #2386
//! Option A).
//!
//! The audit correction (soldr#2365 comment 5233624686) rules #2386 **Option
//! A**: soldr-daemon serves SESSION `0x5350` on the **separate broker-facing
//! backend endpoint** the broker binds and hands over, running the codec-bridge
//! [`serve_session_compile`](crate::daemon::session_serve::serve_session_compile)
//! behind it. `handle_connection` (the private control endpoint) is unchanged — its
//! `Payload{0x5350} → drain_then_close` stays as the defensive default.
//!
//! That endpoint carries three traffic kinds (see running-process
//! `backend_sdk::mux`): the daemon's control wire (none here — SESSION is a
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
use tokio::sync::watch;

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{BackendEndpointMux, LegacyClassification, MuxPoll};
use running_process::broker::protocol::SESSION_PAYLOAD_PROTOCOL;

use crate::core::SoldrPaths;
use crate::daemon::session_serve::serve_session_compile;
use crate::zccache_embedded::SoldrZccacheService;

/// Concrete legacy detector type of the SESSION mux — a plain `fn` pointer so
/// the mux (and therefore the accept-loop `Arc<..>`) has a nameable type.
pub type SessionMux = BackendEndpointMux<fn(&[u8]) -> LegacyClassification>;

type CompileServiceResult = Result<Arc<SoldrZccacheService>, Arc<str>>;

/// Awaitable publication point for the daemon's embedded compile service.
///
/// The SESSION listener is intentionally started before zccache and the
/// daemon database finish initializing. Backend-handle probes therefore stay
/// responsive during startup, while a real compile waits here instead of
/// making the broker mistake slow initialization for a dead daemon.
#[derive(Clone)]
pub(crate) struct CompileServiceReadiness {
    receiver: watch::Receiver<Option<CompileServiceResult>>,
}

pub(crate) struct CompileServicePublisher {
    sender: watch::Sender<Option<CompileServiceResult>>,
}

impl CompileServiceReadiness {
    pub(crate) fn pending() -> (Self, CompileServicePublisher) {
        let (sender, receiver) = watch::channel(None);
        (Self { receiver }, CompileServicePublisher { sender })
    }

    pub(crate) fn ready(service: Arc<SoldrZccacheService>) -> Self {
        let (sender, receiver) = watch::channel(Some(Ok(service)));
        drop(sender);
        Self { receiver }
    }

    async fn wait(&self) -> io::Result<Arc<SoldrZccacheService>> {
        let mut receiver = self.receiver.clone();
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result.map_err(|message| io::Error::other(message.to_string()));
            }
            receiver.changed().await.map_err(|_| {
                io::Error::other("embedded compile service initialization ended unexpectedly")
            })?;
        }
    }
}

impl CompileServicePublisher {
    pub(crate) fn publish(&self, result: CompileServiceResult) {
        self.sender.send_replace(Some(result));
    }
}

/// Names an explicit soldr-owned SESSION endpoint socket path.
///
/// The broker launcher assigns this route-local endpoint when it creates the
/// daemon. It is mandatory for broker-owned production startup; tests may set
/// it explicitly to exercise the endpoint without a broker process.
pub const SOLDR_SESSION_ENDPOINT_PATH_ENV: &str = "SOLDR_SESSION_ENDPOINT_PATH";
/// Broker-assigned private endpoint for the daemon's request/response control
/// protocol. Only the broker and daemon receive this value; user-facing
/// clients tunnel through the stable broker endpoint.
pub const SOLDR_CONTROL_ENDPOINT_PATH_ENV: &str = "SOLDR_DAEMON_CONTROL_ENDPOINT_PATH";

pub fn private_control_endpoint_from_session(session_endpoint: &str) -> String {
    sibling_endpoint(session_endpoint, ".control.sock")
}

fn sibling_endpoint(session_endpoint: &str, suffix: &str) -> String {
    session_endpoint.strip_suffix(".session.sock").map_or_else(
        || format!("{session_endpoint}{suffix}"),
        |base| format!("{base}{suffix}"),
    )
}

pub fn resolved_control_endpoint_path(_paths: &SoldrPaths) -> io::Result<std::path::PathBuf> {
    let logical = std::env::var_os(SOLDR_CONTROL_ENDPOINT_PATH_ENV)
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "required daemon endpoint variable {SOLDR_CONTROL_ENDPOINT_PATH_ENV} is unset"
                ),
            )
        })?;
    Ok(runtime_control_endpoint_path(logical))
}

fn runtime_control_endpoint_path(logical: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        const PREFIX: &str = r"\\.\pipe\";
        let rendered = logical.to_string_lossy();
        if rendered.starts_with(PREFIX) {
            logical
        } else {
            std::path::PathBuf::from(format!("{PREFIX}{rendered}"))
        }
    }
    #[cfg(not(windows))]
    {
        logical
    }
}

/// The mux for the SESSION endpoint: serves the `0x5350` lane and declares
/// **no** daemon control wire.
///
/// Distinct from `backend_handle_adoption::soldr_backend_endpoint_mux`, which is
/// built with `served = &[]` for the private control endpoint and MUST NOT be reused —
/// serving `0x5350` there would change `handle_connection`, which the audit
/// correction forbids.
pub fn soldr_session_endpoint_mux(daemon: DaemonProcess) -> SessionMux {
    BackendEndpointMux::new(daemon, &[SESSION_PAYLOAD_PROTOCOL], classify_never_legacy)
}

/// The SESSION endpoint has no daemon control wire: every framed connection is either a
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
/// - a control-wire verdict → an error (the SESSION endpoint serves no control
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
    service: &CompileServiceReadiness,
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
                let service = service.wait().await?;
                return serve_session_compile(replay, &service, paths).await;
            }
            MuxPoll::Legacy => {
                return Err(io::Error::other(
                    "unexpected daemon-control frame on the SESSION endpoint",
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

pub fn resolved_session_endpoint_path(_paths: &SoldrPaths) -> io::Result<String> {
    std::env::var_os(SOLDR_SESSION_ENDPOINT_PATH_ENV)
        .filter(|path| !path.is_empty())
        .map(|path| path.to_string_lossy().into_owned())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "required daemon endpoint variable {SOLDR_SESSION_ENDPOINT_PATH_ENV} is unset"
                ),
            )
        })
}

/// Deterministic daemon-side handoff endpoint used only by the broker's
/// SCM_RIGHTS/DuplicateHandle transport. Clients always dial the one stable
/// broker listener.
pub fn handoff_endpoint_path(session_endpoint: &str) -> String {
    sibling_endpoint(session_endpoint, ".handoff.sock")
}

pub(crate) fn resolve_handoff_listener(paths: &SoldrPaths) -> io::Result<SessionListener> {
    let session_endpoint = resolved_session_endpoint_path(paths)?;
    bind_session_listener(&handoff_endpoint_path(&session_endpoint))
}

/// Resolve the SESSION endpoint listener the daemon serves.
///
/// Requires the broker-provided executable-path-derived endpoint.
///
/// # Errors
///
/// Fails only if the resolved path cannot be bound (e.g. already in use).
pub(crate) fn resolve_session_listener(paths: &SoldrPaths) -> io::Result<Option<SessionListener>> {
    let path = resolved_session_endpoint_path(paths)?;
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
        running_process::broker::secure_dir::ensure_private_dir(parent)?;
    }

    let name = local_session_name(socket_path)?;
    let options = ListenerOptions::new().name(name).reclaim_name(false);
    #[cfg(unix)]
    let options = {
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
        options.mode(0o600)
    };
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")
            .map_err(io::Error::other)?;
        let descriptor = SecurityDescriptor::deserialize(&sddl).map_err(io::Error::other)?;
        options.security_descriptor(descriptor)
    };
    let first = options.create_tokio();
    #[cfg(unix)]
    {
        match first {
            Ok(listener) => Ok(listener),
            Err(err)
                if running_process::broker::server::singleton_bind::is_already_bound_error(&err)
                    && running_process::broker::server::singleton_bind::unix_socket_path_is_stale(
                        socket_path,
                    ) =>
            {
                let _ = std::fs::remove_file(socket_path);
                let retry_name = local_session_name(socket_path)?;
                let options = ListenerOptions::new()
                    .name(retry_name)
                    .reclaim_name(false);
                use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
                options.mode(0o600).create_tokio()
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(not(unix))]
    first
}

fn local_session_name(socket_path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    running_process::broker::server::singleton_bind::wrap_socket_name(socket_path)
        .map_err(io::Error::other)
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
    serve_session_endpoint_with_readiness(
        listener,
        CompileServiceReadiness::ready(service),
        paths,
        mux,
    )
    .await
}

pub(crate) async fn serve_session_endpoint_with_readiness(
    listener: SessionListener,
    service: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> io::Result<()> {
    use interprocess::local_socket::tokio::prelude::*;

    loop {
        let stream = listener.accept().await?;
        if !session_peer_is_current_user(&stream) {
            tracing::warn!(target: "soldr::daemon", "rejected foreign SESSION endpoint peer");
            continue;
        }
        let service = service.clone();
        let paths = paths.clone();
        let mux = Arc::clone(&mux);
        tokio::spawn(async move {
            if let Err(err) = serve_session_connection(stream, &service, &paths, &mux).await {
                eprintln!("soldr-daemon: SESSION endpoint connection ended: {err}");
            }
        });
    }
}

pub(crate) fn spawn_session_endpoint_servers(
    session_listener: SessionListener,
    handoff_listener: SessionListener,
    readiness: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let handoff_readiness = readiness.clone();
    let handoff_paths = paths.clone();
    let handoff_mux = Arc::clone(&mux);
    let session = tokio::spawn(async move {
        if let Err(error) =
            serve_session_endpoint_with_readiness(session_listener, readiness, paths, mux).await
        {
            tracing::warn!(target: "soldr::daemon", "SESSION endpoint serve ended: {error}");
        }
    });
    let handoff = tokio::spawn(async move {
        if let Err(error) = serve_handoff_endpoint_with_readiness(
            handoff_listener,
            handoff_readiness,
            handoff_paths,
            handoff_mux,
        )
        .await
        {
            tracing::warn!(target: "soldr::daemon", "handoff endpoint serve ended: {error}");
        }
    });
    (session, handoff)
}

/// Accept broker-to-daemon connection handoffs and dispatch every accepted
/// client connection through the same SESSION handler as the proxy path.
pub(crate) async fn serve_handoff_endpoint_with_readiness(
    listener: SessionListener,
    service: CompileServiceReadiness,
    paths: SoldrPaths,
    mux: Arc<SessionMux>,
) -> io::Result<()> {
    use interprocess::local_socket::tokio::prelude::*;

    let expected_service_name =
        std::env::var(running_process::broker::server::BACKEND_ENV_SERVICE_NAME)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME.to_string()
            });

    loop {
        let control = listener.accept().await?;
        if !session_peer_is_current_user(&control) {
            tracing::warn!(target: "soldr::daemon", "rejected foreign handoff endpoint peer");
            continue;
        }
        let service = service.clone();
        let paths = paths.clone();
        let mux = Arc::clone(&mux);
        let expected_service_name = expected_service_name.clone();
        tokio::spawn(async move {
            match receive_handed_off_session(control, &expected_service_name).await {
                Ok(client) => {
                    if let Err(error) =
                        serve_session_connection(client, &service, &paths, &mux).await
                    {
                        eprintln!("soldr-daemon: handed-off SESSION ended: {error}");
                    }
                }
                Err(error) => {
                    eprintln!("soldr-daemon: rejected broker connection handoff: {error}");
                }
            }
        });
    }
}

fn session_peer_is_current_user(stream: &interprocess::local_socket::tokio::Stream) -> bool {
    use running_process::broker::server::{connection, PeerCredentialPolicy};
    let Some(policy) = PeerCredentialPolicy::current_user() else {
        return false;
    };
    connection::peer_identity_from_tokio_stream(stream).is_ok_and(|peer| policy.allows(&peer))
}

async fn read_handoff_offer_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    expected_service_name: &str,
) -> io::Result<running_process::broker::protocol::HandoffOffer> {
    use prost::Message as _;
    use running_process::broker::protocol::{
        Frame, FrameKind, HandoffOffer, HANDOFF_PAYLOAD_PROTOCOL,
    };

    let body = read_envelope_body(stream).await?;
    let frame = Frame::decode(body.as_slice()).map_err(io::Error::other)?;
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request)
        || frame.payload_protocol != HANDOFF_PAYLOAD_PROTOCOL
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff control connection did not carry a HandoffOffer request",
        ));
    }
    let offer = HandoffOffer::decode(frame.payload.as_slice()).map_err(io::Error::other)?;
    if offer.correlation_id != frame.request_id
        || offer.service_name != expected_service_name
        || offer.token.len() != running_process::broker::server::HANDOFF_TOKEN_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff offer identity, correlation, or token was invalid",
        ));
    }
    Ok(offer)
}

async fn accept_handoff_offer_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    offer: &running_process::broker::protocol::HandoffOffer,
) -> io::Result<()> {
    use prost::Message as _;
    use running_process::broker::protocol::HandoffAck;
    use tokio::io::AsyncWriteExt as _;

    let ack = HandoffAck {
        token: offer.token.clone(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: offer.correlation_id,
    };
    let frame = running_process::broker::server::handoff::handoff_ack_frame(&ack);
    let bytes =
        running_process::broker::protocol::encode_framed(&frame).map_err(io::Error::other)?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

async fn read_envelope_body(
    stream: &mut interprocess::local_socket::tokio::Stream,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    let mut header = [0_u8; 5];
    tokio::time::timeout(
        running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE,
        stream.read_exact(&mut header),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handoff offer header timed out"))??;
    if header[0] != running_process::broker::protocol::ENVELOPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff offer used the wrong envelope version",
        ));
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if len > running_process::broker::protocol::MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff offer exceeded the frame limit",
        ));
    }
    let mut body = vec![0_u8; len];
    tokio::time::timeout(
        running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE,
        stream.read_exact(&mut body),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handoff offer body timed out"))??;
    Ok(body)
}

#[cfg(unix)]
async fn receive_handed_off_session(
    control: interprocess::local_socket::tokio::Stream,
    expected_service_name: &str,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    let (mut control, client_fd, transport_token) =
        tokio::task::spawn_blocking(move || receive_unix_descriptor(control))
            .await
            .map_err(|error| io::Error::other(format!("handoff receive task failed: {error}")))??;
    let offer = read_handoff_offer_async(&mut control, expected_service_name).await?;
    if offer.token.as_slice() != transport_token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SCM_RIGHTS token did not match the framed handoff offer",
        ));
    }
    let stream = interprocess::os::unix::uds_local_socket::tokio::Stream::try_from(client_fd)?;
    // A positive ACK transfers ownership. Do not send it until the received
    // descriptor is a usable Tokio SESSION stream.
    accept_handoff_offer_async(&mut control, &offer).await?;
    Ok(stream.into())
}

#[cfg(unix)]
fn receive_unix_descriptor(
    control: interprocess::local_socket::tokio::Stream,
) -> io::Result<(
    interprocess::local_socket::tokio::Stream,
    std::os::fd::OwnedFd,
    [u8; running_process::broker::server::HANDOFF_TOKEN_BYTES],
)> {
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};

    let interprocess::local_socket::tokio::Stream::UdSocket(socket) = &control;
    let socket_fd = socket.as_fd().as_raw_fd();
    let mut token = [0_u8; running_process::broker::server::HANDOFF_TOKEN_BYTES];
    let mut iov = libc::iovec {
        iov_base: token.as_mut_ptr().cast(),
        iov_len: token.len(),
    };
    let mut ancillary =
        vec![0_u8; unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize }];
    let deadline =
        std::time::Instant::now() + running_process::broker::server::DEFAULT_HANDOFF_ACK_DEADLINE;
    loop {
        let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = ancillary.as_mut_ptr().cast();
        message.msg_controllen = ancillary.len() as _;
        let received = unsafe { libc::recvmsg(socket_fd, &mut message, libc::MSG_DONTWAIT) };
        if received < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            return Err(error);
        }
        if received as usize != token.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short SCM_RIGHTS token prelude",
            ));
        }
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if header.is_null()
            || unsafe {
                (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS
            }
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff prelude omitted SCM_RIGHTS",
            ));
        }
        let received_fd = unsafe { *libc::CMSG_DATA(header).cast::<libc::c_int>() };
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(received_fd) };
        let descriptor_flags = unsafe { libc::fcntl(received_fd, libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe {
                libc::fcntl(
                    received_fd,
                    libc::F_SETFD,
                    descriptor_flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(io::Error::last_os_error());
        }
        return Ok((control, owned, token));
    }
}

#[cfg(windows)]
async fn receive_handed_off_session(
    mut control: interprocess::local_socket::tokio::Stream,
    expected_service_name: &str,
) -> io::Result<interprocess::local_socket::tokio::Stream> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};

    let offer = read_handoff_offer_async(&mut control, expected_service_name).await?;
    if offer.handle_value == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DuplicateHandle offer carried a null handle",
        ));
    }
    let owned = unsafe { OwnedHandle::from_raw_handle(offer.handle_value as *mut _) };
    let stream =
        interprocess::os::windows::named_pipe::local_socket::tokio::Stream::try_from(owned)
            .map_err(io::Error::other)?;
    // A positive ACK transfers ownership. Do not send it until the duplicated
    // handle is a usable Tokio SESSION stream.
    accept_handoff_offer_async(&mut control, &offer).await?;
    Ok(stream.into())
}

#[cfg(test)]
mod tests;
