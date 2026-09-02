//! SIGTERM detection on Windows (soldr#3059; formerly "graceful-termination
//! signal handling" under issue #1286 — see `platform/process/signal.rs`
//! for the history of what the caller now does with this).
//!
//! Windows has no POSIX SIGTERM; console interrupts are already covered
//! by the host-neutral `tokio::signal::ctrl_c`. This future never
//! resolves, so the daemon's terminate-hook task simply parks until it
//! is aborted at shutdown.

/// Never resolves on Windows. Kept on the facade surface so the daemon
/// can register its SIGTERM fast-exit hook unconditionally.
pub async fn wait_for_terminate_signal() -> bool {
    std::future::pending::<()>().await;
    false
}
