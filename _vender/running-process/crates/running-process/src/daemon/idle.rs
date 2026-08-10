use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

/// Tracks last activity and triggers shutdown when idle too long.
pub struct IdleMonitor {
    /// Unix timestamp in milliseconds of last activity
    last_activity_ms: Arc<AtomicU64>,
    /// How long to wait before idle shutdown
    timeout: Duration,
    /// Shutdown signal sender
    shutdown_tx: watch::Sender<bool>,
}

impl IdleMonitor {
    pub fn new(timeout_secs: u64, shutdown_tx: watch::Sender<bool>) -> Self {
        let now_ms = now_millis();
        Self {
            last_activity_ms: Arc::new(AtomicU64::new(now_ms)),
            timeout: Duration::from_secs(timeout_secs),
            shutdown_tx,
        }
    }

    /// Call this on every IPC request to reset the idle timer.
    pub fn touch(&self) {
        self.last_activity_ms.store(now_millis(), Ordering::Relaxed);
    }

    /// Get a handle that can be cloned and passed to connection handlers.
    pub fn handle(&self) -> IdleHandle {
        IdleHandle {
            last_activity_ms: Arc::clone(&self.last_activity_ms),
        }
    }

    /// Run the idle check loop. This should be spawned as a tokio task.
    /// Checks every 30 seconds. Triggers shutdown when idle timeout exceeded.
    pub async fn run(&self) {
        let check_interval = Duration::from_secs(30);
        let mut interval = tokio::time::interval(check_interval);

        loop {
            interval.tick().await;

            let last_ms = self.last_activity_ms.load(Ordering::Relaxed);
            let now_ms = now_millis();

            let idle_duration = Duration::from_millis(now_ms.saturating_sub(last_ms));

            if idle_duration >= self.timeout {
                tracing::info!(
                    "idle timeout reached ({:.0}s idle, {:.0}s limit) — shutting down",
                    idle_duration.as_secs_f64(),
                    self.timeout.as_secs_f64()
                );
                let _ = self.shutdown_tx.send(true);
                break;
            }
        }
    }
}

/// Lightweight, cloneable handle for touching the idle timer from connection handlers.
#[derive(Clone)]
pub struct IdleHandle {
    last_activity_ms: Arc<AtomicU64>,
}

impl IdleHandle {
    pub fn touch(&self) {
        self.last_activity_ms.store(now_millis(), Ordering::Relaxed);
    }
}

/// Current time as milliseconds since UNIX epoch.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_millis_is_reasonable() {
        let ms = now_millis();
        // Should be after 2024-01-01 (1_704_067_200_000 ms since epoch)
        assert!(ms > 1_704_067_200_000);
    }

    #[test]
    fn idle_handle_touch_updates_timestamp() {
        let (tx, _rx) = watch::channel(false);
        let monitor = IdleMonitor::new(60, tx);
        let handle = monitor.handle();

        // Store an old timestamp
        monitor.last_activity_ms.store(1_000_000, Ordering::Relaxed);

        // Touch via handle
        handle.touch();

        let updated = monitor.last_activity_ms.load(Ordering::Relaxed);
        assert!(updated > 1_000_000);
    }

    #[test]
    fn monitor_touch_updates_timestamp() {
        let (tx, _rx) = watch::channel(false);
        let monitor = IdleMonitor::new(60, tx);

        monitor.last_activity_ms.store(1_000_000, Ordering::Relaxed);
        monitor.touch();

        let updated = monitor.last_activity_ms.load(Ordering::Relaxed);
        assert!(updated > 1_000_000);
    }

    #[tokio::test]
    async fn idle_monitor_triggers_shutdown() {
        let (tx, mut rx) = watch::channel(false);

        // Use a tiny timeout so the test completes quickly
        let monitor = IdleMonitor::new(0, tx);

        // Set last activity far in the past
        monitor.last_activity_ms.store(0, Ordering::Relaxed);

        // Run the monitor — it should trigger shutdown on the first tick
        let monitor_task = tokio::spawn(async move {
            monitor.run().await;
        });

        // Wait for the shutdown signal
        let _ = rx.changed().await;
        assert!(*rx.borrow());

        // The task should complete
        let _ = monitor_task.await;
    }

    #[tokio::test]
    async fn idle_monitor_does_not_trigger_when_active() {
        let (tx, mut rx) = watch::channel(false);

        // 10-second timeout — longer than the test will run
        let monitor = IdleMonitor::new(10, tx);
        let handle = monitor.handle();

        let monitor_task = tokio::spawn(async move {
            monitor.run().await;
        });

        // Keep touching the monitor
        handle.touch();

        // Wait a short time — shutdown should NOT trigger
        let result = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
        assert!(result.is_err(), "shutdown should not have triggered");

        // Abort the monitor task since it will run for 10s otherwise
        monitor_task.abort();
    }
}
