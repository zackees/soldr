//! Backend crash recovery policy for broker-managed services.
//!
//! This module only models state transitions. The caller remains responsible
//! for removing dead registry entries, spawning processes, and mapping refusal
//! decisions into wire-level `Refused` replies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::backend_registry::BackendKey;
use super::spawn_coordinator::DEFAULT_SPAWN_BUDGET_WINDOW;

/// Default delay before the broker retries a crashed backend once.
pub const DEFAULT_RECOVERY_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Default window before a backend-unavailable refusal can retry recovery.
pub const DEFAULT_RECOVERY_BUDGET_WINDOW: Duration = DEFAULT_SPAWN_BUDGET_WINDOW;

/// Recovery tuning for one backend key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendRecoveryPolicy {
    /// Delay reported before the one allowed retry should begin.
    pub retry_backoff: Duration,
    /// Window bounding crash retries and backend-unavailable retry-after hints.
    pub budget_window: Duration,
}

impl BackendRecoveryPolicy {
    /// Build a policy, clamping zero durations to a non-zero floor.
    pub fn new(retry_backoff: Duration, budget_window: Duration) -> Self {
        Self {
            retry_backoff: non_zero_duration(retry_backoff),
            budget_window: non_zero_duration(budget_window),
        }
    }
}

impl Default for BackendRecoveryPolicy {
    fn default() -> Self {
        Self {
            retry_backoff: DEFAULT_RECOVERY_RETRY_BACKOFF,
            budget_window: DEFAULT_RECOVERY_BUDGET_WINDOW,
        }
    }
}

/// Per-backend recovery state keyed by broker instance, service, and version.
#[derive(Debug)]
pub struct BackendRecoveryState {
    policy: BackendRecoveryPolicy,
    entries: HashMap<BackendKey, BackendRecoveryEntry>,
}

impl BackendRecoveryState {
    /// Create empty recovery state with the default policy.
    pub fn new() -> Self {
        Self::with_policy(BackendRecoveryPolicy::default())
    }

    /// Create empty recovery state with explicit policy settings.
    pub fn with_policy(policy: BackendRecoveryPolicy) -> Self {
        Self {
            policy,
            entries: HashMap::new(),
        }
    }

    /// Record one observed backend crash and return the recovery decision.
    ///
    /// The first crash in a budget window permits one retry after the configured
    /// backoff. A second crash in the same window is treated as backend
    /// unavailable and returns a retry-after hint for the rest of the window.
    pub fn record_crash(&mut self, key: BackendKey, now: Instant) -> BackendRecoveryDecision {
        let entry = self
            .entries
            .entry(key)
            .or_insert_with(|| BackendRecoveryEntry::new(now));
        entry.refresh(now, self.policy.budget_window);
        entry.crashes_in_window = entry.crashes_in_window.saturating_add(1);

        if entry.crashes_in_window == 1 {
            return BackendRecoveryDecision::Retry {
                retry_after: self.policy.retry_backoff,
                attempt: 1,
            };
        }

        BackendRecoveryDecision::Refuse {
            reason: BackendRecoveryRefusalReason::BackendUnavailable,
            retry_after: retry_after(entry.window_started_at, now, self.policy.budget_window),
        }
    }

    /// Reset crash recovery state after the backend is verified healthy again.
    pub fn record_success(&mut self, key: &BackendKey) {
        self.entries.remove(key);
    }
}

impl Default for BackendRecoveryState {
    fn default() -> Self {
        Self::new()
    }
}

/// Decision returned after an observed backend crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendRecoveryDecision {
    /// The broker may retry this backend once after `retry_after`.
    Retry {
        /// Delay before the retry should begin.
        retry_after: Duration,
        /// 1-based recovery attempt number.
        attempt: u32,
    },
    /// The backend should be refused as unavailable.
    Refuse {
        /// Refusal reason suitable for mapping to a Hello `Refused` reason.
        reason: BackendRecoveryRefusalReason,
        /// Retry-after hint for clients.
        retry_after: Duration,
    },
}

/// Refusal reason emitted by backend recovery state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendRecoveryRefusalReason {
    /// The backend crashed again after its one retry.
    BackendUnavailable,
}

#[derive(Clone, Debug)]
struct BackendRecoveryEntry {
    window_started_at: Instant,
    crashes_in_window: u32,
}

impl BackendRecoveryEntry {
    fn new(now: Instant) -> Self {
        Self {
            window_started_at: now,
            crashes_in_window: 0,
        }
    }

    fn refresh(&mut self, now: Instant, budget_window: Duration) {
        if elapsed_since(self.window_started_at, now) >= budget_window {
            self.window_started_at = now;
            self.crashes_in_window = 0;
        }
    }
}

fn non_zero_duration(duration: Duration) -> Duration {
    if duration.is_zero() {
        Duration::from_millis(1)
    } else {
        duration
    }
}

fn retry_after(window_started_at: Instant, now: Instant, budget_window: Duration) -> Duration {
    budget_window.saturating_sub(elapsed_since(window_started_at, now))
}

fn elapsed_since(started_at: Instant, now: Instant) -> Duration {
    now.checked_duration_since(started_at)
        .unwrap_or(Duration::ZERO)
}
