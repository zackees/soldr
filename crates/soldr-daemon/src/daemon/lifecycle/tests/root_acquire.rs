//! Tests for [`crate::daemon::lifecycle::RootOwnershipGuard::acquire_with_grace`]:
//! the stop→relaunch grace window must wait out a lock held by an exiting
//! owner, but back off immediately when a live daemon serves the root.

use crate::core::SoldrPaths;
use crate::daemon::generation_key::set_generation_override;
use crate::daemon::lifecycle::{RootAcquireOutcome, RootOwnershipGuard};
use std::time::Duration;

const POLL: Duration = Duration::from_millis(20);

#[test]
fn different_broker_generations_can_hold_the_same_product_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let global = RootOwnershipGuard::try_acquire(&paths)
        .unwrap()
        .expect("legacy generation owns the old global lock");
    set_generation_override(Some("soldr-daemon-generation-a".into()));
    let a = RootOwnershipGuard::try_acquire(&paths)
        .unwrap()
        .expect("generation A coexists with the legacy daemon");
    assert!(RootOwnershipGuard::try_acquire(&paths).unwrap().is_none());
    set_generation_override(Some("soldr-daemon-generation-b".into()));
    let b = RootOwnershipGuard::try_acquire(&paths)
        .unwrap()
        .expect("generation B coexists with generation A");
    set_generation_override(None);
    drop((global, a, b));
}

#[test]
fn free_lock_is_acquired_without_waiting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let outcome =
        RootOwnershipGuard::acquire_with_grace(&paths, Duration::from_secs(5), POLL, || None)
            .expect("io ok");
    assert!(matches!(outcome, RootAcquireOutcome::Acquired(_)));
}

#[test]
fn busy_lock_with_a_serving_daemon_backs_off_immediately() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let _held = RootOwnershipGuard::try_acquire(&paths)
        .expect("io ok")
        .expect("first acquisition wins");
    let start = std::time::Instant::now();
    let outcome =
        RootOwnershipGuard::acquire_with_grace(&paths, Duration::from_secs(30), POLL, || {
            Some(4242)
        })
        .expect("io ok");
    assert!(matches!(outcome, RootAcquireOutcome::AlreadyServing(4242)));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a redundant spawn against a served root must not sit out the grace budget \
         (issue #1814); waited {:?}",
        start.elapsed()
    );
}

#[test]
fn busy_lock_released_within_budget_is_acquired() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let held = RootOwnershipGuard::try_acquire(&paths)
        .expect("io ok")
        .expect("first acquisition wins");
    // Simulate the exiting previous owner: release the lock shortly
    // after the challenger starts waiting.
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
    });
    let outcome =
        RootOwnershipGuard::acquire_with_grace(&paths, Duration::from_secs(30), POLL, || None)
            .expect("io ok");
    handle.join().expect("releaser thread");
    assert!(
        matches!(outcome, RootAcquireOutcome::Acquired(_)),
        "the grace window must absorb a lock released mid-wait"
    );
}

#[test]
fn busy_lock_never_released_times_out_after_the_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let _held = RootOwnershipGuard::try_acquire(&paths)
        .expect("io ok")
        .expect("first acquisition wins");
    let outcome =
        RootOwnershipGuard::acquire_with_grace(&paths, Duration::from_millis(200), POLL, || None)
            .expect("io ok");
    assert!(matches!(outcome, RootAcquireOutcome::TimedOut));
}
