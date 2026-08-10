//! Handlers for the SQLite-backed registry: register/unregister and the
//! two list variants.

use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, ListActiveResponse, ListByOriginatorResponse, ProcessState,
    RegisterResponse, StatusCode, TrackedProcess, UnregisterResponse,
};

use crate::daemon::registry::{self, TrackedEntry};

use super::util::error_response;
use super::DaemonState;

/// Convert a [`TrackedEntry`] to a proto [`TrackedProcess`].
pub(super) fn entry_to_tracked_process(entry: &TrackedEntry) -> TrackedProcess {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let uptime = (now - entry.registered_at).max(0.0);

    TrackedProcess {
        pid: entry.pid,
        created_at: entry.created_at_ms as f64 / 1000.0,
        kind: entry.kind.clone(),
        command: entry.command.clone(),
        cwd: entry.cwd.clone(),
        originator: entry.originator.clone(),
        containment: entry.containment.clone(),
        registered_at: entry.registered_at,
        uptime_seconds: uptime,
        parent_alive: true,                // Phase 4 reaper will validate
        state: ProcessState::Alive as i32, // Phase 4 reaper will validate
        last_validated_at: 0.0,            // Phase 4
    }
}

/// Handle a `Register` request by adding a process to the registry.
pub fn handle_register(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.register else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing register payload".into(),
        );
    };

    if req.pid == 0 {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "pid must be > 0".into(),
        );
    }
    if req.command.is_empty() {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "command must not be empty".into(),
        );
    }

    let created_at_ms = registry::created_at_to_ms(req.created_at);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let entry = TrackedEntry {
        pid: req.pid,
        created_at_ms,
        kind: req.kind.clone(),
        command: req.command.clone(),
        cwd: req.cwd.clone(),
        originator: req.originator.clone(),
        containment: req.containment.clone(),
        registered_at: now,
    };

    if let Err(e) = state.registry.register(entry) {
        return error_response(
            request.id,
            StatusCode::Internal,
            format!("registry error: {e}"),
        );
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        register: Some(RegisterResponse {}),
        ..Default::default()
    }
}

/// Handle an `Unregister` request by removing a process from the registry.
pub fn handle_unregister(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.unregister else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing unregister payload".into(),
        );
    };

    if state.registry.unregister(req.pid) {
        DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            unregister: Some(UnregisterResponse {}),
            ..Default::default()
        }
    } else {
        error_response(
            request.id,
            StatusCode::NotFound,
            format!("pid {} not found in registry", req.pid),
        )
    }
}

/// Handle a `ListActive` request by returning all tracked processes.
pub fn handle_list_active(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let entries = state.registry.list_all();
    let processes: Vec<TrackedProcess> = entries.iter().map(entry_to_tracked_process).collect();

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        list_active: Some(ListActiveResponse { processes }),
        ..Default::default()
    }
}

/// Handle a `ListByOriginator` request by returning processes matching the tool prefix.
pub fn handle_list_by_originator(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.list_by_originator else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing list_by_originator payload".into(),
        );
    };

    let entries = state.registry.list_by_originator(&req.tool);
    let processes: Vec<TrackedProcess> = entries.iter().map(entry_to_tracked_process).collect();

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        list_by_originator: Some(ListByOriginatorResponse { processes }),
        ..Default::default()
    }
}
