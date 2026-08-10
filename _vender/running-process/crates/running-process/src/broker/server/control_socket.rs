//! Shared broker control socket dispatch for Hello and admin frames.
//!
//! The v1 broker uses one local socket for both client Hello negotiation and
//! admin verbs. This module keeps the bounded synchronous serve helpers aligned
//! with that contract while the long-lived daemon loop is still being built.

use std::io::{Read, Write};
use std::num::NonZeroUsize;

use interprocess::local_socket::traits::{Listener, Stream as _};
use prost::Message;

use crate::broker::protocol::{
    read_frame, write_frame, AdminReply, ErrorCode, Frame, FramingError, HelloReply,
    MAX_HELLO_BYTES,
};

use super::admin::{handle_admin_frame, AdminFrameError, AdminSnapshot, ADMIN_PAYLOAD_PROTOCOL};
use super::connection::{
    bind_local_socket, peer_identity_from_stream, refused_reply, reply_for_framing_error,
    write_response_frame, BrokerConnectionError, HelloResponder, LocalSocketCleanup,
    PeerCredentialPolicy,
};
use super::deadline_stream::{hello_read_deadline, DeadlineStream};
use super::fd_pressure::{FdPressureDecision, FdPressureGuard};
use super::hello_handler::PeerIdentity;

/// Result of handling one control socket connection.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlSocketReply {
    /// Peer was rejected by credential policy before any bytes were read.
    DroppedPeer,
    /// The connection was handled as a Hello exchange.
    Hello(HelloReply),
    /// The connection was handled as an admin request.
    Admin(AdminReply),
}

/// Connection limit for a broker control-socket accept loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlSocketConnectionLimit {
    /// Accept exactly this many connections, then return.
    Bounded(NonZeroUsize),
    /// Continue accepting until the process exits or binding/accepting fails.
    Unbounded,
}

impl ControlSocketConnectionLimit {
    fn should_continue(self, accepted: usize) -> bool {
        match self {
            Self::Bounded(limit) => accepted < limit.get(),
            Self::Unbounded => true,
        }
    }
}

/// Handle one already-accepted broker control connection.
pub fn handle_control_connection_with_peer_policy<S, R, F>(
    stream: &mut S,
    hello_responder: &R,
    snapshot_provider: &F,
    peer: PeerIdentity,
    peer_policy: &PeerCredentialPolicy,
) -> Result<ControlSocketReply, ControlSocketError>
where
    S: Read + Write,
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot + ?Sized,
{
    handle_control_connection_with_peer_policy_and_fd_guard(
        stream,
        hello_responder,
        snapshot_provider,
        peer,
        peer_policy,
        None,
    )
}

/// Handle one already-accepted broker control connection, refusing Hello
/// frames with `ERROR_FD_PRESSURE` while `fd_guard` reports a demotion
/// (#390). Admin frames are always served so `status` can surface the
/// demoted state.
pub fn handle_control_connection_with_peer_policy_and_fd_guard<S, R, F>(
    stream: &mut S,
    hello_responder: &R,
    snapshot_provider: &F,
    peer: PeerIdentity,
    peer_policy: &PeerCredentialPolicy,
    fd_guard: Option<&FdPressureGuard>,
) -> Result<ControlSocketReply, ControlSocketError>
where
    S: Read + Write,
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot + ?Sized,
{
    if !peer_policy.allows(&peer) {
        return Ok(ControlSocketReply::DroppedPeer);
    }

    let request_bytes = match read_frame(stream) {
        Ok(bytes) => bytes,
        Err(err) => {
            let reply = reply_for_framing_error(&err);
            write_response_frame(stream, None, &reply)?;
            return Ok(ControlSocketReply::Hello(reply));
        }
    };

    let request_frame = match Frame::decode(request_bytes.as_slice()) {
        Ok(frame) => frame,
        Err(_) => {
            let reply = refused_reply(ErrorCode::ErrorPeerRejected, "malformed broker Frame", 0);
            write_response_frame(stream, None, &reply)?;
            return Ok(ControlSocketReply::Hello(reply));
        }
    };

    if request_frame.payload_protocol == ADMIN_PAYLOAD_PROTOCOL {
        let snapshot = snapshot_provider();
        let response_frame = handle_admin_frame(request_frame, &snapshot)?;
        let reply = write_admin_response_frame(stream, &response_frame)?;
        return Ok(ControlSocketReply::Admin(reply));
    }

    let reply = if request_bytes.len() > MAX_HELLO_BYTES {
        refused_reply(
            ErrorCode::ErrorPeerRejected,
            "initial Hello frame exceeds 64 KiB",
            0,
        )
    } else if let Some(guard) = fd_guard.filter(|guard| guard.is_demoted()) {
        guard.refusal_reply()
    } else {
        hello_responder.handle_frame(request_frame.clone(), peer)
    };
    write_response_frame(stream, Some(&request_frame), &reply)?;
    Ok(ControlSocketReply::Hello(reply))
}

/// Run a bounded local-socket accept loop that dispatches Hello and admin
/// frames on the same endpoint.
pub fn serve_control_socket_connections_with_policy<R, F>(
    socket_path: &str,
    hello_responder: &R,
    snapshot_provider: F,
    connection_count: usize,
    peer_policy: &PeerCredentialPolicy,
) -> Result<(), ControlSocketError>
where
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot,
{
    let Some(connection_count) = NonZeroUsize::new(connection_count) else {
        return Ok(());
    };

    serve_control_socket_connections_with_limit_and_policy(
        socket_path,
        hello_responder,
        snapshot_provider,
        ControlSocketConnectionLimit::Bounded(connection_count),
        peer_policy,
    )
}

/// Run a broker control-socket accept loop that dispatches Hello and admin
/// frames on the same endpoint.
pub fn serve_control_socket_connections_with_limit_and_policy<R, F>(
    socket_path: &str,
    hello_responder: &R,
    snapshot_provider: F,
    connection_limit: ControlSocketConnectionLimit,
    peer_policy: &PeerCredentialPolicy,
) -> Result<(), ControlSocketError>
where
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot,
{
    serve_control_socket_connections_with_limit_policy_and_post_hello(
        socket_path,
        hello_responder,
        snapshot_provider,
        connection_limit,
        peer_policy,
        |_stream, _reply| {},
    )
}

/// Run a broker control-socket accept loop with a post-Hello connection hook.
///
/// `post_hello` runs after a Hello reply has been written, with the client
/// connection still open. The production serve path uses it to attempt the
/// optional handle-passing handoff (#387) when negotiation issued a handoff
/// token; the hook must stay silent toward the client on failure.
pub fn serve_control_socket_connections_with_limit_policy_and_post_hello<R, F, H>(
    socket_path: &str,
    hello_responder: &R,
    snapshot_provider: F,
    connection_limit: ControlSocketConnectionLimit,
    peer_policy: &PeerCredentialPolicy,
    post_hello: H,
) -> Result<(), ControlSocketError>
where
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot,
    H: FnMut(&mut interprocess::local_socket::Stream, &HelloReply),
{
    let fd_guard = FdPressureGuard::default();
    serve_control_socket_connections_with_limit_policy_post_hello_and_fd_guard(
        socket_path,
        hello_responder,
        snapshot_provider,
        connection_limit,
        peer_policy,
        post_hello,
        &fd_guard,
    )
}

/// Run a broker control-socket accept loop with fd-pressure self-demotion
/// (#390).
///
/// `fd_guard` is shared so callers can surface the demotion state in admin
/// snapshots. When `accept()` fails with EMFILE/ENFILE the loop demotes
/// instead of returning the error: subsequent Hello connections receive a
/// structured `ERROR_FD_PRESSURE` refusal (admin verbs keep working), and
/// the guard recovers automatically after a streak of successful accepts.
#[allow(clippy::too_many_arguments)]
pub fn serve_control_socket_connections_with_limit_policy_post_hello_and_fd_guard<R, F, H>(
    socket_path: &str,
    hello_responder: &R,
    snapshot_provider: F,
    connection_limit: ControlSocketConnectionLimit,
    peer_policy: &PeerCredentialPolicy,
    mut post_hello: H,
    fd_guard: &FdPressureGuard,
) -> Result<(), ControlSocketError>
where
    R: HelloResponder + ?Sized,
    F: Fn() -> AdminSnapshot,
    H: FnMut(&mut interprocess::local_socket::Stream, &HelloReply),
{
    /// Back-off between accepts while demoted so a hard fd-exhaustion loop
    /// cannot spin the broker's CPU at 100%.
    const FD_PRESSURE_ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

    let listener = bind_local_socket(socket_path)?;
    let cleanup = LocalSocketCleanup(socket_path);
    let result = (|| {
        let mut accepted = 0;
        while connection_limit.should_continue(accepted) {
            let mut stream = match listener.accept() {
                Ok(stream) => {
                    fd_guard.on_accept_ok();
                    stream
                }
                Err(err) => {
                    let was_demoted = fd_guard.is_demoted();
                    if fd_guard.on_accept_error(&err) == FdPressureDecision::Demoted {
                        if !was_demoted {
                            eprintln!(
                                "running-process-broker: accept on {socket_path} demoted \
                                 under fd pressure: {err}"
                            );
                        }
                        accepted += 1;
                        std::thread::sleep(FD_PRESSURE_ACCEPT_BACKOFF);
                        continue;
                    }
                    return Err(BrokerConnectionError::Io(err).into());
                }
            };
            accepted += 1;
            let peer = peer_identity_from_stream(&stream)?;
            // Bound the Hello/admin read against a deadline (issue #590,
            // cluster G) so a silent or trickle peer cannot stall this
            // single-threaded accept loop. Set the accepted stream
            // nonblocking for the deadline-bounded handler, then restore
            // blocking mode for the post_hello callback below.
            let nonblocking_set = stream.set_nonblocking(true).is_ok();
            let reply_result = {
                let mut deadline_stream = DeadlineStream::new(&mut stream, hello_read_deadline());
                handle_control_connection_with_peer_policy_and_fd_guard(
                    &mut deadline_stream,
                    hello_responder,
                    &snapshot_provider,
                    peer.clone(),
                    peer_policy,
                    Some(fd_guard),
                )
            };
            if nonblocking_set {
                let _ = stream.set_nonblocking(false);
            }
            let reply = reply_result?;
            if reply == ControlSocketReply::DroppedPeer {
                eprintln!(
                    "running-process-broker: dropped connection on {socket_path} from peer \
                     pid={} uid_or_sid={:?}: credential policy refused",
                    peer.pid, peer.uid_or_sid
                );
            }
            if let ControlSocketReply::Hello(hello_reply) = &reply {
                post_hello(&mut stream, hello_reply);
            }
        }
        Ok(())
    })();
    drop(listener);
    drop(cleanup);
    result
}

fn write_admin_response_frame<W: Write>(
    writer: &mut W,
    response_frame: &Frame,
) -> Result<AdminReply, ControlSocketError> {
    let mut response_bytes = Vec::new();
    response_frame
        .encode(&mut response_bytes)
        .map_err(ControlSocketError::EncodeFrame)?;
    write_frame(writer, &response_bytes)?;
    AdminReply::decode(response_frame.payload.as_slice())
        .map_err(ControlSocketError::DecodeAdminReply)
}

/// Errors raised while dispatching a shared broker control socket frame.
#[derive(Debug, thiserror::Error)]
pub enum ControlSocketError {
    /// Hello/local-socket connection handling failed.
    #[error(transparent)]
    Connection(#[from] BrokerConnectionError),
    /// Frame read/write failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// Admin frame validation or dispatch failed.
    #[error(transparent)]
    AdminFrame(#[from] AdminFrameError),
    /// The response frame could not be encoded.
    #[error("failed to encode broker control response Frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// The admin response payload could not be decoded after dispatch.
    #[error("failed to decode admin reply payload: {0}")]
    DecodeAdminReply(prost::DecodeError),
}

#[cfg(test)]
mod cluster_g_tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct NeverReady;
    impl Read for NeverReady {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "never ready",
            ))
        }
    }
    impl Write for NeverReady {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn deadline_stream_read_times_out_on_silent_peer() {
        let mut inner = NeverReady;
        let mut ds = DeadlineStream::new(&mut inner, Instant::now() + Duration::from_millis(100));
        let mut buf = [0u8; 4];
        let start = Instant::now();
        let err = ds.read(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2), "must be bounded");
    }

    #[test]
    fn deadline_stream_passes_ready_data_through() {
        let data = b"hello";
        let mut cursor = std::io::Cursor::new(data.to_vec());
        let mut ds = DeadlineStream::new(&mut cursor, Instant::now() + Duration::from_secs(1));
        let mut buf = [0u8; 5];
        ds.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, data);
    }
}
