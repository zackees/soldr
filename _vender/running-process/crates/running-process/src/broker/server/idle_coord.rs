//! Broker-side backend idle coordination model.
//!
//! This module is deliberately a pure state model. It tracks monotonic
//! activity timestamps by backend key and reports which running backends should
//! receive a future `Quiesce(IdleTimeout)` lifecycle broadcast.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::backend_registry::BackendKey;
use super::broadcast::QuiesceReason;

/// Default idle timeout for broker-managed backends.
pub const DEFAULT_BACKEND_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle timeout policy for broker-managed backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendIdlePolicy {
    /// Idle duration after which a running backend should be quiesced.
    pub default_idle_timeout: Duration,
}

impl BackendIdlePolicy {
    /// Build a policy, clamping zero timeout to a non-zero floor.
    pub fn new(default_idle_timeout: Duration) -> Self {
        Self {
            default_idle_timeout: non_zero_duration(default_idle_timeout),
        }
    }

    /// Return the configured default idle timeout.
    pub fn default_idle_timeout(&self) -> Duration {
        self.default_idle_timeout
    }
}

impl Default for BackendIdlePolicy {
    fn default() -> Self {
        Self {
            default_idle_timeout: DEFAULT_BACKEND_IDLE_TIMEOUT,
        }
    }
}

/// Coordinates idle deadlines for backend keys.
#[derive(Debug)]
pub struct BackendIdleCoordinator {
    policy: BackendIdlePolicy,
    entries: HashMap<BackendKey, BackendIdleEntry>,
}

impl BackendIdleCoordinator {
    /// Create an empty coordinator with the default idle policy.
    pub fn new() -> Self {
        Self::with_policy(BackendIdlePolicy::default())
    }

    /// Create an empty coordinator with an explicit idle policy.
    pub fn with_policy(policy: BackendIdlePolicy) -> Self {
        Self {
            policy,
            entries: HashMap::new(),
        }
    }

    /// Record backend activity and reset its idle deadline.
    pub fn mark_activity(&mut self, key: BackendKey, now: Instant) {
        self.mark_activity_with_timeout(key, now, self.policy.default_idle_timeout);
    }

    /// Record backend activity with a backend-specific timeout.
    pub fn mark_activity_with_timeout(
        &mut self,
        key: BackendKey,
        now: Instant,
        idle_timeout: Duration,
    ) {
        self.entries.insert(
            key,
            BackendIdleEntry {
                last_active_at: now,
                idle_timeout: non_zero_duration(idle_timeout),
                state: BackendIdleState::Running,
            },
        );
    }

    /// Mark a backend as draining after a quiesce request has been emitted.
    ///
    /// Returns true when the backend key was tracked.
    pub fn mark_draining(&mut self, key: &BackendKey) -> bool {
        self.mark_state(key, BackendIdleState::Draining)
    }

    /// Mark a backend as quiesced after it has drained.
    ///
    /// Returns true when the backend key was tracked.
    pub fn mark_quiesced(&mut self, key: &BackendKey) -> bool {
        self.mark_state(key, BackendIdleState::Quiesced)
    }

    /// Remove a backend from idle tracking.
    pub fn remove_backend(&mut self, key: &BackendKey) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Collect running backends whose idle timeout has elapsed.
    ///
    /// Collected backends are moved to `draining` so repeated collection does
    /// not emit duplicate quiesce requests unless fresh activity is recorded.
    pub fn collect_due_for_quiesce(&mut self, now: Instant) -> Vec<BackendIdleDue> {
        let mut due = Vec::new();

        for (key, entry) in &mut self.entries {
            if entry.state != BackendIdleState::Running {
                continue;
            }

            let idle_for = elapsed_since(entry.last_active_at, now);
            if idle_for < entry.idle_timeout {
                continue;
            }

            entry.state = BackendIdleState::Draining;
            due.push(BackendIdleDue {
                key: key.clone(),
                idle_for,
                configured_timeout: entry.idle_timeout,
                reason: QuiesceReason::IdleTimeout,
            });
        }

        due
    }

    fn mark_state(&mut self, key: &BackendKey, state: BackendIdleState) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        entry.state = state;
        true
    }
}

impl Default for BackendIdleCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Backend due for idle quiesce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendIdleDue {
    /// Backend key that crossed its idle deadline.
    pub key: BackendKey,
    /// Monotonic elapsed time since the backend was last marked active.
    pub idle_for: Duration,
    /// Timeout configured for this backend entry.
    pub configured_timeout: Duration,
    /// Quiesce reason for the future lifecycle broadcast call.
    pub reason: QuiesceReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendIdleState {
    Running,
    Draining,
    Quiesced,
}

#[derive(Clone, Debug)]
struct BackendIdleEntry {
    last_active_at: Instant,
    idle_timeout: Duration,
    state: BackendIdleState,
}

fn non_zero_duration(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}

fn elapsed_since(started_at: Instant, now: Instant) -> Duration {
    now.checked_duration_since(started_at)
        .unwrap_or(Duration::ZERO)
}
