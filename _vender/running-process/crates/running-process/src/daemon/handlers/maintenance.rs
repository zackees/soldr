//! Session maintenance handlers: purge exited, bulk terminate, get backlog.

use crate::proto::daemon::{
    BulkTerminateSessionsResponse, DaemonRequest, DaemonResponse, GetSessionBacklogResponse,
    PipeStreamKind, PurgeExitedSessionsResponse, StatusCode,
};

use super::util::{error_pty_response, termination_outcome_to_proto};
use super::DaemonState;

/// Purge exited sessions from both registries (#130 M9 H4). Live
/// sessions are untouched. Returns counts so callers can report how
/// much was reaped.
pub fn handle_purge_exited_sessions(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let originator = request
        .purge_exited_sessions
        .as_ref()
        .map(|r| r.originator.clone())
        .unwrap_or_default();
    let pty_purged = state.pty_sessions.purge_exited(&originator) as u32;
    let pipe_purged = state.pipe_sessions.purge_exited(&originator) as u32;
    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        purge_exited_sessions: Some(PurgeExitedSessionsResponse {
            pty_purged,
            pipe_purged,
        }),
        ..Default::default()
    }
}

/// Terminate every session whose age is strictly greater than the
/// requested threshold (#130 M9 H4). Each session keeps its own
/// soft-then-hard schedule; this handler just issues the terminate
/// signal and returns counts. Use `older_than_secs=0` to terminate
/// everything in scope.
pub fn handle_bulk_terminate_sessions(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.bulk_terminate_sessions.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "bulk_terminate_sessions payload missing".into(),
            )
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let threshold = req.older_than_secs as f64;
    let grace = std::time::Duration::from_millis(req.grace_ms.max(1) as u64);

    let mut pty_terminated: u32 = 0;
    for session in state.pty_sessions.list() {
        if session.exit_state().is_some() {
            continue;
        }
        if !req.originator.is_empty() && session.originator != req.originator {
            continue;
        }
        if (now - session.created_at_unix) <= threshold {
            continue;
        }
        if session.terminate(grace).is_ok() {
            pty_terminated += 1;
        }
    }

    let mut pipe_terminated: u32 = 0;
    for session in state.pipe_sessions.list() {
        if session.exit_state().is_some() {
            continue;
        }
        if !req.originator.is_empty() && session.originator != req.originator {
            continue;
        }
        if (now - session.created_at_unix) <= threshold {
            continue;
        }
        if session.terminate(grace).is_ok() {
            pipe_terminated += 1;
        }
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        bulk_terminate_sessions: Some(BulkTerminateSessionsResponse {
            pty_terminated,
            pipe_terminated,
        }),
        ..Default::default()
    }
}

/// Snapshot a session's output ring buffer without consuming it
/// (#130 M7 B4). Looks up the session in the PTY registry first, then
/// falls back to the pipe registry. For pipe sessions the request's
/// `pipe_stream` field selects between stdout and stderr (default
/// stdout).
pub fn handle_get_session_backlog(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.get_session_backlog.as_ref() {
        Some(r) => r,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "get_session_backlog payload missing".into(),
            )
        }
    };

    if let Some(pty) = state.pty_sessions.get(&req.session_id) {
        let (backlog, missed) = pty.backlog_snapshot();
        let (exited, exit_code, exited_at, outcome) = match pty.exit_state() {
            Some(s) => (true, s.exit_code, s.exited_at_unix, s.outcome),
            None => (
                false,
                0,
                0.0,
                crate::daemon::pty_sessions::TerminationOutcome::Unspecified,
            ),
        };
        return DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            get_session_backlog: Some(GetSessionBacklogResponse {
                backlog,
                bytes_missed: missed,
                session_kind: "pty".into(),
                exited,
                exit_code,
                exited_at,
                termination_outcome: termination_outcome_to_proto(outcome) as i32,
            }),
            ..Default::default()
        };
    }

    if let Some(pipe) = state.pipe_sessions.get(&req.session_id) {
        let stream = match PipeStreamKind::try_from(req.pipe_stream) {
            Ok(PipeStreamKind::Stderr) => crate::daemon::pipe_sessions::PipeStreamSelect::Stderr,
            // Default and Stdout both map to stdout.
            _ => crate::daemon::pipe_sessions::PipeStreamSelect::Stdout,
        };
        let (backlog, missed) = pipe.backlog_snapshot(stream);
        let (exited, exit_code, exited_at, outcome) = match pipe.exit_state() {
            Some(s) => (true, s.exit_code, s.exited_at_unix, s.outcome),
            None => (
                false,
                0,
                0.0,
                crate::daemon::pty_sessions::TerminationOutcome::Unspecified,
            ),
        };
        return DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            get_session_backlog: Some(GetSessionBacklogResponse {
                backlog,
                bytes_missed: missed,
                session_kind: "pipe".into(),
                exited,
                exit_code,
                exited_at,
                termination_outcome: termination_outcome_to_proto(outcome) as i32,
            }),
            ..Default::default()
        };
    }

    error_pty_response(
        request.id,
        StatusCode::NotFound,
        format!("session not found: {}", req.session_id),
    )
}
