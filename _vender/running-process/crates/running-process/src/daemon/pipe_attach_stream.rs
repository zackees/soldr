//! Streaming attach handler for pipe-backed sessions (#130 milestone 3).
//!
//! Parallel to [`crate::daemon::attach_stream`] but for stdout/stderr of a
//! pipe-backed session. The client sends an `AttachPipeStreamRequest`;
//! after the response is delivered, the daemon pushes `PipeStreamFrame`
//! messages until EOF, terminate, exit, steal, or client disconnect.
//! Pipe streams are one-way (daemon → client); client-side stdin is the
//! separate `WritePipeStdin` RPC.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, warn};

use crate::proto::daemon::{
    pipe_stream_frame::Frame as PipeStreamOneof, AttachPipeStreamRequest, AttachPipeStreamResponse,
    DaemonResponse, PipeStreamFrame, PipeStreamKind, StatusCode,
};

use crate::daemon::handlers::DaemonState;
use crate::daemon::pipe_sessions::{PipeAttachError, PipeStreamSelect};
use crate::daemon::pty_sessions::{AttachmentEnded, OutboundFrame};

pub async fn run_pipe_attach_stream<T>(
    mut framed: Framed<T, LengthDelimitedCodec>,
    request_id: u64,
    attach_req: AttachPipeStreamRequest,
    state: Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let stream = match PipeStreamKind::try_from(attach_req.stream) {
        Ok(PipeStreamKind::Stdout) => PipeStreamSelect::Stdout,
        Ok(PipeStreamKind::Stderr) => PipeStreamSelect::Stderr,
        _ => {
            let resp = error_attach_response(
                request_id,
                StatusCode::InvalidArgument,
                "stream must be PIPE_STREAM_KIND_STDOUT or PIPE_STREAM_KIND_STDERR".into(),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
    };

    let session = match state.pipe_sessions.get(&attach_req.session_id) {
        Some(s) => s,
        None => {
            let resp = error_attach_response(
                request_id,
                StatusCode::NotFound,
                format!("pipe session not found: {}", attach_req.session_id),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
    };

    let (handle, backlog, dropped) = match session.attach_stream(stream, attach_req.steal) {
        Ok(h) => h,
        Err(PipeAttachError::AlreadyAttached) => {
            let resp = error_attach_response(
                request_id,
                StatusCode::AlreadyAttached,
                format!(
                    "pipe session stream {:?} already has an attached client",
                    stream
                ),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
        Err(PipeAttachError::SessionExited(s)) => {
            let resp = error_attach_response(
                request_id,
                StatusCode::NotFound,
                format!(
                    "pipe session has already exited (exit_code={}, at={})",
                    s.exit_code, s.exited_at_unix
                ),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
        Err(PipeAttachError::StreamUnavailable) => {
            let resp = error_attach_response(
                request_id,
                StatusCode::InvalidArgument,
                "requested stream is not available on this session (likely merged into stdout)"
                    .into(),
            );
            send_response(&mut framed, &resp).await?;
            return Ok(());
        }
    };

    let response = DaemonResponse {
        request_id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        attach_pipe_stream: Some(AttachPipeStreamResponse {
            backlog: backlog.clone(),
            backlog_truncated: dropped > 0,
            bytes_missed: dropped,
        }),
        ..Default::default()
    };
    send_response(&mut framed, &response).await?;

    let session_for_cleanup = Arc::clone(&session);
    let mut receiver = handle.receiver;

    loop {
        tokio::select! {
            outbound = receiver.recv() => {
                let frame = match outbound {
                    Some(f) => f,
                    None => {
                        debug!(session_id = %session.id, "pipe outbound channel closed");
                        break;
                    }
                };
                let (terminal, pipe_frame) = encode_pipe_frame(frame);
                let bytes = pipe_frame.encode_to_vec();
                if let Err(e) = framed.send(Bytes::from(bytes)).await {
                    warn!(session_id = %session.id, error = %e, "send to pipe attach client failed");
                    break;
                }
                if terminal {
                    break;
                }
            }
            // Pipe attach is one-way; receiving anything from the client
            // (other than disconnect) is unexpected. We still poll the
            // socket so a client disconnect breaks the loop promptly.
            inbound = framed.next() => {
                match inbound {
                    Some(Ok(_unexpected)) => {
                        // Silently drop unexpected client frames.
                    }
                    Some(Err(e)) => {
                        warn!(session_id = %session.id, error = %e, "pipe attach client frame error");
                        break;
                    }
                    None => {
                        debug!(session_id = %session.id, "pipe attach client disconnected");
                        break;
                    }
                }
            }
        }
    }

    if session_for_cleanup.is_attached(stream) {
        session_for_cleanup.clear_attachment(stream);
    }
    Ok(())
}

fn encode_pipe_frame(frame: OutboundFrame) -> (bool, PipeStreamFrame) {
    match frame {
        OutboundFrame::Output(bytes) => (
            false,
            PipeStreamFrame {
                frame: Some(PipeStreamOneof::Bytes(bytes)),
            },
        ),
        OutboundFrame::MissedBytes(n) => (
            false,
            PipeStreamFrame {
                frame: Some(PipeStreamOneof::MissedBytes(n)),
            },
        ),
        OutboundFrame::Exit(code) => (
            true,
            PipeStreamFrame {
                frame: Some(PipeStreamOneof::ExitCode(code)),
            },
        ),
        OutboundFrame::Ended(AttachmentEnded::Stolen) => (
            true,
            PipeStreamFrame {
                frame: Some(PipeStreamOneof::StolenBy("peer".to_string())),
            },
        ),
        OutboundFrame::Ended(AttachmentEnded::Detached) => (
            true,
            PipeStreamFrame {
                frame: Some(PipeStreamOneof::Eof(true)),
            },
        ),
        OutboundFrame::Ended(end) => {
            let msg = match end {
                AttachmentEnded::Terminated => "session terminated by request",
                AttachmentEnded::SessionExited => "session exited",
                AttachmentEnded::Detached | AttachmentEnded::Stolen => unreachable!(),
            };
            (
                true,
                PipeStreamFrame {
                    frame: Some(PipeStreamOneof::Error(msg.to_string())),
                },
            )
        }
    }
}

fn error_attach_response(request_id: u64, code: StatusCode, message: String) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code as i32,
        message,
        attach_pipe_stream: Some(AttachPipeStreamResponse::default()),
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
    fn encode_pipe_frame_maps_non_terminal_frames() {
        let (terminal, output) = encode_pipe_frame(OutboundFrame::Output(b"abc".to_vec()));
        assert!(!terminal);
        assert!(matches!(
            output.frame,
            Some(PipeStreamOneof::Bytes(bytes)) if bytes == b"abc"
        ));

        let (terminal, missed) = encode_pipe_frame(OutboundFrame::MissedBytes(42));
        assert!(!terminal);
        assert!(matches!(
            missed.frame,
            Some(PipeStreamOneof::MissedBytes(42))
        ));
    }

    #[test]
    fn encode_pipe_frame_maps_terminal_frames() {
        let (terminal, exit) = encode_pipe_frame(OutboundFrame::Exit(7));
        assert!(terminal);
        assert!(matches!(exit.frame, Some(PipeStreamOneof::ExitCode(7))));

        let (terminal, stolen) = encode_pipe_frame(OutboundFrame::Ended(AttachmentEnded::Stolen));
        assert!(terminal);
        assert!(matches!(
            stolen.frame,
            Some(PipeStreamOneof::StolenBy(peer)) if peer == "peer"
        ));

        let (terminal, detached) =
            encode_pipe_frame(OutboundFrame::Ended(AttachmentEnded::Detached));
        assert!(terminal);
        assert!(matches!(detached.frame, Some(PipeStreamOneof::Eof(true))));

        let (terminal, exited) =
            encode_pipe_frame(OutboundFrame::Ended(AttachmentEnded::SessionExited));
        assert!(terminal);
        assert!(matches!(
            exited.frame,
            Some(PipeStreamOneof::Error(message)) if message == "session exited"
        ));

        let (terminal, terminated) =
            encode_pipe_frame(OutboundFrame::Ended(AttachmentEnded::Terminated));
        assert!(terminal);
        assert!(matches!(
            terminated.frame,
            Some(PipeStreamOneof::Error(message)) if message == "session terminated by request"
        ));
    }

    #[test]
    fn error_attach_response_uses_pipe_attach_payload() {
        let response = error_attach_response(99, StatusCode::NotFound, "missing".into());

        assert_eq!(response.request_id, 99);
        assert_eq!(response.code, StatusCode::NotFound as i32);
        assert_eq!(response.message, "missing");
        assert!(response.attach_pipe_stream.is_some());
    }
}
