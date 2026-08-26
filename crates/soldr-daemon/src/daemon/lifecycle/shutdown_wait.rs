use super::{classify_shutdown_observation, pid_is_alive, ShutdownWaitOutcome};
use crate::cache_lib::daemon_lifecycle_log_path;
use crate::core::SoldrPaths;
use std::path::Path;
use std::time::{Duration, Instant};

pub const GRACEFUL_SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SHUTDOWN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

pub fn wait_for_shutdown_responder(
    paths: &SoldrPaths,
    sock_path: &Path,
    responder: crate::daemon::protocol::ShutdownAck,
    timeout: Duration,
) -> ShutdownWaitOutcome {
    let started = Instant::now();
    let responder_endpoint = crate::daemon::backend_handle_adoption::read_broker_route_claim(paths)
        .ok()
        .flatten()
        .filter(|claim| {
            claim.pid == responder.pid && claim.started_at_unix_ms == responder.generation
        })
        .map(|claim| claim.ipc_endpoint.path);
    let mut next_heartbeat = SHUTDOWN_HEARTBEAT_INTERVAL;
    loop {
        if started.elapsed() >= next_heartbeat {
            let mut message = crate::daemon::wait_heartbeat::heartbeat_message(
                "daemon graceful shutdown",
                started.elapsed(),
                timeout,
                None,
            );
            if let Some(phase) = latest_shutdown_phase(
                paths,
                responder.pid,
                responder.generation,
                responder_endpoint.as_deref(),
            ) {
                message.push_str(&format!("; daemon phase: {phase}"));
            }
            eprintln!("{message}");
            next_heartbeat += SHUTDOWN_HEARTBEAT_INTERVAL;
        }
        let responder_pid_alive = pid_is_alive(responder.pid);
        if timeout.is_zero() || started.elapsed() >= timeout {
            return classify_shutdown_observation(responder, responder_pid_alive, None)
                .unwrap_or(ShutdownWaitOutcome::TimedOut);
        }
        let endpoint_identity = crate::daemon::client::status(sock_path)
            .ok()
            .map(|status| (status.pid, status.generation));
        if let Some(outcome) =
            classify_shutdown_observation(responder, responder_pid_alive, endpoint_identity)
        {
            return outcome;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return ShutdownWaitOutcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
}

pub(crate) fn latest_shutdown_phase(
    paths: &SoldrPaths,
    pid: u32,
    responder_generation: u64,
    responder_endpoint: Option<&str>,
) -> Option<String> {
    std::fs::read_to_string(daemon_lifecycle_log_path(paths))
        .ok()?
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|event| {
            let event_pid = event.get("target_pid")?.as_u64()? as u32;
            let generation = event.get("target_generation")?.as_u64()?;
            let endpoint = event
                .get("target_endpoint")
                .and_then(|value| value.as_str());
            let name = event.get("event")?.as_str()?;
            (event_pid == pid
                && generation == responder_generation
                && responder_endpoint.is_none_or(|expected| endpoint == Some(expected))
                && name.starts_with("shutdown-phase-"))
            .then(|| name.trim_start_matches("shutdown-phase-").to_string())
        })
}
