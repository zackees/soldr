//! SIGTERM wait for the daemon's fast-exit hook (soldr#3059; formerly a
//! graceful-drain hook under issue #1286 — see `platform/process/signal.rs`
//! for the history).

/// Wait for SIGTERM, returning `true` once a terminate signal arrived.
/// `false` when the handler cannot be registered (the caller then runs
/// without the TERM hook — same behavior as a host without signals).
pub async fn wait_for_terminate_signal() -> bool {
    let Ok(mut term) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        return false;
    };
    term.recv().await.is_some()
}
