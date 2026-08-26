use super::current_version_claim_matches;
use crate::core::SoldrPaths;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};

/// Bound for the short transition where a broker route exists but its prior
/// endpoint generation is still retiring.
pub const STATUS_RETIRING_RETRY_TIMEOUT: Duration = Duration::from_secs(5);

fn retiring_endpoint_error(error: &crate::daemon::client::ClientError) -> bool {
    matches!(error, crate::daemon::client::ClientError::Io(io)
        if matches!(io.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof | ErrorKind::WouldBlock))
}

pub(crate) fn status_with_retiring_retry<F, R>(
    mut status: F,
    mut retiring_route_is_verified: R,
    timeout: Duration,
    require_verified_identity: bool,
) -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError>
where
    F: FnMut() -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError>,
    R: FnMut() -> Option<(u32, u64)>,
{
    let started = Instant::now();
    loop {
        let expected = retiring_route_is_verified();
        match status() {
            Ok(status)
                if expected.is_some_and(|identity| identity == (status.pid, status.generation))
                    || (!require_verified_identity && expected.is_none()) =>
            {
                return Ok(status)
            }
            Ok(_status) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(status) => {
                return Err(crate::daemon::client::ClientError::Protocol(format!(
                    "status generation pid={} generation={} did not match the verified route claim",
                    status.pid, status.generation
                )))
            }
            Err(error)
                if retiring_endpoint_error(&error)
                    && expected.is_some()
                    && started.elapsed() < timeout =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn status_after_route_ready(
    paths: &SoldrPaths,
    sock_path: &Path,
    timeout: Duration,
) -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError> {
    status_with_retiring_retry(
        || crate::daemon::client::status(sock_path),
        || verified_route_identity(paths),
        timeout,
        false,
    )
}

pub fn status_after_negotiated_route(
    paths: &SoldrPaths,
    sock_path: &Path,
    backend_pipe: &str,
    daemon_version: &str,
    timeout: Duration,
) -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError> {
    status_with_retiring_retry(
        || crate::daemon::client::status(sock_path),
        || negotiated_route_identity(paths, backend_pipe, daemon_version),
        timeout,
        true,
    )
}

fn negotiated_route_identity(
    paths: &SoldrPaths,
    backend_pipe: &str,
    daemon_version: &str,
) -> Option<(u32, u64)> {
    let claim = crate::daemon::backend_handle_adoption::read_broker_route_claim(paths)
        .ok()
        .flatten()?;
    (daemon_version == crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION
        && current_version_claim_matches(paths)
        && claim.ipc_endpoint.path == backend_pipe
        && claim.boot_id == running_process::broker::host_identity::current().boot_id)
        .then_some((claim.pid, claim.started_at_unix_ms))
}

fn verified_route_identity(paths: &SoldrPaths) -> Option<(u32, u64)> {
    let claim = crate::daemon::backend_handle_adoption::read_broker_route_claim(paths)
        .ok()
        .flatten()?;
    if !current_version_claim_matches(paths) {
        return None;
    }
    let handle = crate::daemon::backend_handle_adoption::probe_soldr_daemon(paths)?;
    (handle.pid() == claim.pid).then_some((claim.pid, claim.started_at_unix_ms))
}
