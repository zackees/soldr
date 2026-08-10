//! Handlers for the kill-tree and kill-zombies request types.

use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, KillTreeResponse, KillZombiesResponse, StatusCode, ZombieReport,
};
use std::time::Duration;

use crate::daemon::reaper;

use super::util::error_response;
use super::DaemonState;

/// Handle a `KillTree` request by killing a process and its descendants.
pub fn handle_kill_tree(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.kill_tree else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing kill_tree payload".into(),
        );
    };

    let timeout = if req.timeout_seconds > 0.0 {
        req.timeout_seconds
    } else {
        3.0
    };
    let timeout = Duration::from_secs_f64(timeout.min(30.0));
    let killed = match crate::process_tree::kill_tree(req.pid, timeout) {
        Ok(killed) => killed,
        Err(error) => {
            return error_response(
                request.id,
                StatusCode::Internal,
                format!("failed to kill process tree: {error}"),
            );
        }
    };

    // Unregister from registry (if tracked).
    state.registry.unregister(req.pid);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        kill_tree: Some(KillTreeResponse {
            processes_killed: killed,
        }),
        ..Default::default()
    }
}

/// Handle a `KillZombies` request by scanning for and optionally killing zombie processes.
pub fn handle_kill_zombies(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.kill_zombies else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing kill_zombies payload".into(),
        );
    };

    let zombies = reaper::scan_for_zombies(state);
    let orphan_conhosts = reaper::scan_for_orphan_conhosts();

    let mut reports: Vec<ZombieReport> = Vec::new();

    // Registry-based zombies.
    if req.dry_run {
        reports.extend(zombies.iter().map(|z| ZombieReport {
            pid: z.pid,
            command: z.command.clone(),
            reason: z.reason.clone(),
            killed: false,
        }));
        reports.extend(orphan_conhosts.iter().map(|z| ZombieReport {
            pid: z.pid,
            command: z.command.clone(),
            reason: z.reason.clone(),
            killed: false,
        }));
    } else {
        let reg_results = reaper::kill_zombies(state, &zombies);
        reports.extend(
            zombies
                .iter()
                .zip(reg_results.iter())
                .map(|(z, (_pid, killed))| ZombieReport {
                    pid: z.pid,
                    command: z.command.clone(),
                    reason: z.reason.clone(),
                    killed: *killed,
                }),
        );

        let conhost_results = reaper::kill_conhosts(&orphan_conhosts);
        reports.extend(orphan_conhosts.iter().zip(conhost_results.iter()).map(
            |(z, (_pid, killed))| ZombieReport {
                pid: z.pid,
                command: z.command.clone(),
                reason: z.reason.clone(),
                killed: *killed,
            },
        ));
    }

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        kill_zombies: Some(KillZombiesResponse { zombies: reports }),
        ..Default::default()
    }
}
