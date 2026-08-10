//! Service supervision (runpm) request handlers — Phase 2 + Phase 3 (#222, #426).
//!
//! `start`, `stop`, `restart`, `delete`, `list`, `describe` (show), `logs`,
//! and `flush` are implemented end-to-end against the SQLite-backed
//! [`crate::daemon::services::ServiceRegistry`]. Only `save`/`resurrect`
//! (Phase 4) remain stubs that acknowledge the RPC.

use std::collections::HashMap;

use crate::daemon::services::{ServiceDef, ServiceError, ServiceRecord};
use crate::daemon::services_snapshot::{resurrect_from_snapshot, save_snapshot};
use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, ServiceConfig, ServiceDeleteResponse, ServiceDescribeResponse,
    ServiceFlushResponse, ServiceListResponse, ServiceLogsResponse, ServiceRestartResponse,
    ServiceResurrectResponse, ServiceSaveResponse, ServiceStartResponse, ServiceState,
    ServiceStopResponse, StatusCode,
};

use super::DaemonState;

// ---------------------------------------------------------------------------
// Proto <-> domain mapping
// ---------------------------------------------------------------------------

fn def_from_config(config: ServiceConfig) -> ServiceDef {
    ServiceDef {
        name: config.name,
        cmd: config.cmd,
        cwd: config.cwd,
        env: config.env.into_iter().collect(),
        autorestart: config.autorestart,
        max_restarts: config.max_restarts,
        restart_delay_ms: config.restart_delay_ms,
        kill_timeout_ms: config.kill_timeout_ms,
        min_uptime_ms: config.min_uptime_ms,
    }
}

fn config_from_def(def: &ServiceDef) -> ServiceConfig {
    let env: HashMap<String, String> = def.env.iter().cloned().collect();
    ServiceConfig {
        name: def.name.clone(),
        cmd: def.cmd.clone(),
        cwd: def.cwd.clone(),
        env,
        autorestart: def.autorestart,
        max_restarts: def.max_restarts,
        restart_delay_ms: def.restart_delay_ms,
        kill_timeout_ms: def.kill_timeout_ms,
        min_uptime_ms: def.min_uptime_ms,
    }
}

fn state_from_record(rec: &ServiceRecord) -> ServiceState {
    ServiceState {
        name: rec.def.name.clone(),
        id: rec.id,
        status: rec.status.as_str().to_string(),
        pid: rec.pid,
        restart_count: rec.restart_count,
        last_started_at: rec.last_started_at,
        last_exited_at: rec.last_exited_at,
        last_exit_code: rec.last_exit_code,
        config: Some(config_from_def(&rec.def)),
    }
}

/// Map a [`ServiceError`] to the closest protobuf [`StatusCode`].
fn status_for(err: &ServiceError) -> StatusCode {
    match err {
        ServiceError::NotFound(_) => StatusCode::NotFound,
        ServiceError::AlreadyExists(_) | ServiceError::InvalidConfig(_) => {
            StatusCode::InvalidArgument
        }
        ServiceError::Spawn(_) | ServiceError::Db(_) => StatusCode::Internal,
    }
}

fn err_response(request: &DaemonRequest, err: ServiceError) -> DaemonResponse {
    DaemonResponse {
        request_id: request.id,
        code: status_for(&err) as i32,
        message: err.to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Handlers — implemented (Phase 2)
// ---------------------------------------------------------------------------

/// `ServiceStart`: create/update a service definition and launch it.
pub fn handle_service_start(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(req) = request.service_start.as_ref() else {
        return err_response(
            request,
            ServiceError::InvalidConfig("missing service_start payload".into()),
        );
    };
    let Some(config) = req.config.clone() else {
        return err_response(
            request,
            ServiceError::InvalidConfig("missing service config".into()),
        );
    };
    match state.services.start(def_from_config(config)) {
        Ok(rec) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_start: Some(ServiceStartResponse {
                service: Some(state_from_record(&rec)),
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceStop`: stop the targeted service(s).
pub fn handle_service_stop(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_stop
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.stop(&target) {
        Ok(stopped_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_stop: Some(ServiceStopResponse { stopped_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceRestart`: stop + start the targeted service(s), bumping counts.
pub fn handle_service_restart(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_restart
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.restart(&target) {
        Ok(restarted_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_restart: Some(ServiceRestartResponse { restarted_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceDelete`: stop (if running) and remove the targeted service(s).
pub fn handle_service_delete(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_delete
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.delete(&target) {
        Ok(deleted_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_delete: Some(ServiceDeleteResponse { deleted_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceList`: return all service definitions + state.
pub fn handle_service_list(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    match state.services.list() {
        Ok(records) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_list: Some(ServiceListResponse {
                services: records.iter().map(state_from_record).collect(),
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceDescribe` (show): return one service by name or id.
pub fn handle_service_describe(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_describe
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.describe(&target) {
        Ok(rec) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_describe: Some(ServiceDescribeResponse {
                service: Some(state_from_record(&rec)),
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

// ---------------------------------------------------------------------------
// Handlers — Phase 3
// ---------------------------------------------------------------------------

/// Hard cap on the bytes we'll return in a single `service_logs` response so
/// one noisy service can't blow the IPC budget. ~64 KiB is comfortably below
/// the daemon's protobuf message ceiling and easily fits one screenful for an
/// operator scrolling logs.
const LOGS_RESPONSE_BYTE_BUDGET: usize = 64 * 1024;

/// Default tail length when the client requests `lines: 0`.
const LOGS_DEFAULT_LINES: u32 = 100;

/// `ServiceLogs`: tail the per-service `-out.log` and `-err.log` files on
/// disk. Each line is prefixed with `[stdout]` / `[stderr]` so the operator
/// can tell streams apart in a single transcript. `--follow` is implemented
/// client-side by polling this handler — there is no streaming RPC.
pub fn handle_service_logs(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(req) = request.service_logs.as_ref() else {
        return err_response(
            request,
            ServiceError::InvalidConfig("missing service_logs payload".into()),
        );
    };
    match state.services.read_logs(
        &req.target,
        req.lines,
        LOGS_DEFAULT_LINES,
        LOGS_RESPONSE_BYTE_BUDGET,
    ) {
        Ok(log_text) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_logs: Some(ServiceLogsResponse { log_text }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceFlush`: truncate the on-disk `-out.log` and `-err.log` files for
/// the targeted service(s) to zero bytes. `target == "all"` (or empty)
/// flushes every registered service; a single-target miss returns
/// `NOT_FOUND`. The append-mode writer threads keep going across the
/// truncate — the next line they emit lands at offset 0.
pub fn handle_service_flush(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let target = request
        .service_flush
        .as_ref()
        .map(|r| r.target.clone())
        .unwrap_or_default();
    match state.services.flush_logs(&target) {
        Ok(flushed_count) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_flush: Some(ServiceFlushResponse { flushed_count }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceSave` (Phase 4 — #427): write the current `services` table to an
/// atomic JSON snapshot next to the SQLite db. The response carries the
/// absolute snapshot path and the row count so the operator can verify
/// the write without re-listing.
pub fn handle_service_save(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    match save_snapshot(&state.services) {
        Ok((path, count)) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_save: Some(ServiceSaveResponse {
                snapshot_path: path.to_string_lossy().into_owned(),
                service_count: count,
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}

/// `ServiceResurrect` (Phase 4 — #427): rehydrate definitions from the
/// JSON snapshot and re-launch every service that was `online` when it was
/// saved. Idempotent: a second call updates existing rows in place via
/// `INSERT OR REPLACE`. Returns the total number of definitions restored
/// (re-launches are best-effort; per-service spawn failures are warned and
/// counted via the daemon's tracing output rather than failing the batch).
pub fn handle_service_resurrect(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    match resurrect_from_snapshot(&state.services) {
        Ok((restored, _restarted)) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            service_resurrect: Some(ServiceResurrectResponse {
                restored_count: restored,
            }),
            ..Default::default()
        },
        Err(e) => err_response(request, e),
    }
}
