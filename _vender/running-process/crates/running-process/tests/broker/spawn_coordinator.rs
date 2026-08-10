#![cfg(feature = "client")]

use std::time::{Duration, Instant};

use running_process::broker::server::{
    acquire_spawn_lock, BackendKey, BrokerInstanceKey, SpawnBeginError, SpawnBudgetConfig,
    SpawnCoordinator, SpawnLockError, SpawnOutcome, DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW,
    DEFAULT_SPAWN_BUDGET_WINDOW,
};

fn key(version: &str) -> BackendKey {
    BackendKey::new(BrokerInstanceKey::Shared, "zccache", version)
}

#[test]
fn spawn_budget_exhaustion_is_per_backend_key() {
    let now = Instant::now();
    let mut coordinator =
        SpawnCoordinator::with_config(SpawnBudgetConfig::new(2, Duration::from_secs(10)));
    let first_key = key("1.11.20");
    let second_key = key("1.11.21");

    coordinator.try_begin(first_key.clone(), now).unwrap();
    coordinator.finish(&first_key, SpawnOutcome::Failed, now);
    coordinator.try_begin(first_key.clone(), now).unwrap();
    coordinator.finish(&first_key, SpawnOutcome::Failed, now);

    let err = coordinator.try_begin(first_key, now).unwrap_err();
    assert!(matches!(
        err,
        SpawnBeginError::BudgetExhausted {
            retry_after,
            remaining: 0
        } if retry_after == Duration::from_secs(10)
    ));

    let permit = coordinator.try_begin(second_key, now).unwrap();
    assert_eq!(permit.attempt_number, 1);
}

#[test]
fn cooldown_resets_spawn_budget() {
    let now = Instant::now();
    let mut coordinator =
        SpawnCoordinator::with_config(SpawnBudgetConfig::new(1, Duration::from_secs(10)));
    let key = key("1.11.20");

    coordinator.try_begin(key.clone(), now).unwrap();
    coordinator.finish(&key, SpawnOutcome::Failed, now);
    assert!(matches!(
        coordinator.try_begin(key.clone(), now),
        Err(SpawnBeginError::BudgetExhausted { .. })
    ));

    let permit = coordinator
        .try_begin(key, now + Duration::from_secs(10))
        .unwrap();
    assert_eq!(permit.attempt_number, 1);
}

#[test]
fn single_flight_blocks_duplicate_spawn_for_same_key() {
    let now = Instant::now();
    let mut coordinator = SpawnCoordinator::new();
    let key = key("1.11.20");

    coordinator.try_begin(key.clone(), now).unwrap();

    assert_eq!(
        coordinator.try_begin(key, now),
        Err(SpawnBeginError::AlreadyInProgress)
    );
}

#[test]
fn finishing_success_resets_failure_budget() {
    let now = Instant::now();
    let mut coordinator =
        SpawnCoordinator::with_config(SpawnBudgetConfig::new(2, Duration::from_secs(10)));
    let key = key("1.11.20");

    coordinator.try_begin(key.clone(), now).unwrap();
    coordinator.finish(&key, SpawnOutcome::Failed, now);
    coordinator.try_begin(key.clone(), now).unwrap();
    coordinator.finish(&key, SpawnOutcome::Success, now);

    let snapshot = coordinator.snapshot(key, now);
    assert_eq!(snapshot.attempts_used, 0);
    assert_eq!(snapshot.remaining, 2);
    assert!(!snapshot.in_flight);
    assert_eq!(snapshot.retry_after, None);
}

#[test]
fn snapshot_reports_retry_after_when_budget_is_empty() {
    let now = Instant::now();
    let mut coordinator =
        SpawnCoordinator::with_config(SpawnBudgetConfig::new(1, Duration::from_secs(10)));
    let key = key("1.11.20");

    coordinator.try_begin(key.clone(), now).unwrap();
    coordinator.finish(&key, SpawnOutcome::Failed, now);
    let snapshot = coordinator.snapshot(key, now + Duration::from_secs(3));

    assert_eq!(snapshot.attempts_used, 1);
    assert_eq!(snapshot.remaining, 0);
    assert_eq!(snapshot.retry_after, Some(Duration::from_secs(7)));
}

#[test]
fn default_budget_is_three_attempts_per_thirty_seconds() {
    let now = Instant::now();
    let mut coordinator = SpawnCoordinator::new();
    let key = key("1.11.20");

    assert_eq!(DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW, 3);
    assert_eq!(DEFAULT_SPAWN_BUDGET_WINDOW, Duration::from_secs(30));

    for attempt_number in 1..=3 {
        let permit = coordinator.try_begin(key.clone(), now).unwrap();
        assert_eq!(permit.attempt_number, attempt_number);
        coordinator.finish(&key, SpawnOutcome::Failed, now);
    }

    assert!(matches!(
        coordinator.try_begin(key.clone(), now + Duration::from_secs(29)),
        Err(SpawnBeginError::BudgetExhausted {
            retry_after,
            remaining: 0
        }) if retry_after == Duration::from_secs(1)
    ));

    let permit = coordinator
        .try_begin(key, now + DEFAULT_SPAWN_BUDGET_WINDOW)
        .unwrap();
    assert_eq!(permit.attempt_number, 1);
}

#[test]
fn acquire_spawn_lock_reports_lock_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("zccache-1.11.20.spawn.lock");
    let _guard = acquire_spawn_lock(&lock_path).unwrap();

    let err = acquire_spawn_lock(&lock_path).unwrap_err();

    assert!(matches!(err, SpawnLockError::AlreadyLocked { path } if path == lock_path));
}

#[test]
fn spawn_lock_guard_releases_lock_on_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("zccache-1.11.20.spawn.lock");
    let guard = acquire_spawn_lock(&lock_path).unwrap();
    assert_eq!(guard.path(), lock_path.as_path());
    assert!(lock_path.exists());

    drop(guard);

    let _next_guard = acquire_spawn_lock(&lock_path).unwrap();
}
