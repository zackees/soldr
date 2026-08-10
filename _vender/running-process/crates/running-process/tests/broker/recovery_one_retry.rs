#![cfg(feature = "client")]

use std::time::{Duration, Instant};

use running_process::broker::server::{
    BackendKey, BackendRecoveryDecision, BackendRecoveryPolicy, BackendRecoveryRefusalReason,
    BackendRecoveryState, BrokerInstanceKey,
};

fn key(version: &str) -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, "zccache", version)
}

fn recovery_state() -> BackendRecoveryState {
    BackendRecoveryState::with_policy(BackendRecoveryPolicy::new(
        Duration::from_millis(75),
        Duration::from_secs(30),
    ))
}

#[test]
fn first_crash_permits_retry_after_backoff() {
    let now = Instant::now();
    let mut recovery = recovery_state();

    assert_eq!(
        recovery.record_crash(key("1.11.20"), now),
        BackendRecoveryDecision::Retry {
            retry_after: Duration::from_millis(75),
            attempt: 1,
        }
    );
}

#[test]
fn second_crash_refuses_backend_unavailable_with_retry_after() {
    let now = Instant::now();
    let mut recovery = recovery_state();
    let key = key("1.11.20");

    assert!(matches!(
        recovery.record_crash(key.clone(), now),
        BackendRecoveryDecision::Retry { .. }
    ));

    assert_eq!(
        recovery.record_crash(key.clone(), now + Duration::from_secs(2)),
        BackendRecoveryDecision::Refuse {
            reason: BackendRecoveryRefusalReason::BackendUnavailable,
            retry_after: Duration::from_secs(28),
        }
    );

    assert_eq!(
        recovery.record_crash(key, now + Duration::from_secs(30)),
        BackendRecoveryDecision::Retry {
            retry_after: Duration::from_millis(75),
            attempt: 1,
        }
    );
}

#[test]
fn successful_recovery_resets_crash_state() {
    let now = Instant::now();
    let mut recovery = recovery_state();
    let key = key("1.11.20");

    recovery.record_crash(key.clone(), now);
    recovery.record_crash(key.clone(), now + Duration::from_millis(75));
    recovery.record_success(&key);

    assert_eq!(
        recovery.record_crash(key, now + Duration::from_secs(1)),
        BackendRecoveryDecision::Retry {
            retry_after: Duration::from_millis(75),
            attempt: 1,
        }
    );
}
