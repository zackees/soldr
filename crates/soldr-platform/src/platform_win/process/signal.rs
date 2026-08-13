//! Graceful-termination signal handling on Windows.
//!
//! Windows has no POSIX SIGTERM; console interrupts are already covered
//! by the host-neutral `tokio::signal::ctrl_c`. This future never
//! resolves, so the daemon's terminate-hook task simply parks until it
//! is aborted at shutdown.

/// Never resolves on Windows. Kept on the facade surface so the daemon
/// can register its SIGTERM drain hook unconditionally.
pub async fn wait_for_terminate_signal() -> bool {
    std::future::pending::<()>().await;
    false
}
