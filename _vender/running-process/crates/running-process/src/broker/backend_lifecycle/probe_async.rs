//! Async flavor of the endpoint probe used by `BackendHandle` (#414).
//!
//! Mirrors the blocking [`probe_endpoint`] / [`probe_endpoint_response`]
//! contract from [`super::probe`] but exposes an `.await`-able
//! signature so tokio daemons (zccache, soldr, clud) don't have to
//! wrap the blocking surface in `spawn_blocking` at every call site.
//!
//! ## Implementation note (frozen wire, opt-in async)
//!
//! v1 ships the canonical probe wire as a synchronous,
//! deadline-bound read/write loop in [`super::probe`]. That code path
//! is the wire-of-record — it owns the nonblocking flag, the manual
//! poll cadence, and the precise deadline behavior that all of v1's
//! squat-detection and stale-manifest tests pin against. Re-implementing
//! it against `tokio::io::AsyncRead`/`AsyncWrite` would duplicate the
//! wire surface for no observable behavior gain (the probe is one
//! short request/response with a 500ms cap) and risk drift.
//!
//! Instead, the async flavor hands the blocking probe to
//! `tokio::task::spawn_blocking`. The caller gets the async surface
//! (so an `await` from a tokio task replaces a manual
//! `spawn_blocking` wrap at the call site), the wire is unchanged,
//! and the runtime worker thread is freed during the wait. This is
//! the same contract async daemons want — the `spawn_blocking`
//! requirement called out on the blocking variants in
//! `BackendHandle` becomes our internal detail.
//!
//! [`probe_endpoint`]: super::probe::probe_endpoint
//! [`probe_endpoint_response`]: super::probe::probe_endpoint_response

use std::time::Duration;

use crate::broker::backend_lifecycle::identity::DaemonProcess;
use crate::broker::backend_lifecycle::probe::{
    self, EndpointProbeError, ProbeError, DEFAULT_ENDPOINT_PROBE_TIMEOUT,
};
use crate::broker::backend_lifecycle::verify_pid::{self, ProcessHandle};
use crate::broker::protocol::Endpoint;

/// Async counterpart of
/// [`probe_endpoint`](super::probe::probe_endpoint).
///
/// Performs the same endpoint-identity tuple check, PID verification,
/// and active nonce probe as the blocking variant — but the synchronous
/// nonce probe (the only piece that does IO) runs on
/// `tokio::task::spawn_blocking` so the caller's task yields the runtime
/// worker thread during the wait. PID verification is cheap and stays
/// inline on the calling task.
pub async fn probe_endpoint_async(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<ProcessHandle, ProbeError> {
    if !probe::same_endpoint(endpoint, &expected.ipc_endpoint) {
        return Err(ProbeError::EndpointMismatch);
    }
    // PID verification is fast (process-table lookup + exe hash);
    // running it inline keeps the platform-specific `ProcessHandle`
    // (which contains a non-Send Windows HANDLE) on the caller's
    // task instead of crossing the spawn_blocking boundary.
    let process_handle =
        verify_pid::verify_daemon_process(expected).map_err(ProbeError::VerifyPid)?;
    probe_endpoint_response_async(endpoint, expected).await?;
    Ok(process_handle)
}

/// Async counterpart of
/// [`probe_endpoint_response`](super::probe::probe_endpoint_response).
pub async fn probe_endpoint_response_async(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<(), EndpointProbeError> {
    probe_endpoint_response_with_timeout_async(endpoint, expected, DEFAULT_ENDPOINT_PROBE_TIMEOUT)
        .await
}

/// Timed variant of [`probe_endpoint_response_async`].
pub async fn probe_endpoint_response_with_timeout_async(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
    timeout: Duration,
) -> Result<(), EndpointProbeError> {
    let endpoint = endpoint.clone();
    let expected = expected.clone();
    match tokio::task::spawn_blocking(move || {
        probe::probe_endpoint_response_with_timeout(&endpoint, &expected, timeout)
    })
    .await
    {
        Ok(result) => result,
        Err(join_err) => Err(EndpointProbeError::Io(std::io::Error::other(format!(
            "async probe worker thread panicked or was cancelled: {join_err}"
        )))),
    }
}
