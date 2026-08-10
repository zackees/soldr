#![cfg(feature = "client")]

use std::time::{Duration, Instant};

use running_process::broker::server::{
    BackendIdleCoordinator, BackendIdleDue, BackendIdlePolicy, BackendKey, BrokerInstanceKey,
    QuiesceReason, DEFAULT_BACKEND_IDLE_TIMEOUT,
};

fn key(service_name: &str, version: &str) -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, service_name, version)
}

fn due_key(due: &[BackendIdleDue]) -> Option<&BackendKey> {
    due.first().map(|item| &item.key)
}

#[test]
fn default_idle_timeout_is_thirty_seconds() {
    let policy = BackendIdlePolicy::default();

    assert_eq!(DEFAULT_BACKEND_IDLE_TIMEOUT, Duration::from_secs(30));
    assert_eq!(policy.default_idle_timeout(), Duration::from_secs(30));
    assert_eq!(
        BackendIdlePolicy::new(Duration::ZERO).default_idle_timeout(),
        Duration::from_millis(1)
    );
}

#[test]
fn active_backend_is_not_due_before_timeout() {
    let now = Instant::now();
    let mut coordinator =
        BackendIdleCoordinator::with_policy(BackendIdlePolicy::new(Duration::from_secs(10)));

    coordinator.mark_activity(key("zccache", "1.11.20"), now);

    assert!(coordinator
        .collect_due_for_quiesce(now + Duration::from_secs(9))
        .is_empty());
}

#[test]
fn idle_backend_is_due_at_timeout() {
    let now = Instant::now();
    let mut coordinator =
        BackendIdleCoordinator::with_policy(BackendIdlePolicy::new(Duration::from_secs(10)));
    let key = key("zccache", "1.11.20");

    coordinator.mark_activity(key.clone(), now);
    let due = coordinator.collect_due_for_quiesce(now + Duration::from_secs(10));

    assert_eq!(
        due,
        vec![BackendIdleDue {
            key,
            idle_for: Duration::from_secs(10),
            configured_timeout: Duration::from_secs(10),
            reason: QuiesceReason::IdleTimeout,
        }]
    );
}

#[test]
fn mark_activity_resets_idle_deadline() {
    let now = Instant::now();
    let mut coordinator =
        BackendIdleCoordinator::with_policy(BackendIdlePolicy::new(Duration::from_secs(10)));
    let key = key("zccache", "1.11.20");

    coordinator.mark_activity(key.clone(), now);
    coordinator.mark_activity(key.clone(), now + Duration::from_secs(9));

    assert!(coordinator
        .collect_due_for_quiesce(now + Duration::from_secs(18))
        .is_empty());

    let due = coordinator.collect_due_for_quiesce(now + Duration::from_secs(19));

    assert_eq!(due.len(), 1);
    assert_eq!(due[0].key, key);
    assert_eq!(due[0].idle_for, Duration::from_secs(10));
    assert_eq!(due[0].reason, QuiesceReason::IdleTimeout);
}

#[test]
fn draining_and_quiesced_backends_are_not_repeatedly_emitted() {
    let now = Instant::now();
    let mut coordinator =
        BackendIdleCoordinator::with_policy(BackendIdlePolicy::new(Duration::from_secs(10)));
    let draining = key("zccache", "1.11.20");
    let quiesced = key("soldr", "0.7.0");

    coordinator.mark_activity(draining.clone(), now);
    let due = coordinator.collect_due_for_quiesce(now + Duration::from_secs(10));
    assert_eq!(due_key(&due), Some(&draining));

    assert!(coordinator
        .collect_due_for_quiesce(now + Duration::from_secs(11))
        .is_empty());

    coordinator.mark_activity(quiesced.clone(), now);
    assert!(coordinator.mark_quiesced(&quiesced));
    assert!(coordinator
        .collect_due_for_quiesce(now + Duration::from_secs(10))
        .is_empty());

    assert!(coordinator.mark_draining(&quiesced));
    assert!(coordinator
        .collect_due_for_quiesce(now + Duration::from_secs(20))
        .is_empty());
}

#[test]
fn multiple_backend_keys_have_independent_deadlines() {
    let now = Instant::now();
    let mut coordinator =
        BackendIdleCoordinator::with_policy(BackendIdlePolicy::new(Duration::from_secs(10)));
    let old = key("zccache", "1.11.20");
    let fresh = key("soldr", "0.7.0");

    coordinator.mark_activity(old.clone(), now);
    coordinator.mark_activity(fresh.clone(), now + Duration::from_secs(5));

    let first_due = coordinator.collect_due_for_quiesce(now + Duration::from_secs(10));
    assert_eq!(due_key(&first_due), Some(&old));

    let second_due = coordinator.collect_due_for_quiesce(now + Duration::from_secs(15));
    assert_eq!(due_key(&second_due), Some(&fresh));
}

#[test]
fn remove_backend_drops_idle_tracking() {
    let now = Instant::now();
    let mut coordinator = BackendIdleCoordinator::new();
    let key = key("zccache", "1.11.20");

    coordinator.mark_activity(key.clone(), now);

    assert!(coordinator.remove_backend(&key));
    assert!(!coordinator.remove_backend(&key));
    assert!(coordinator
        .collect_due_for_quiesce(now + DEFAULT_BACKEND_IDLE_TIMEOUT)
        .is_empty());
}
