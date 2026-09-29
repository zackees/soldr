use super::*;
use std::cell::Cell;

/// Fixture lease controller: records acquire/release calls and the lease
/// "receipt" (a fixture stand-in for `ResidentCapacityLease`) each saw, with
/// no daemon or IPC involved.
#[derive(Default)]
struct RecordingLeaseController {
    acquire_calls: Cell<u32>,
    release_calls: Cell<u32>,
    /// `Some(receipt)` once `release` has run; lets a test prove the exact
    /// value `acquire` handed out is the one that reached `release`, not a
    /// dropped-and-forgotten leak.
    released_receipt: Cell<Option<u32>>,
    /// When `false`, `acquire` returns `None` -- the "daemon unreachable /
    /// reservation refused" path.
    acquire_succeeds: Cell<bool>,
}

impl RecordingLeaseController {
    fn new() -> Self {
        Self {
            acquire_succeeds: Cell::new(true),
            ..Default::default()
        }
    }

    fn refusing() -> Self {
        Self {
            acquire_succeeds: Cell::new(false),
            ..Default::default()
        }
    }
}

impl ResidentLeaseController for RecordingLeaseController {
    type Lease = u32;

    fn acquire(&self) -> Option<Self::Lease> {
        self.acquire_calls.set(self.acquire_calls.get() + 1);
        self.acquire_succeeds.get().then_some(42)
    }

    fn release(&self, lease: Option<Self::Lease>) {
        self.release_calls.set(self.release_calls.get() + 1);
        self.released_receipt.set(lease);
    }
}

/// The scoping gate's positive half: the EXECUTION stage is literally named
/// `"nextest"` in the frozen plan (`stage_named(plan, "nextest")` in
/// `execute.rs`), so that exact name must acquire, and the exact receipt
/// `acquire` produced is what the caller gets to release.
#[test]
fn nextest_execution_acquires_the_lease() {
    let controller = RecordingLeaseController::new();

    let lease = acquire_for_stage(&controller, "nextest");

    assert_eq!(lease, Some(Some(42)));
    assert_eq!(controller.acquire_calls.get(), 1);
    assert_eq!(
        controller.release_calls.get(),
        0,
        "release belongs to the stage's exit, not to acquisition"
    );
}

/// A refused/unavailable acquisition (daemon down, reservation rejected)
/// must not fail `ci-test`: the lease is best-effort, so the stage is still
/// leased-scope (`Some`) with nothing acquired (`None`), and the caller still
/// releases it on exit.
#[test]
fn a_refused_acquisition_is_still_a_leased_stage_with_nothing_held() {
    let controller = RecordingLeaseController::refusing();

    let lease = acquire_for_stage(&controller, "nextest");

    assert_eq!(lease, Some(None));
    assert_eq!(controller.acquire_calls.get(), 1);
}

/// The scoping gate's negative half, mirroring
/// `cargo_restoring_runner_is_not_injected_into_nextest_compilation` in
/// `execute_tests.rs`: `nextest-compile` is a pure compile stage the
/// daemon's ordinary shared/exclusive compiler admission already accounts
/// for, so it must never acquire this lease -- even though it now runs in
/// the same peer chain as Nextest execution (soldr#3446).
#[test]
fn nextest_compile_never_acquires_the_lease() {
    let controller = RecordingLeaseController::new();

    let lease = acquire_for_stage(&controller, "nextest-compile");

    assert_eq!(lease, None);
    assert_eq!(
        controller.acquire_calls.get(),
        0,
        "nextest-compile must never reach the resident-capacity lease"
    );
}

/// Pins the reserved weight: `acquire_resident` on the daemon rejects any
/// reservation `>= max`, and CI's compiler-admission capacity is `3` on the
/// standard `ubuntu-24.04` runner (`available_parallelism() - 1` with
/// `CARGO_BUILD_JOBS`/`SOLDR_JOBS` unset, per CLAUDE.md). `1` leaves 2 of 3
/// slots free throughout Nextest EXECUTION -- real headroom for the nested
/// fixture compiles that regressed in warm run 1. A silent change to this
/// constant changes that trade-off, so pin it.
#[test]
fn the_reserved_weight_leaves_real_compiler_headroom() {
    assert_eq!(NEXTEST_RESIDENT_LEASE_PERMITS, 1);
}
