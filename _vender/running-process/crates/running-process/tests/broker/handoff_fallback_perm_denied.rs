#![cfg(feature = "client")]

use std::time::{Duration, Instant};

use running_process::broker::server::{
    BackendKey, BrokerInstanceKey, HandoffAttemptDecision, HandoffAttemptFailure,
    HandoffAttemptInputs, HandoffFallbackPolicy, HandoffFallbackReason, HandoffFallbackState,
};

fn key(version: &str) -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, "zccache", version)
}

fn assert_silent_reconnect(
    decision: HandoffAttemptDecision,
    reason: HandoffFallbackReason,
) -> Option<Duration> {
    let HandoffAttemptDecision::FallbackToReconnect(fallback) = decision else {
        panic!("expected reconnect fallback");
    };

    assert_eq!(fallback.reason, reason);
    assert!(fallback.uses_backend_reconnect());
    assert!(!fallback.sends_client_error());
    fallback.retry_after
}

#[test]
fn disabled_inputs_skip_handoff_with_silent_reconnect() {
    let now = Instant::now();
    let backend = key("1.11.20");
    let mut state = HandoffFallbackState::new();

    assert_silent_reconnect(
        state.should_attempt(&backend, HandoffAttemptInputs::new(false, true, false), now),
        HandoffFallbackReason::ClientUnsupported,
    );
    assert_silent_reconnect(
        state.should_attempt(&backend, HandoffAttemptInputs::new(true, false, false), now),
        HandoffFallbackReason::ServicePolicyDisabled,
    );
    assert_silent_reconnect(
        state.should_attempt(&backend, HandoffAttemptInputs::new(true, true, true), now),
        HandoffFallbackReason::FdPressureDisabled,
    );
    assert_silent_reconnect(
        state.should_attempt(&backend, HandoffAttemptInputs::adopted_backend(true), now),
        HandoffFallbackReason::AdoptedBackend,
    );
}

#[test]
fn permission_and_integrity_failures_fall_back_without_client_error() {
    let now = Instant::now();
    let backend = key("1.11.20");
    let mut state =
        HandoffFallbackState::with_policy(HandoffFallbackPolicy::new(8, Duration::from_secs(30)));

    assert_eq!(
        state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffAttemptDecision::Attempt
    );
    assert_silent_reconnect(
        state.record_failed_attempt(
            backend.clone(),
            HandoffAttemptFailure::PermissionDenied,
            now,
        ),
        HandoffFallbackReason::PermissionDenied,
    );
    assert_silent_reconnect(
        state.record_failed_attempt(
            backend.clone(),
            HandoffAttemptFailure::IntegrityMismatch,
            now + Duration::from_millis(1),
        ),
        HandoffFallbackReason::IntegrityMismatch,
    );
    assert_silent_reconnect(
        state.record_failed_attempt(
            backend,
            HandoffAttemptFailure::BackendAckTimeout,
            now + Duration::from_millis(2),
        ),
        HandoffFallbackReason::BackendAckTimeout,
    );
}

#[test]
fn failed_handoff_attempts_are_bounded_and_then_rate_limited() {
    let now = Instant::now();
    let backend = key("1.11.20");
    let mut state =
        HandoffFallbackState::with_policy(HandoffFallbackPolicy::new(2, Duration::from_secs(30)));

    assert_eq!(
        state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffAttemptDecision::Attempt
    );
    state.record_failed_attempt(
        backend.clone(),
        HandoffAttemptFailure::PermissionDenied,
        now,
    );
    state.record_failed_attempt(
        backend.clone(),
        HandoffAttemptFailure::BackendAckTimeout,
        now + Duration::from_millis(1),
    );
    state.record_failed_attempt(
        backend.clone(),
        HandoffAttemptFailure::BackendAckTimeout,
        now + Duration::from_millis(2),
    );

    assert_eq!(state.failed_attempt_count(&backend, now), 2);
    let retry_after = assert_silent_reconnect(
        state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffFallbackReason::FailedAttemptRateLimited,
    )
    .expect("rate-limited handoff includes retry-after");
    assert!(retry_after <= Duration::from_secs(30));

    assert_eq!(
        state.should_attempt(
            &backend,
            HandoffAttemptInputs::enabled(),
            now + Duration::from_secs(31)
        ),
        HandoffAttemptDecision::Attempt
    );
    assert_eq!(
        state.failed_attempt_count(&backend, now + Duration::from_secs(31)),
        0
    );
}

#[test]
fn successful_handoff_resets_failed_attempt_budget() {
    let now = Instant::now();
    let backend = key("1.11.20");
    let mut state =
        HandoffFallbackState::with_policy(HandoffFallbackPolicy::new(1, Duration::from_secs(30)));

    state.record_failed_attempt(
        backend.clone(),
        HandoffAttemptFailure::PermissionDenied,
        now,
    );
    assert_silent_reconnect(
        state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffFallbackReason::FailedAttemptRateLimited,
    );

    state.record_success(&backend);

    assert_eq!(
        state.should_attempt(&backend, HandoffAttemptInputs::enabled(), now),
        HandoffAttemptDecision::Attempt
    );
    assert_eq!(state.failed_attempt_count(&backend, now), 0);
}
