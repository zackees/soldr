//! Keep the daemon route alive for the tail of `soldr cook` (soldr#3117).
//!
//! The broker reaps a daemon route once every process that asked for it has
//! exited and `broker_reaper::DEFAULT_GRACE` has elapsed. During Phase 2 the
//! requesters are the compiler wrapper re-entries cargo spawns; the `soldr
//! cook` process itself never asks for the route. When the compile finishes
//! those requesters are gone, and the post-compile tail -- target trim, the
//! multi-hundred-MiB archive pack -- routinely outlives the grace window. The
//! pre-flight `CookLookup` then still succeeds, the pack runs, and the closing
//! `CookRecord` finds the daemon reaped: the artifact is written but never
//! indexed, so the next run can never hydrate from it. In CI this made the
//! stable-tree cook rebuild on every run (soldr#3117).
//!
//! The same reap also blanks the other end of the mechanism: the front door's
//! hydrate pre-flight asks the daemon whether the index has entries before it
//! computes a lookup key, and with no daemon it silently skips, so a cook that
//! starts more than the grace window after the previous compile never even
//! tries to restore.
//!
//! The fix is for the cook process to ask for the route itself, once, before
//! Phase 2. The broker records the requester from kernel-supplied peer
//! credentials, so a live `soldr cook` keeps the route out of the reaper's
//! reach for as long as it runs -- through the hydrate pre-flight, the
//! compile, the trim, the pack, and the closing record -- and a route already
//! reaped is relaunched by the same request.

use crate::core::SoldrError;

/// Register this process as a requester of the daemon route and make sure the
/// route is ready. Returns the service name on success.
///
/// Best-effort by contract: the caller reports the error and continues; the
/// compile then fails or succeeds on its own terms, and the closing
/// `CookRecord` still classifies the index outcome on its own.
pub(crate) fn hold_daemon_route_for_cook() -> Result<String, SoldrError> {
    let (_daemon_path, service_name) = crate::zccache::register_broker_daemon_service()?;
    // Same as `soldr daemon start`: every later control call in this process
    // (the hydrate pre-flight's status probe, CookLookup, CookRecord) and the
    // compiler re-entries cargo spawns must resolve the route this process
    // holds, not re-derive one from a sibling image or a stale route claim.
    std::env::set_var(
        crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
        &service_name,
    );
    let ready_route = crate::session_transport::ensure_broker_route(
        &service_name,
        crate::DAEMON_START_ROUTE_BUDGET,
    )
    .map_err(|err| {
        SoldrError::Other(format!(
            "broker could not provide soldr-daemon route {service_name} for soldr cook: {err}"
        ))
    })?;
    // A route the broker has just launched answers its first control
    // requests slowly (state store open, image hash); a 2 s status probe
    // against it times out, and the hydrate pre-flight would read that as
    // "no daemon". Wait for status-readiness the way `soldr daemon start`
    // does.
    let paths = crate::core::SoldrPaths::new()?;
    let sock = crate::daemon::client::default_sock_path(&paths);
    crate::daemon::lifecycle::status_after_negotiated_route(
        &paths,
        &sock,
        &ready_route.backend_pipe,
        &ready_route.daemon_version,
        crate::daemon::lifecycle::START_STATUS_READY_TIMEOUT,
    )
    .map_err(|err| {
        SoldrError::Other(format!(
            "soldr-daemon route {service_name} was published but did not become status-ready for soldr cook: {err:?}"
        ))
    })?;
    Ok(service_name)
}
