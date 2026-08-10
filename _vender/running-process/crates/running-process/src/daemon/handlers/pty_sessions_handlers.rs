//! Detachable PTY session handlers (issue #130 milestone 2).
//!
//! The daemon owns each PTY child + master via
//! [`crate::daemon::pty_sessions::PtySessionRegistry`]. These non-streaming
//! handlers cover Spawn / Detach / List / Terminate / Resize; Attach is
//! handled separately in `server.rs::handle_attach_streaming` because it
//! takes ownership of the IPC framed stream after the response is sent.

use crate::proto::daemon::{
    AttachPtySessionResponse, DaemonRequest, DaemonResponse, DetachPtySessionResponse, KeyValue,
    ListPtySessionsResponse, PtySessionInfo, ResizePtySessionResponse, SpawnPtySessionResponse,
    StatusCode, TerminatePtySessionResponse,
};
use crate::terminal_graphics::terminal_graphics_capabilities_to_proto;

use super::util::{error_pty_response, termination_outcome_to_proto};
use super::DaemonState;

pub fn handle_spawn_pty_session(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.spawn_pty_session.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "spawn_pty_session payload missing".into(),
            );
        }
    };

    let rows = if req.rows == 0 { 24 } else { req.rows as u16 };
    let cols = if req.cols == 0 { 80 } else { req.cols as u16 };

    let cwd = if req.cwd.is_empty() {
        None
    } else {
        Some(req.cwd.clone())
    };

    // Build env. If `clear_inherited_env` is false, layer the supplied env
    // on top of the daemon's; otherwise use only the supplied entries. The
    // case-insensitive dedup that `SpawnDaemon` does on Windows is not
    // re-implemented here because `NativePtyProcess` does not collapse env
    // keys the way `Command::env` does — every KV pair survives.
    let env = if req.env.is_empty() && !req.clear_inherited_env {
        None
    } else {
        let mut pairs: Vec<(String, String)> = if req.clear_inherited_env {
            Vec::new()
        } else {
            std::env::vars().collect()
        };
        for KeyValue { key, value } in &req.env {
            // Overwrite if key already present.
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

    match state.pty_sessions.spawn(
        req.argv.clone(),
        cwd,
        env,
        rows,
        cols,
        originator,
        command_display,
    ) {
        Ok(session) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            spawn_pty_session: Some(SpawnPtySessionResponse {
                session_id: session.id.clone(),
                pid: session.pid,
                created_at: session.created_at_unix,
            }),
            ..Default::default()
        },
        Err(e) => error_pty_response(request.id, StatusCode::Internal, e.to_string()),
    }
}

pub fn handle_detach_pty_session(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.detach_pty_session.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "detach_pty_session payload missing".into(),
            );
        }
    };

    let session = match state.pty_sessions.get(&req.session_id) {
        Some(s) => s,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::NotFound,
                format!("session not found: {}", req.session_id),
            );
        }
    };

    // Notify the attached client and drop the slot.
    session.notify_attached(crate::daemon::pty_sessions::OutboundFrame::Ended(
        crate::daemon::pty_sessions::AttachmentEnded::Detached,
    ));
    session.clear_attachment();

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        detach_pty_session: Some(DetachPtySessionResponse::default()),
        ..Default::default()
    }
}

pub fn handle_list_pty_sessions(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let originator_filter = request
        .list_pty_sessions
        .as_ref()
        .map(|r| r.originator.clone())
        .unwrap_or_default();

    let mut infos = Vec::new();
    for session in state.pty_sessions.list() {
        if !originator_filter.is_empty() && session.originator != originator_filter {
            continue;
        }
        let exit = session.exit_state();
        let (exited, exit_code, exited_at, outcome) = match exit {
            Some(s) => (true, s.exit_code, s.exited_at_unix, s.outcome),
            None => (
                false,
                0,
                0.0,
                crate::daemon::pty_sessions::TerminationOutcome::Unspecified,
            ),
        };
        infos.push(PtySessionInfo {
            session_id: session.id.clone(),
            pid: session.pid,
            command: session.command.clone(),
            cwd: session.cwd.clone(),
            originator: session.originator.clone(),
            created_at: session.created_at_unix,
            attached: session.is_attached(),
            exited,
            exit_code,
            exited_at,
            rows: session.rows() as u32,
            cols: session.cols() as u32,
            termination_outcome: termination_outcome_to_proto(outcome) as i32,
            attached_is_tty: session.attached_is_tty(),
            attached_term: session.attached_term(),
            attached_graphics_capabilities: Some(terminal_graphics_capabilities_to_proto(
                &session.attached_graphics_capabilities(),
            )),
        });
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        list_pty_sessions: Some(ListPtySessionsResponse { sessions: infos }),
        ..Default::default()
    }
}

pub fn handle_terminate_pty_session(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.terminate_pty_session.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "terminate_pty_session payload missing".into(),
            );
        }
    };

    let session = match state.pty_sessions.get(&req.session_id) {
        Some(s) => s,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::NotFound,
                format!("session not found: {}", req.session_id),
            );
        }
    };

    // M4 will turn this into a configurable soft-then-hard schedule.
    // For M2 we issue an immediate terminate and let the reader thread
    // observe the exit + record exit state.
    let grace_ms = if req.grace_ms == 0 {
        2000
    } else {
        req.grace_ms
    };
    if let Err(e) = session.terminate(std::time::Duration::from_millis(grace_ms as u64)) {
        return error_pty_response(request.id, StatusCode::Internal, e.to_string());
    }

    // Notify any attached client.
    session.notify_attached(crate::daemon::pty_sessions::OutboundFrame::Ended(
        crate::daemon::pty_sessions::AttachmentEnded::Terminated,
    ));

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        terminate_pty_session: Some(TerminatePtySessionResponse::default()),
        ..Default::default()
    }
}

/// Stub for the attach handler. The actual attach work happens in
/// `server.rs::handle_attach_streaming` because it needs ownership of the
/// IPC framed stream after the response is sent. This stub exists so the
/// dispatcher table stays uniform; it should never be reached because the
/// server-side connection loop intercepts `ATTACH_PTY_SESSION` before
/// dispatch.
pub fn handle_attach_pty_session(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Internal as i32,
        message: "attach_pty_session must be intercepted by the streaming server path".into(),
        attach_pty_session: Some(AttachPtySessionResponse::default()),
        ..Default::default()
    }
}

/// Resize a PTY session without an active attachment (#130 M5
/// follow-up). The new size persists for the lifetime of the session
/// and overrides any per-attach size passed by future attach requests
/// (they can still override it again by sending their own rows/cols).
pub fn handle_resize_pty_session(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.resize_pty_session.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "resize_pty_session payload missing".into(),
            )
        }
    };
    let session = match state.pty_sessions.get(&req.session_id) {
        Some(s) => s,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::NotFound,
                format!("session not found: {}", req.session_id),
            )
        }
    };
    let rows = if req.rows == 0 {
        session.rows()
    } else {
        req.rows as u16
    };
    let cols = if req.cols == 0 {
        session.cols()
    } else {
        req.cols as u16
    };
    if let Err(e) = session.resize(rows, cols) {
        return error_pty_response(request.id, StatusCode::Internal, e.to_string());
    }
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        resize_pty_session: Some(ResizePtySessionResponse::default()),
        ..Default::default()
    }
}
