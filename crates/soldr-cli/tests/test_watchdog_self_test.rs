//! Self-tests for the `timed_test!` watchdog macro.
//!
//! The fast cases (panic-forwarding, short body) live in the lib
//! `test_util::tests` module and run on every `cargo test`. This file
//! exercises the macro itself end-to-end from the perspective of an
//! integration test under `tests/`, which is the surface most callers
//! reach for.
//!
//! The `deliberate_hang_*` test is gated behind the optional
//! `test-watchdog-self-test` feature *and* `#[ignore]`d. It aborts the
//! test binary on purpose, so it would poison the normal test run if
//! it ever executed without operator intent. Verify the abort path
//! manually with:
//!
//! ```text
//! soldr cargo test -p soldr-cli --features test-watchdog-self-test \
//!     --test test_watchdog_self_test -- --ignored --nocapture deliberate_hang
//! ```
//!
//! Expected output: a `TEST HUNG (>2s): deliberate_hang_from_macro`
//! banner followed by a backtrace, then a non-zero exit.

use std::time::Duration;

use soldr_cli::timed_test;

timed_test!(macro_default_timeout_runs_quick_body, {
    // Default 2-minute deadline; trivial body must succeed.
    let sum: u64 = (1..=10).sum();
    assert_eq!(sum, 55);
});

timed_test!(
    macro_custom_timeout_runs_quick_body,
    Duration::from_secs(5),
    {
        let s = String::from("watchdog");
        assert_eq!(s.len(), 8);
    }
);

// Mirrors the `#[ignore]`d unit test in `src/test_util.rs` but reached
// through the integration-test surface so a maintainer can verify the
// abort path without a feature flag too. Kept gated to the self-test
// feature so the integration crate only depends on `test_util`'s
// public items when they're actually available.
#[cfg(feature = "test-watchdog-self-test")]
timed_test!(deliberate_hang_from_macro, Duration::from_secs(2), {
    std::thread::sleep(Duration::from_secs(300));
});
