//! Streaming attach handler for the daemon-owned PTY sessions
//! (issue #130 milestone 2).
//!
//! After the daemon server decodes a request of type `ATTACH_PTY_SESSION`, it
//! stops the normal request/response loop and
//! hands the framed transport to [`run_attach_stream`]. From that point the
//! same socket carries `PtyStreamFrame` (daemon → client) and
//! `PtyInputFrame` (client → daemon) messages until either side disconnects
//! or the session ends. The framing (length-prefixed big-endian u32) is
//! unchanged; only the payload type differs.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, warn};

use crate::proto::daemon::{
    pty_input_frame::Frame as InputOneof, pty_stream_frame::Frame as StreamOneof,
    AttachPtySessionRequest, AttachPtySessionResponse, DaemonResponse, PtyInputFrame,
    PtyStreamFrame, StatusCode,
};

use crate::daemon::handlers::DaemonState;
use crate::daemon::pty_sessions::{AttachError, AttachmentEnded, OutboundFrame};
use crate::terminal_graphics::{
    terminal_graphics_capabilities_from_proto, TerminalGraphicsCapabilities,
};

/// Drive the attach stream for the lifetime of one client connection.
///
/// Returns once the stream ends (client disconnect, detach request, session
/// exit, or terminate). The framed transport is consumed; the caller should
/// drop the connection afterwards.
pub async fn run_attach_stream<T>(
    mut framed: Framed<T, LengthDelimitedCodec>,
    request_id: u64,
    attach_req: AttachPtySessionRequest,
    state: Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    // -----------------------------------------------------------------------
    // 1. Look up the session and install the attachment.
    // -----------------------------------------------------------------------
    let session = match state.pty_sessions.get(&attach_req.session_id) {
        Some(s) => s,
        None => {
            let resp = error_attach_response(
                request_id,
                StatusCode::NotFound,
                format!("session not found: {}", attach_req.session_id),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
    };

    let rows = if attach_req.rows == 0 {
        session.rows()
    } else {
        attach_req.rows as u16
    };
    let cols = if attach_req.cols == 0 {
        session.cols()
    } else {
        attach_req.cols as u16
    };

    let (handle, backlog, bytes_dropped) = match session.attach_with_terminal_info(
        attach_req.steal,
        rows,
        cols,
        attach_req.is_tty,
        attach_req.term.clone(),
        attach_req
            .graphics_capabilities
            .as_ref()
            .map(terminal_graphics_capabilities_from_proto)
            .unwrap_or_else(TerminalGraphicsCapabilities::unknown),
    ) {
        Ok(h) => h,
        Err(AttachError::AlreadyAttached) => {
            let resp = error_attach_response(
                request_id,
                StatusCode::AlreadyAttached,
                "session already has an attached client".into(),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
        Err(AttachError::SessionExited(state)) => {
            let resp = error_attach_response(
                request_id,
                StatusCode::NotFound,
                format!(
                    "session has already exited (exit_code={}, at={})",
                    state.exit_code, state.exited_at_unix
                ),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
    };

    // -----------------------------------------------------------------------
    // 2. Send the AttachPtySessionResponse with the initial backlog.
    // -----------------------------------------------------------------------
    let response = DaemonResponse {
        request_id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        attach_pty_session: Some(AttachPtySessionResponse {
            stream_endpoint: String::new(),
            backlog: backlog.clone(),
            backlog_truncated: bytes_dropped > 0,
            bytes_missed: bytes_dropped,
        }),
        ..Default::default()
    };
    send_response(&mut framed, &response).await?;

    // -----------------------------------------------------------------------
    // 3. Stream loop. Two pumps:
    //    - PTY output (handle.receiver) -> PtyStreamFrame -> socket
    //    - PtyInputFrame from socket -> session
    //
    // Either pump exiting tears down the attachment.
    // -----------------------------------------------------------------------
    let session_for_cleanup = Arc::clone(&session);
    let mut receiver = handle.receiver;

    loop {
        tokio::select! {
            // Daemon -> client
            outbound = receiver.recv() => {
                let frame = match outbound {
                    Some(f) => f,
                    None => {
                        // Receiver closed — the session removed the attachment slot.
                        debug!(session_id = %session.id, "outbound channel closed");
                        break;
                    }
                };
                let stream_frame = encode_outbound(frame);
                let (terminal, frame_bytes) = stream_frame;
                let bytes = frame_bytes.encode_to_vec();
                if let Err(e) = framed.send(Bytes::from(bytes)).await {
                    warn!(session_id = %session.id, error = %e, "send to attached client failed");
                    break;
                }
                if terminal {
                    debug!(session_id = %session.id, "terminal stream frame sent; closing");
                    break;
                }
            }
            // Client -> daemon
            inbound = framed.next() => {
                let bytes = match inbound {
                    Some(Ok(b)) => b,
                    Some(Err(e)) => {
                        warn!(session_id = %session.id, error = %e, "input frame decode error");
                        break;
                    }
                    None => {
                        debug!(session_id = %session.id, "client disconnected mid-stream");
                        break;
                    }
                };
                let input = match PtyInputFrame::decode(bytes.as_ref()) {
                    Ok(f) => f,
                    Err(e) => {
                        warn!(session_id = %session.id, error = %e, "PtyInputFrame decode error");
                        continue;
                    }
                };
                if apply_input_frame(input, &session) {
                    // Detach requested by client.
                    debug!(session_id = %session.id, "client requested detach");
                    break;
                }
            }
        }
    }

    // Clear the attachment slot only if it still belongs to us (a steal
    // would have already replaced it).
    if session_for_cleanup.is_attached() {
        session_for_cleanup.clear_attachment();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

/// Encode an [`OutboundFrame`] into a `PtyStreamFrame` plus a "is this the
/// final frame on this connection" flag.
fn encode_outbound(frame: OutboundFrame) -> (bool, PtyStreamFrame) {
    match frame {
        OutboundFrame::Output(bytes) => (
            false,
            PtyStreamFrame {
                frame: Some(StreamOneof::Output(bytes)),
            },
        ),
        OutboundFrame::MissedBytes(n) => (
            false,
            PtyStreamFrame {
                frame: Some(StreamOneof::MissedBytes(n)),
            },
        ),
        OutboundFrame::Exit(code) => (
            true,
            PtyStreamFrame {
                frame: Some(StreamOneof::ExitCode(code)),
            },
        ),
        OutboundFrame::Ended(end) => {
            let oneof = match end {
                AttachmentEnded::Stolen => StreamOneof::StolenBy("peer".to_string()),
                AttachmentEnded::SessionExited => StreamOneof::Error("session exited".into()),
                AttachmentEnded::Terminated => {
                    StreamOneof::Error("session terminated by request".into())
                }
                AttachmentEnded::Detached => StreamOneof::Error("detached".into()),
            };
            (true, PtyStreamFrame { frame: Some(oneof) })
        }
    }
}

/// Apply a client → daemon input frame. Returns `true` if the client
/// requested detach and the streaming loop should terminate cleanly.
fn apply_input_frame(
    input: PtyInputFrame,
    session: &Arc<crate::daemon::pty_sessions::OwnedPtySession>,
) -> bool {
    let Some(kind) = input.frame else {
        return false;
    };
    match kind {
        InputOneof::Input(bytes) => {
            if let Err(e) = session.write_input(&bytes) {
                warn!(session_id = %session.id, error = %e, "PTY write_input failed");
            }
            false
        }
        InputOneof::Resize(resize) => {
            let rows = resize.rows as u16;
            let cols = resize.cols as u16;
            if let Err(e) = session.resize(rows, cols) {
                warn!(session_id = %session.id, error = %e, "PTY resize failed");
            }
            false
        }
        InputOneof::Interrupt(true) => {
            if let Err(e) = session.send_interrupt() {
                warn!(session_id = %session.id, error = %e, "PTY send_interrupt failed");
            }
            false
        }
        InputOneof::Interrupt(false) => false,
        InputOneof::Detach(true) => true,
        InputOneof::Detach(false) => false,
    }
}

fn error_attach_response(request_id: u64, code: StatusCode, message: String) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code as i32,
        message,
        attach_pty_session: Some(AttachPtySessionResponse::default()),
        ..Default::default()
    }
}

async fn send_response<T>(
    framed: &mut Framed<T, LengthDelimitedCodec>,
    response: &DaemonResponse,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let encoded = response.encode_to_vec();
    framed.send(Bytes::from(encoded)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_outbound_maps_non_terminal_frames() {
        let (terminal, output) = encode_outbound(OutboundFrame::Output(b"abc".to_vec()));
        assert!(!terminal);
        assert!(matches!(
            output.frame,
            Some(StreamOneof::Output(bytes)) if bytes == b"abc"
        ));

        let (terminal, missed) = encode_outbound(OutboundFrame::MissedBytes(42));
        assert!(!terminal);
        assert!(matches!(missed.frame, Some(StreamOneof::MissedBytes(42))));
    }

    #[test]
    fn encode_outbound_maps_terminal_frames() {
        let (terminal, exit) = encode_outbound(OutboundFrame::Exit(7));
        assert!(terminal);
        assert!(matches!(exit.frame, Some(StreamOneof::ExitCode(7))));

        let (terminal, stolen) = encode_outbound(OutboundFrame::Ended(AttachmentEnded::Stolen));
        assert!(terminal);
        assert!(matches!(
            stolen.frame,
            Some(StreamOneof::StolenBy(peer)) if peer == "peer"
        ));

        let (terminal, exited) =
            encode_outbound(OutboundFrame::Ended(AttachmentEnded::SessionExited));
        assert!(terminal);
        assert!(matches!(
            exited.frame,
            Some(StreamOneof::Error(message)) if message == "session exited"
        ));

        let (terminal, terminated) =
            encode_outbound(OutboundFrame::Ended(AttachmentEnded::Terminated));
        assert!(terminal);
        assert!(matches!(
            terminated.frame,
            Some(StreamOneof::Error(message)) if message == "session terminated by request"
        ));

        let (terminal, detached) = encode_outbound(OutboundFrame::Ended(AttachmentEnded::Detached));
        assert!(terminal);
        assert!(matches!(
            detached.frame,
            Some(StreamOneof::Error(message)) if message == "detached"
        ));
    }

    #[test]
    fn error_attach_response_uses_attach_payload() {
        let response = error_attach_response(99, StatusCode::NotFound, "missing".into());

        assert_eq!(response.request_id, 99);
        assert_eq!(response.code, StatusCode::NotFound as i32);
        assert_eq!(response.message, "missing");
        assert!(response.attach_pty_session.is_some());
    }
}
