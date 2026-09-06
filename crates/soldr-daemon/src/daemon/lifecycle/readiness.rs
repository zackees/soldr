use super::current_version_claim_matches;
use crate::core::SoldrPaths;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};

/// Bound for the short transition where a broker route exists but its prior
/// endpoint generation is still retiring.
pub const STATUS_RETIRING_RETRY_TIMEOUT: Duration = Duration::from_secs(5);

/// Explicit start waits longer than an ordinary status query because a route
/// can pass the broker's session-endpoint probe before its control endpoint is
/// accepting requests on a heavily loaded host.
pub const START_STATUS_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Transport faults that mean "this endpoint is not answering *yet*", as
/// opposed to "this endpoint answered and said no".
///
/// `WouldBlock` and `TimedOut` are the two spellings of a socket/pipe read
/// deadline — which one surfaces depends on the platform, exactly as
/// [`crate::daemon::client`]'s own `is_deadline_error` documents — and
/// `client::status` runs on a 2 s reply budget, so a daemon that has accepted
/// the connection but is still initializing produces one of them.
fn retiring_endpoint_error(error: &crate::daemon::client::ClientError) -> bool {
    matches!(error, crate::daemon::client::ClientError::Io(io)
        if matches!(io.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof | ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

/// A negotiated-route readiness wait that ran out of budget, carrying what it
/// observed so the next diagnosis does not start from a bare `WouldBlock`
/// (soldr#3126).
#[derive(Debug)]
pub struct NegotiatedRouteError {
    pub error: crate::daemon::client::ClientError,
    pub waited: Duration,
    pub probes: u32,
    /// Whether a matching broker route claim ever became readable during the
    /// wait. `false` means the daemon never got as far as publishing its
    /// identity, which points at daemon start-up, not at the control endpoint.
    pub claim_observed: bool,
}

impl std::fmt::Display for NegotiatedRouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "waited {:.1}s across {} status probes; matching route claim was {}: {:?}",
            self.waited.as_secs_f64(),
            self.probes,
            if self.claim_observed {
                "observed"
            } else {
                "never observed"
            },
            self.error,
        )
    }
}

impl std::error::Error for NegotiatedRouteError {}

struct RetryOutcome {
    result: Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError>,
    waited: Duration,
    probes: u32,
    claim_observed: bool,
}

fn retry_status_until_ready<F, R>(
    mut status: F,
    mut retiring_route_is_verified: R,
    timeout: Duration,
    require_verified_identity: bool,
) -> RetryOutcome
where
    F: FnMut() -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError>,
    R: FnMut() -> Option<(u32, u64)>,
{
    let started = Instant::now();
    let mut probes = 0u32;
    let mut claim_observed = false;
    let result = loop {
        let expected = retiring_route_is_verified();
        claim_observed |= expected.is_some();
        probes += 1;
        match status() {
            Ok(status)
                if expected.is_some_and(|identity| identity == (status.pid, status.generation))
                    || (!require_verified_identity && expected.is_none()) =>
            {
                break Ok(status)
            }
            Ok(_status) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(status) => {
                break Err(crate::daemon::client::ClientError::Protocol(format!(
                    "status generation pid={} generation={} did not match the verified route claim",
                    status.pid, status.generation
                )))
            }
            // soldr#3126: on the negotiated path the caller has just been told
            // by the broker that the route is published and is explicitly
            // waiting for readiness, so a transient transport fault must be
            // retried for the whole budget even before the route claim is
            // readable. The daemon writes that claim part-way through
            // `run_server`, after hashing its own image and binding the
            // session listener, so the first probes legitimately land in a
            // window where `expected` is still `None` — gating the retry on
            // `expected.is_some()` made the very first 2 s reply timeout
            // terminal and left the 30 s budget unused. The unverified path
            // (`require_verified_identity == false`) keeps demanding identity
            // evidence before it retries.
            Err(error)
                if retiring_endpoint_error(&error)
                    && (expected.is_some() || require_verified_identity)
                    && started.elapsed() < timeout =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => break Err(error),
        }
    };
    RetryOutcome {
        result,
        waited: started.elapsed(),
        probes,
        claim_observed,
    }
}

pub(crate) fn status_with_retiring_retry<F, R>(
    status: F,
    retiring_route_is_verified: R,
    timeout: Duration,
    require_verified_identity: bool,
) -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError>
where
    F: FnMut() -> Result<crate::daemon::protocol::StatusInfo, crate::daemon::client::ClientError>,
    R: FnMut() -> Option<(u32, u64)>,
{
    retry_status_until_ready(
        status,
        retiring_route_is_verified,
        timeout,
        require_verified_identity,
    )
    .result
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
) -> Result<crate::daemon::protocol::StatusInfo, NegotiatedRouteError> {
    let outcome = retry_status_until_ready(
        || crate::daemon::client::status(sock_path),
        || negotiated_route_identity(paths, backend_pipe, daemon_version),
        timeout,
        true,
    );
    match outcome.result {
        Ok(status) => Ok(status),
        Err(error) => Err(NegotiatedRouteError {
            error,
            waited: outcome.waited,
            probes: outcome.probes,
            claim_observed: outcome.claim_observed,
        }),
    }
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
