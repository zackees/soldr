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

#[test]
fn lease_is_acquired_before_and_released_after_a_successful_guarded_run() {
    let controller = RecordingLeaseController::new();

    let result = run_nextest_execution(&controller, "nextest", || Ok(0));

    assert_eq!(result.unwrap(), 0);
    assert_eq!(controller.acquire_calls.get(), 1);
    assert_eq!(controller.release_calls.get(), 1);
    assert_eq!(
        controller.released_receipt.get(),
        Some(42),
        "the exact lease `acquire` produced must be the one `release` gets, not a leaked or swapped value"
    );
}

/// The failure/cancel path: `guarded` reports a nonzero stage exit code
/// (the shape `supervise_parallel_stage_and_dylint` returns when Nextest or
/// its Dylint sibling fails and the other is canceled). The lease must
/// still be released.
#[test]
fn lease_is_released_when_the_guarded_stage_reports_a_failure_code() {
    let controller = RecordingLeaseController::new();

    let result = run_nextest_execution(&controller, "nextest", || Ok(73));

    assert_eq!(result.unwrap(), 73);
    assert_eq!(controller.acquire_calls.get(), 1);
    assert_eq!(controller.release_calls.get(), 1);
    assert_eq!(controller.released_receipt.get(), Some(42));
}

/// The other failure/cancel shape: `guarded` returns `Err` (the spawn-error
/// path `supervise_parallel_stage_and_dylint` takes when the second branch
/// cannot even start). The lease must still be released -- nothing between
/// `acquire` and `release` may early-return around it.
#[test]
fn lease_is_released_when_the_guarded_stage_errors() {
    let controller = RecordingLeaseController::new();

    let result = run_nextest_execution(&controller, "nextest", || {
        Err(SoldrError::Other("fixture spawn failure".into()))
    });

    assert!(result.is_err());
    assert_eq!(controller.acquire_calls.get(), 1);
    assert_eq!(controller.release_calls.get(), 1);
    assert_eq!(controller.released_receipt.get(), Some(42));
}

/// A refused/unavailable acquisition (daemon down, reservation rejected)
/// must not fail `ci-test`: the lease is best-effort, so `guarded` still
/// runs and `release` still runs, both with `None`.
#[test]
fn a_refused_acquisition_still_runs_the_guarded_stage_and_still_releases() {
    let controller = RecordingLeaseController::refusing();

    let result = run_nextest_execution(&controller, "nextest", || Ok(0));

    assert_eq!(result.unwrap(), 0);
    assert_eq!(controller.acquire_calls.get(), 1);
    assert_eq!(controller.release_calls.get(), 1);
    assert_eq!(
        controller.released_receipt.get(),
        None,
        "nothing was acquired, so release must see None, not a fabricated receipt"
    );
}

/// The scoping gate's positive half: the EXECUTION stage is literally named
/// `"nextest"` in the frozen plan (`stage_named(plan, "nextest")` in
/// `execute.rs`), so that exact name must acquire.
#[test]
fn nextest_execution_acquires_the_lease() {
    let controller = RecordingLeaseController::new();

    let result = run_nextest_execution(&controller, "nextest", || Ok(0));

    assert_eq!(result.unwrap(), 0);
    assert_eq!(controller.acquire_calls.get(), 1);
    assert_eq!(controller.release_calls.get(), 1);
}

/// The scoping gate's negative half, mirroring
/// `cargo_restoring_runner_is_not_injected_into_nextest_compilation` in
/// `execute_tests.rs`: `nextest-compile` is a pure compile stage the
/// daemon's ordinary shared/exclusive compiler admission already accounts
/// for, so it must never acquire this lease. `guarded` still runs -- only
/// the lease is skipped.
#[test]
fn nextest_compile_never_acquires_the_lease() {
    let controller = RecordingLeaseController::new();

    let result = run_nextest_execution(&controller, "nextest-compile", || Ok(0));

    assert_eq!(result.unwrap(), 0);
    assert_eq!(
        controller.acquire_calls.get(),
        0,
        "nextest-compile must never reach the resident-capacity lease"
    );
    assert_eq!(
        controller.release_calls.get(),
        0,
        "nothing was acquired for nextest-compile, so nothing should be released either"
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
