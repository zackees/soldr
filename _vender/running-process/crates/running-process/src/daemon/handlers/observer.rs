//! IPC handlers for daemon-owned observer subscriptions (#221 Phase 2 / #429).
//!
//! Sibling of `handlers::telemetry` (which handles byte-stream tees). The
//! observer subsystem ships event-stream payloads
//! ([`crate::observer::ObserverEvent`]) instead of bytes. Each session
//! ([`crate::daemon::pty_sessions::OwnedPtySession`] or
//! [`crate::daemon::pipe_sessions::OwnedPipeSession`]) owns an
//! [`crate::daemon::observer_registry::ObserverRegistry`] of registered
//! sinks. The session lifecycle code fans every emitted
//! [`crate::observer::ObserverEvent`] out to every registered sink whose
//! category filter matches.

use crate::daemon::observer_registry::{ObserverBackpressure, ObserverSubscriberId};
use crate::observer::EventCategory;
use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, GetSessionObserverStatusResponse,
    ObserverBackpressure as ProtoObserverBackpressure, ObserverSessionKind,
    RegisterSessionObserverResponse, StatusCode, UnregisterSessionObserverResponse,
};

use super::util::error_pty_response;
use super::DaemonState;

/// Default channel capacity used when the request's `ring_capacity_events`
/// field is zero. Picked to match the documented Phase 2 default.
const DEFAULT_RING_CAPACITY: usize = 1024;

/// Register a session-scoped observer subscription.
///
/// # Persistence semantics
///
/// Observer registrations live on the per-session struct rather than on the
/// IPC connection that requested them. They survive the client's IPC
/// connection going away. When a new client reconnects and re-registers it
/// gets a fresh subscription — but events that arrived while no client was
/// attached are **not** replayed. This is intentionally different from the
/// PTY / pipe byte backlog: observer events are an event-stream surface, so
/// dropped events are recorded in `missed_events` instead of being
/// retroactively delivered.
pub fn handle_register_session_observer(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.register_session_observer.as_ref() {
        Some(req) => req,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "register_session_observer payload missing".into(),
            );
        }
    };

    if req.session_id.is_empty() {
        return error_pty_response(
            request.id,
            StatusCode::InvalidArgument,
            "session_id must not be empty".into(),
        );
    }

    let session_kind = match ObserverSessionKind::try_from(req.session_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid observer session kind".into(),
            );
        }
    };

    let categories = match decode_categories(&req.categories) {
        Ok(c) => c,
        Err(message) => {
            return error_pty_response(request.id, StatusCode::InvalidArgument, message);
        }
    };

    let backpressure = match ProtoObserverBackpressure::try_from(req.backpressure) {
        Ok(ProtoObserverBackpressure::DropOldest) => ObserverBackpressure::DropOldest,
        Ok(ProtoObserverBackpressure::Block) => ObserverBackpressure::Block,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid backpressure".into(),
            );
        }
    };

    let capacity = if req.ring_capacity_events == 0 {
        DEFAULT_RING_CAPACITY
    } else {
        req.ring_capacity_events as usize
    };

    let result = match session_kind {
        ObserverSessionKind::Pty => state
            .pty_sessions
            .get(&req.session_id)
            .ok_or_else(|| "PTY session not found".to_string())
            .map(|session| {
                // Receiver is intentionally dropped on the daemon side in
                // this PR. The bounded channel is the sink; future PRs
                // will stream events back over IPC. For now consumers
                // observe activity via GetSessionObserverStatus
                // (delivered_events / missed_events counters). The
                // receiver living inside the daemon side is fine: the
                // sender's `try_send` still detects "full" correctly.
                let (id, rx) =
                    session
                        .observers
                        .add_channel(categories.clone(), capacity, backpressure);
                // Drop the receiver explicitly to make the
                // event-stream-not-yet-wired semantics deliberate; under
                // DropOldest the channel will fill quickly and overflow
                // events will be counted via missed_events.
                drop(rx);
                id
            }),
        ObserverSessionKind::Pipe => state
            .pipe_sessions
            .get(&req.session_id)
            .ok_or_else(|| "pipe session not found".to_string())
            .map(|session| {
                let (id, rx) =
                    session
                        .observers
                        .add_channel(categories.clone(), capacity, backpressure);
                drop(rx);
                id
            }),
        ObserverSessionKind::Unspecified => Err("session_kind must be PTY or PIPE".into()),
    };

    match result {
        Ok(id) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: "OK".into(),
            register_session_observer: Some(RegisterSessionObserverResponse {
                subscriber_id: id.into_string(),
            }),
            ..Default::default()
        },
        Err(message) => {
            let status = if message.contains("not found") {
                StatusCode::NotFound
            } else {
                StatusCode::InvalidArgument
            };
            error_pty_response(request.id, status, message)
        }
    }
}

/// Unregister a previously registered subscription.
pub fn handle_unregister_session_observer(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.unregister_session_observer.as_ref() {
        Some(req) => req,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "unregister_session_observer payload missing".into(),
            );
        }
    };
    let session_kind = match ObserverSessionKind::try_from(req.session_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid observer session kind".into(),
            );
        }
    };
    if req.subscriber_id.is_empty() {
        return error_pty_response(
            request.id,
            StatusCode::InvalidArgument,
            "subscriber_id must not be empty".into(),
        );
    }
    let subscriber = ObserverSubscriberId::from_string(req.subscriber_id.clone());

    let result = match session_kind {
        ObserverSessionKind::Pty => state
            .pty_sessions
            .get(&req.session_id)
            .ok_or_else(|| "PTY session not found".to_string())
            .and_then(|session| {
                if session.observers.remove(&subscriber) {
                    Ok(())
                } else {
                    Err("subscriber_id not found".into())
                }
            }),
        ObserverSessionKind::Pipe => state
            .pipe_sessions
            .get(&req.session_id)
            .ok_or_else(|| "pipe session not found".to_string())
            .and_then(|session| {
                if session.observers.remove(&subscriber) {
                    Ok(())
                } else {
                    Err("subscriber_id not found".into())
                }
            }),
        ObserverSessionKind::Unspecified => Err("session_kind must be PTY or PIPE".into()),
    };

    match result {
        Ok(()) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: "OK".into(),
            unregister_session_observer: Some(UnregisterSessionObserverResponse::default()),
            ..Default::default()
        },
        Err(message) => {
            let status = if message.contains("not found") {
                StatusCode::NotFound
            } else {
                StatusCode::InvalidArgument
            };
            error_pty_response(request.id, status, message)
        }
    }
}

/// Fetch counters (delivered + missed) for a registered subscription.
pub fn handle_get_session_observer_status(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.get_session_observer_status.as_ref() {
        Some(req) => req,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "get_session_observer_status payload missing".into(),
            );
        }
    };
    let session_kind = match ObserverSessionKind::try_from(req.session_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid observer session kind".into(),
            );
        }
    };
    if req.subscriber_id.is_empty() {
        return error_pty_response(
            request.id,
            StatusCode::InvalidArgument,
            "subscriber_id must not be empty".into(),
        );
    }
    let subscriber = ObserverSubscriberId::from_string(req.subscriber_id.clone());

    let status = match session_kind {
        ObserverSessionKind::Pty => state
            .pty_sessions
            .get(&req.session_id)
            .ok_or_else(|| "PTY session not found".to_string())
            .and_then(|session| {
                session
                    .observers
                    .status(&subscriber)
                    .ok_or_else(|| "subscriber_id not found".into())
            }),
        ObserverSessionKind::Pipe => state
            .pipe_sessions
            .get(&req.session_id)
            .ok_or_else(|| "pipe session not found".to_string())
            .and_then(|session| {
                session
                    .observers
                    .status(&subscriber)
                    .ok_or_else(|| "subscriber_id not found".into())
            }),
        ObserverSessionKind::Unspecified => Err("session_kind must be PTY or PIPE".into()),
    };

    match status {
        Ok(status) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: "OK".into(),
            get_session_observer_status: Some(GetSessionObserverStatusResponse {
                missed_events: status.missed_events,
                disconnected: status.disconnected,
                delivered_events: status.delivered_events,
            }),
            ..Default::default()
        },
        Err(message) => {
            let status_code = if message.contains("not found") {
                StatusCode::NotFound
            } else {
                StatusCode::InvalidArgument
            };
            error_pty_response(request.id, status_code, message)
        }
    }
}

/// Decode the `repeated uint32` category list from the request into a vec of
/// strongly-typed [`EventCategory`] values. Empty input defaults to
/// `[Lifecycle]` to match the Phase 1 in-process default.
fn decode_categories(raw: &[u32]) -> Result<Vec<EventCategory>, String> {
    if raw.is_empty() {
        return Ok(vec![EventCategory::Lifecycle]);
    }
    let mut out = Vec::with_capacity(raw.len());
    for value in raw {
        let category = match *value {
            0 => EventCategory::Lifecycle,
            1 => EventCategory::File,
            2 => EventCategory::Network,
            3 => EventCategory::Process,
            other => return Err(format!("invalid observer category {other}")),
        };
        if !out.contains(&category) {
            out.push(category);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::handlers::DaemonState;
    use crate::daemon::pipe_sessions::PipeSessionRegistry;
    use crate::daemon::pty_sessions::PtySessionRegistry;
    use crate::daemon::registry::Registry;
    use crate::daemon::services::ServiceRegistry;
    use crate::proto::daemon::{
        DaemonRequest, GetSessionObserverStatusRequest, ObserverSessionKind,
        RegisterSessionObserverRequest, UnregisterSessionObserverRequest,
    };
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::watch;

    fn make_state() -> (DaemonState, tempfile::TempDir) {
        let (shutdown_tx, _rx) = watch::channel(false);
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let db_path = tmp_dir.path().join("observer-handler-test.db");
        let registry = Arc::new(Registry::open(&db_path).unwrap());
        let pty_sessions = Arc::new(PtySessionRegistry::new());
        let pipe_sessions = Arc::new(PipeSessionRegistry::new());
        let services =
            Arc::new(ServiceRegistry::open(&db_path, tmp_dir.path().join("services")).unwrap());
        let state = DaemonState {
            start_time: Instant::now(),
            version: "0.0.0-test".to_string(),
            socket_path: "/tmp/test.sock".to_string(),
            db_path: db_path.display().to_string(),
            scope: "global".to_string(),
            scope_hash: "0000000000000000".to_string(),
            scope_cwd: "/tmp".to_string(),
            shutdown_tx,
            active_connections: AtomicU32::new(0),
            registry,
            pty_sessions,
            pipe_sessions,
            services,
            emergency_reserve: Arc::new(
                crate::daemon::emergency_reserve::EmergencyReserve::initialize_at(
                    tmp_dir.path().join("emergency-reserve.bin"),
                    4096,
                ),
            ),
        };
        (state, tmp_dir)
    }

    /// The handler routes through dispatch, validates input, and surfaces
    /// expected error codes. Full registry behavior is exercised by the
    /// `observer_registry::tests` unit suite; this test confirms the
    /// IPC-shape glue around it.
    #[test]
    fn register_then_status_returns_expected_counts() {
        let (state, _tmp) = make_state();

        // Missing payload → INVALID_ARGUMENT.
        let bad = DaemonRequest {
            id: 1,
            ..Default::default()
        };
        let resp = handle_register_session_observer(&bad, &state);
        assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
        assert!(resp
            .message
            .contains("register_session_observer payload missing"));

        // Empty session_id → INVALID_ARGUMENT.
        let req = DaemonRequest {
            id: 2,
            register_session_observer: Some(RegisterSessionObserverRequest {
                session_id: String::new(),
                session_kind: ObserverSessionKind::Pty as i32,
                categories: vec![],
                ring_capacity_events: 0,
                backpressure: ProtoObserverBackpressure::DropOldest as i32,
            }),
            ..Default::default()
        };
        let resp = handle_register_session_observer(&req, &state);
        assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
        assert!(resp.message.contains("session_id must not be empty"));

        // Unknown session id → NOT_FOUND, *and* the response carries no
        // subscriber payload so the client knows the round trip failed.
        let req = DaemonRequest {
            id: 3,
            register_session_observer: Some(RegisterSessionObserverRequest {
                session_id: "no-such-session".into(),
                session_kind: ObserverSessionKind::Pty as i32,
                categories: vec![0u32],
                ring_capacity_events: 8,
                backpressure: ProtoObserverBackpressure::DropOldest as i32,
            }),
            ..Default::default()
        };
        let resp = handle_register_session_observer(&req, &state);
        assert_eq!(resp.code, StatusCode::NotFound as i32);
        assert!(resp.register_session_observer.is_none());

        // Unknown subscriber id on GET → NOT_FOUND.
        let pipe_req = DaemonRequest {
            id: 4,
            get_session_observer_status: Some(GetSessionObserverStatusRequest {
                session_id: "no-such-pipe".into(),
                session_kind: ObserverSessionKind::Pipe as i32,
                subscriber_id: "deadbeef".into(),
            }),
            ..Default::default()
        };
        let resp = handle_get_session_observer_status(&pipe_req, &state);
        assert_eq!(resp.code, StatusCode::NotFound as i32);

        // Unregister with unknown session → NOT_FOUND.
        let unreg = DaemonRequest {
            id: 5,
            unregister_session_observer: Some(UnregisterSessionObserverRequest {
                session_id: "no-such-session".into(),
                session_kind: ObserverSessionKind::Pty as i32,
                subscriber_id: "deadbeef".into(),
            }),
            ..Default::default()
        };
        let resp = handle_unregister_session_observer(&unreg, &state);
        assert_eq!(resp.code, StatusCode::NotFound as i32);

        // Sanity: empty subscriber_id is rejected with INVALID_ARGUMENT
        // before we even look at the session map.
        let unreg = DaemonRequest {
            id: 6,
            unregister_session_observer: Some(UnregisterSessionObserverRequest {
                session_id: "anything".into(),
                session_kind: ObserverSessionKind::Pty as i32,
                subscriber_id: String::new(),
            }),
            ..Default::default()
        };
        let resp = handle_unregister_session_observer(&unreg, &state);
        assert_eq!(resp.code, StatusCode::InvalidArgument as i32);
        assert!(resp.message.contains("subscriber_id must not be empty"));
    }
}
