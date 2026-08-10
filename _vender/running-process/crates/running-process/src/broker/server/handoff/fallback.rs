//! Fallback policy for optional broker-to-backend connection handoff.
//!
//! This module does not transfer handles. It keeps the decision surface for
//! Phase 6 small: decide whether a handoff attempt is allowed, record failed
//! attempts, and always translate handoff failures into a silent reconnect
//! fallback for the client.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::super::backend_registry::BackendKey;

/// Default number of failed handoff attempts allowed per backend/window.
pub const DEFAULT_HANDOFF_FAILED_ATTEMPTS_PER_WINDOW: usize = 8;

/// Default window used to rate-limit failed handoff attempts.
pub const DEFAULT_HANDOFF_FAILED_ATTEMPT_WINDOW: Duration = Duration::from_secs(30);

/// Runtime bounds for optional handoff fallback behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffFallbackPolicy {
    /// Maximum failed handoff attempts before attempt suppression begins.
    pub max_failed_attempts_per_window: usize,
    /// Window over which failed handoff attempts are counted.
    pub failed_attempt_window: Duration,
}

impl HandoffFallbackPolicy {
    /// Build a policy, clamping zero values to safe non-zero defaults.
    pub fn new(max_failed_attempts_per_window: usize, failed_attempt_window: Duration) -> Self {
        Self {
            max_failed_attempts_per_window: max_failed_attempts_per_window.max(1),
            failed_attempt_window: if failed_attempt_window.is_zero() {
                Duration::from_millis(1)
            } else {
                failed_attempt_window
            },
        }
    }
}

impl Default for HandoffFallbackPolicy {
    fn default() -> Self {
        Self {
            max_failed_attempts_per_window: DEFAULT_HANDOFF_FAILED_ATTEMPTS_PER_WINDOW,
            failed_attempt_window: DEFAULT_HANDOFF_FAILED_ATTEMPT_WINDOW,
        }
    }
}

/// Inputs that decide whether a specific client request may attempt handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffAttemptInputs {
    /// Whether the client advertised support for staying on a handed-off pipe.
    pub client_supports_handoff: bool,
    /// Whether the matched service permits the handoff optimization.
    pub service_policy_enabled: bool,
    /// Whether fd/handle pressure has temporarily disabled handoff attempts.
    pub fd_pressure_disabled: bool,
    /// Whether this backend was adopted after broker restart rather than spawned by this broker.
    pub backend_adopted_existing: bool,
}

impl HandoffAttemptInputs {
    /// Build inputs for one handoff decision.
    pub fn new(
        client_supports_handoff: bool,
        service_policy_enabled: bool,
        fd_pressure_disabled: bool,
    ) -> Self {
        Self {
            client_supports_handoff,
            service_policy_enabled,
            fd_pressure_disabled,
            backend_adopted_existing: false,
        }
    }

    /// Inputs for the common path where both client and service permit handoff.
    pub fn enabled() -> Self {
        Self::new(true, true, false)
    }

    /// Inputs for an adopted backend that must use reconnect fallback.
    pub fn adopted_backend(client_supports_handoff: bool) -> Self {
        Self {
            client_supports_handoff,
            service_policy_enabled: true,
            fd_pressure_disabled: false,
            backend_adopted_existing: true,
        }
    }
}

/// Decision produced before or after an optional handoff attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoffAttemptDecision {
    /// The broker may attempt platform-specific handoff.
    Attempt,
    /// The broker must reply with the backend endpoint and let the client reconnect.
    FallbackToReconnect(HandoffFallbackDecision),
}

impl HandoffAttemptDecision {
    /// Return the fallback decision when this is a reconnect fallback.
    pub fn fallback(&self) -> Option<&HandoffFallbackDecision> {
        match self {
            Self::Attempt => None,
            Self::FallbackToReconnect(decision) => Some(decision),
        }
    }
}

/// Client-visible behavior when broker handoff is skipped or fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffFallbackDecision {
    /// Reason retained for broker logging and metrics.
    pub reason: HandoffFallbackReason,
    /// Time until handoff attempts should be retried for this backend, when rate-limited.
    pub retry_after: Option<Duration>,
}

impl HandoffFallbackDecision {
    /// Build a reconnect fallback with no retry-after hint.
    pub fn new(reason: HandoffFallbackReason) -> Self {
        Self {
            reason,
            retry_after: None,
        }
    }

    /// Build a reconnect fallback that carries a rate-limit retry-after hint.
    pub fn with_retry_after(reason: HandoffFallbackReason, retry_after: Duration) -> Self {
        Self {
            reason,
            retry_after: Some(retry_after),
        }
    }

    /// Return true because fallback must send `backend_pipe` for reconnect.
    pub fn uses_backend_reconnect(&self) -> bool {
        true
    }

    /// Return false because handoff fallback is an optimization failure, not a client error.
    pub fn sends_client_error(&self) -> bool {
        false
    }
}

/// Broker-side reason for falling back from handoff to client reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffFallbackReason {
    /// The client did not advertise handoff support.
    ClientUnsupported,
    /// The matched service disabled the handoff optimization.
    ServicePolicyDisabled,
    /// Handoff was temporarily disabled because fd/handle pressure is high.
    FdPressureDisabled,
    /// Failed handoff attempts reached the policy limit for this backend/window.
    FailedAttemptRateLimited,
    /// The platform handoff API denied access to duplicate or pass the handle.
    PermissionDenied,
    /// The broker or backend integrity boundary refused the handoff.
    IntegrityMismatch,
    /// The backend did not acknowledge the handed-off connection in time.
    BackendAckTimeout,
    /// The broker adopted an existing backend and cannot transfer handles from the old owner.
    AdoptedBackend,
}

/// Failure observed after a platform-specific handoff attempt was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffAttemptFailure {
    /// The platform denied handle/fd passing permissions.
    PermissionDenied,
    /// The broker and backend integrity levels or trust domains were incompatible.
    IntegrityMismatch,
    /// The backend did not acknowledge the handoff before the broker deadline.
    BackendAckTimeout,
}

impl From<HandoffAttemptFailure> for HandoffFallbackReason {
    fn from(value: HandoffAttemptFailure) -> Self {
        match value {
            HandoffAttemptFailure::PermissionDenied => Self::PermissionDenied,
            HandoffAttemptFailure::IntegrityMismatch => Self::IntegrityMismatch,
            HandoffAttemptFailure::BackendAckTimeout => Self::BackendAckTimeout,
        }
    }
}

/// Per-backend state for suppressing repeatedly failing handoff attempts.
#[derive(Debug)]
pub struct HandoffFallbackState {
    policy: HandoffFallbackPolicy,
    failed_attempts: HashMap<BackendKey, FailedAttemptWindow>,
}

impl HandoffFallbackState {
    /// Create state with default fallback bounds.
    pub fn new() -> Self {
        Self::with_policy(HandoffFallbackPolicy::default())
    }

    /// Create state with explicit fallback bounds.
    pub fn with_policy(policy: HandoffFallbackPolicy) -> Self {
        Self {
            policy,
            failed_attempts: HashMap::new(),
        }
    }

    /// Return the active policy.
    pub fn policy(&self) -> HandoffFallbackPolicy {
        self.policy
    }

    /// Decide whether this request may attempt handoff.
    pub fn should_attempt(
        &mut self,
        backend: &BackendKey,
        inputs: HandoffAttemptInputs,
        now: Instant,
    ) -> HandoffAttemptDecision {
        if !inputs.client_supports_handoff {
            return fallback(HandoffFallbackReason::ClientUnsupported);
        }
        if !inputs.service_policy_enabled {
            return fallback(HandoffFallbackReason::ServicePolicyDisabled);
        }
        if inputs.fd_pressure_disabled {
            return fallback(HandoffFallbackReason::FdPressureDisabled);
        }
        if inputs.backend_adopted_existing {
            return fallback(HandoffFallbackReason::AdoptedBackend);
        }

        match self.rate_limit_for(backend, now) {
            Some(retry_after) => HandoffAttemptDecision::FallbackToReconnect(
                HandoffFallbackDecision::with_retry_after(
                    HandoffFallbackReason::FailedAttemptRateLimited,
                    retry_after,
                ),
            ),
            None => HandoffAttemptDecision::Attempt,
        }
    }

    /// Record a failed handoff attempt and return the silent reconnect fallback.
    pub fn record_failed_attempt(
        &mut self,
        backend: BackendKey,
        failure: HandoffAttemptFailure,
        now: Instant,
    ) -> HandoffAttemptDecision {
        let entry = self
            .failed_attempts
            .entry(backend)
            .or_insert_with(|| FailedAttemptWindow::new(now));
        entry.refresh_if_expired(now, self.policy.failed_attempt_window);
        entry.count = entry
            .count
            .saturating_add(1)
            .min(self.policy.max_failed_attempts_per_window);

        fallback(failure.into())
    }

    /// Clear failed-attempt state after a successful handoff.
    pub fn record_success(&mut self, backend: &BackendKey) {
        self.failed_attempts.remove(backend);
    }

    /// Return the bounded failed-attempt count for a backend.
    pub fn failed_attempt_count(&mut self, backend: &BackendKey, now: Instant) -> usize {
        let policy_window = self.policy.failed_attempt_window;
        let Some(entry) = self.failed_attempts.get_mut(backend) else {
            return 0;
        };
        entry.refresh_if_expired(now, policy_window);
        entry.count
    }

    fn rate_limit_for(&mut self, backend: &BackendKey, now: Instant) -> Option<Duration> {
        let policy = self.policy;
        let entry = self.failed_attempts.get_mut(backend)?;
        entry.refresh_if_expired(now, policy.failed_attempt_window);
        if entry.count < policy.max_failed_attempts_per_window {
            return None;
        }

        Some(entry.retry_after(now, policy.failed_attempt_window))
    }
}

impl Default for HandoffFallbackState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
struct FailedAttemptWindow {
    started_at: Instant,
    count: usize,
}

impl FailedAttemptWindow {
    fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            count: 0,
        }
    }

    fn refresh_if_expired(&mut self, now: Instant, window: Duration) {
        if now
            .checked_duration_since(self.started_at)
            .is_some_and(|elapsed| elapsed >= window)
        {
            self.started_at = now;
            self.count = 0;
        }
    }

    fn retry_after(&self, now: Instant, window: Duration) -> Duration {
        let elapsed = now
            .checked_duration_since(self.started_at)
            .unwrap_or(Duration::ZERO);
        window.saturating_sub(elapsed)
    }
}

fn fallback(reason: HandoffFallbackReason) -> HandoffAttemptDecision {
    HandoffAttemptDecision::FallbackToReconnect(HandoffFallbackDecision::new(reason))
}
