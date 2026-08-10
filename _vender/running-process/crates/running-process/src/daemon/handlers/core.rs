//! Core daemon lifecycle handlers: ping, status, shutdown.

use std::sync::atomic::Ordering;

use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, PingResponse, ShutdownResponse, StatusCode, StatusResponse,
};

use super::DaemonState;

/// Handle a `Ping` request by returning the current server time.
pub fn handle_ping(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    let server_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        ping: Some(PingResponse { server_time_ms }),
        ..Default::default()
    }
}

/// Handle a `Status` request by reporting daemon health information.
pub fn handle_status(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let uptime = state.start_time.elapsed().as_secs();
    let active = state.active_connections.load(Ordering::Relaxed);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        status: Some(StatusResponse {
            version: state.version.clone(),
            uptime_seconds: uptime,
            tracked_process_count: state.registry.count() as u32,
            active_connections: active,
            socket_path: state.socket_path.clone(),
            db_path: state.db_path.clone(),
            scope: state.scope.clone(),
            scope_hash: state.scope_hash.clone(),
            scope_cwd: state.scope_cwd.clone(),
            emergency_reserve: state.emergency_reserve.state().as_str().to_string(),
        }),
        ..Default::default()
    }
}

/// Handle a `Shutdown` request by signalling the server to stop.
pub fn handle_shutdown(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let _ = state.shutdown_tx.send(true);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: "shutting down".to_string(),
        shutdown: Some(ShutdownResponse {}),
        ..Default::default()
    }
}
