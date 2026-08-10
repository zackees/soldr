//! Detachable pipe session handlers (issue #130 milestone 3).
//!
//! Pipe parity for PTY sessions. Three handler shapes:
//!  - SPAWN_PIPE_SESSION: spawn a child with stdin/stdout/stderr piped.
//!  - LIST_PIPE_SESSIONS, DETACH_PIPE_STREAM, TERMINATE_PIPE_SESSION,
//!    WRITE_PIPE_STDIN: regular request/response RPCs.
//!  - ATTACH_PIPE_STREAM: intercepted by the server before dispatch;
//!    this stub returns INTERNAL when reached directly.

use crate::proto::daemon::{
    AttachPipeStreamResponse, DaemonRequest, DaemonResponse, DetachPipeStreamResponse, KeyValue,
    ListPipeSessionsResponse, PipeSessionInfo, PipeStreamKind, SpawnPipeSessionResponse,
    StatusCode, TerminatePipeSessionResponse, WritePipeStdinResponse,
};

use super::util::{error_pty_response, termination_outcome_to_proto};
use super::DaemonState;

pub fn handle_spawn_pipe_session(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.spawn_pipe_session.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "spawn_pipe_session payload missing".into(),
            )
        }
    };

    let cwd = if req.cwd.is_empty() {
        None
    } else {
        Some(req.cwd.clone())
    };

    let env = if req.env.is_empty() && !req.clear_inherited_env {
        None
    } else {
        let mut pairs: Vec<(String, String)> = if req.clear_inherited_env {
            Vec::new()
        } else {
            std::env::vars().collect()
        };
        for KeyValue { key, value } in &req.env {
            if let Some((_, v)) = pairs.iter_mut().find(|(k, _)| k == key) {
                *v = value.clone();
            } else {
                pairs.push((key.clone(), value.clone()));
            }
        }
        Some(pairs)
    };

    let command_display = req.argv.join(" ");
    let originator = if req.originator.is_empty() {
        format!("client:{}", request.client_name)
    } else {
        req.originator.clone()
    };

    match state.pipe_sessions.spawn(
        req.argv.clone(),
        cwd,
        env,
        originator,
        command_display,
        req.merge_stderr_into_stdout,
    ) {
        Ok(session) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            spawn_pipe_session: Some(SpawnPipeSessionResponse {
                session_id: session.id.clone(),
                pid: session.pid,
                created_at: session.created_at_unix,
            }),
            ..Default::default()
        },
        Err(e) => error_pty_response(request.id, StatusCode::Internal, e.to_string()),
    }
}

pub fn handle_list_pipe_sessions(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let originator_filter = request
        .list_pipe_sessions
        .as_ref()
        .map(|r| r.originator.clone())
        .unwrap_or_default();

    let mut infos = Vec::new();
    for session in state.pipe_sessions.list() {
        if !originator_filter.is_empty() && session.originator != originator_filter {
            continue;
        }
        let (exited, exit_code, exited_at, outcome) = match session.exit_state() {
            Some(s) => (true, s.exit_code, s.exited_at_unix, s.outcome),
            None => (
                false,
                0,
                0.0,
                crate::daemon::pty_sessions::TerminationOutcome::Unspecified,
            ),
        };
        infos.push(PipeSessionInfo {
            session_id: session.id.clone(),
            pid: session.pid,
            command: session.command.clone(),
            cwd: session.cwd.clone(),
            originator: session.originator.clone(),
            created_at: session.created_at_unix,
            stdout_attached: session
                .is_attached(crate::daemon::pipe_sessions::PipeStreamSelect::Stdout),
            stderr_attached: session
                .is_attached(crate::daemon::pipe_sessions::PipeStreamSelect::Stderr),
            exited,
            exit_code,
            exited_at,
            merge_stderr_into_stdout: session.merge_stderr_into_stdout,
            termination_outcome: termination_outcome_to_proto(outcome) as i32,
        });
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        list_pipe_sessions: Some(ListPipeSessionsResponse { sessions: infos }),
        ..Default::default()
    }
}

pub fn handle_detach_pipe_stream(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.detach_pipe_stream.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "detach_pipe_stream payload missing".into(),
            )
        }
    };
    let session = match state.pipe_sessions.get(&req.session_id) {
        Some(s) => s,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::NotFound,
                format!("pipe session not found: {}", req.session_id),
            )
        }
    };
    let stream = match PipeStreamKind::try_from(req.stream) {
        Ok(PipeStreamKind::Stdout) => crate::daemon::pipe_sessions::PipeStreamSelect::Stdout,
        Ok(PipeStreamKind::Stderr) => crate::daemon::pipe_sessions::PipeStreamSelect::Stderr,
        _ => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "stream must be PIPE_STREAM_KIND_STDOUT or PIPE_STREAM_KIND_STDERR".into(),
            )
        }
    };
    session.notify_attached(
        stream,
        crate::daemon::pty_sessions::OutboundFrame::Ended(
            crate::daemon::pty_sessions::AttachmentEnded::Detached,
        ),
    );
    session.clear_attachment(stream);
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        detach_pipe_stream: Some(DetachPipeStreamResponse::default()),
        ..Default::default()
    }
}

pub fn handle_terminate_pipe_session(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.terminate_pipe_session.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "terminate_pipe_session payload missing".into(),
            )
        }
    };
    let session = match state.pipe_sessions.get(&req.session_id) {
        Some(s) => s,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::NotFound,
                format!("pipe session not found: {}", req.session_id),
            )
        }
    };
    let grace_ms = if req.grace_ms == 0 {
        2000
    } else {
        req.grace_ms
    };
    if let Err(e) = session.terminate(std::time::Duration::from_millis(grace_ms as u64)) {
        return error_pty_response(request.id, StatusCode::Internal, e.to_string());
    }
    for stream in [
        crate::daemon::pipe_sessions::PipeStreamSelect::Stdout,
        crate::daemon::pipe_sessions::PipeStreamSelect::Stderr,
    ] {
        session.notify_attached(
            stream,
            crate::daemon::pty_sessions::OutboundFrame::Ended(
                crate::daemon::pty_sessions::AttachmentEnded::Terminated,
            ),
        );
    }
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        terminate_pipe_session: Some(TerminatePipeSessionResponse::default()),
        ..Default::default()
    }
}

pub fn handle_write_pipe_stdin(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.write_pipe_stdin.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "write_pipe_stdin payload missing".into(),
            )
        }
    };
    let session = match state.pipe_sessions.get(&req.session_id) {
        Some(s) => s,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::NotFound,
                format!("pipe session not found: {}", req.session_id),
            )
        }
    };
    match session.write_stdin(&req.data, req.close) {
        Ok(n) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            write_pipe_stdin: Some(WritePipeStdinResponse {
                bytes_written: n as u64,
            }),
            ..Default::default()
        },
        Err(e) => error_pty_response(request.id, StatusCode::Internal, e.to_string()),
    }
}

/// Stub for the pipe-stream attach handler. Intercepted by
/// `server.rs::handle_connection_inner` before dispatch; reaching this
/// directly means the dispatcher wiring is broken.
pub fn handle_attach_pipe_stream(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Internal as i32,
        message: "attach_pipe_stream must be intercepted by the streaming server path".into(),
        attach_pipe_stream: Some(AttachPipeStreamResponse::default()),
        ..Default::default()
    }
}
