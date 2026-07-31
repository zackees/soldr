//! Shared retry-with-exponential-backoff for network fetches (soldr#2132).
//!
//! This crate already retried transient network errors in three places
//! (`fetch::mod`, `fetch::llvm`, `fetch::zig`), each with its own copy of the
//! same loop and its own `ATTEMPTS` / `INITIAL_BACKOFF` pair set to the same
//! values. Seven other fetchers had none. That gap failed two lanes of the
//! v0.8.30 release build:
//!
//! ```text
//! soldr build: managed cmake unavailable for host aarch64-unknown-linux-gnu:
//!   network error: error decoding response body
//! → error[E0463]: can't find crate for `core` / `std`
//! ```
//!
//! One truncated response body, and the build failed with an error naming the
//! wrong thing entirely.
//!
//! # What is and is not retried
//!
//! [`is_transient`] deliberately matches the predicate the existing loops
//! already used: [`SoldrError::Network`] and [`SoldrError::ToolNotFound`]
//! (a release asset that is not published *yet*). Everything else is fatal on
//! the first try — in particular a sha256 mismatch is
//! [`SoldrError::Other`]/[`SoldrError::Archive`] and must stay fatal. Retrying
//! an integrity failure would turn a hard stop into three more chances to
//! accept a bad artifact, which is worse than the flake this module exists to
//! absorb.

use std::future::Future;
use std::time::Duration;

use crate::core::SoldrError;

/// Total attempts, including the first. Matches the value the three
/// pre-existing loops independently converged on.
pub(crate) const FETCH_ATTEMPTS: u32 = 4;

/// Delay before the second attempt; doubles thereafter (5s, 10s, 20s).
pub(crate) const FETCH_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

// A single attempt would make the backoff dead code and silently turn this
// module into a no-op wrapper.
const _: () = assert!(FETCH_ATTEMPTS >= 2);

/// Whether `err` is worth another attempt.
///
/// `Network` covers connect/read/decode failures and non-success HTTP. Any
/// integrity or layout error is excluded on purpose — see the module docs.
pub(crate) fn is_transient(err: &SoldrError) -> bool {
    matches!(err, SoldrError::ToolNotFound(_) | SoldrError::Network(_))
}

/// Run `operation`, retrying transient failures with exponential backoff.
///
/// `what` names the thing being fetched and appears in the retry log, so an
/// operator reading CI output can tell *which* fetch is struggling rather than
/// just that something is.
///
/// The last error is returned unchanged once attempts are exhausted, so the
/// caller's own context (and the original message) survive.
pub(crate) async fn with_backoff<T, F, Fut>(what: &str, operation: F) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    with_backoff_params(what, FETCH_ATTEMPTS, FETCH_INITIAL_BACKOFF, operation).await
}

/// [`with_backoff`] with the schedule spelled out. Separate so tests can drive
/// it with a zero backoff instead of sleeping for real seconds.
pub(crate) async fn with_backoff_params<T, F, Fut>(
    what: &str,
    attempts: u32,
    initial_backoff: Duration,
    mut operation: F,
) -> Result<T, SoldrError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SoldrError>>,
{
    let mut backoff = initial_backoff;
    let mut attempt: u32 = 1;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < attempts && is_transient(&err) => {
                eprintln!(
                    "soldr: transient error fetching {what} (attempt {attempt}/{attempts}): \
                     {err}; retrying in {backoff:?}"
                );
                if !backoff.is_zero() {
                    tokio::time::sleep(backoff).await;
                }
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const NO_SLEEP: Duration = Duration::ZERO;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
            .block_on(future)
    }

    crate::timed_test!(transient_failures_are_retried_until_one_succeeds, {
        let calls = Cell::new(0u32);
        let result: Result<&str, SoldrError> =
            block_on(with_backoff_params("test-asset", 4, NO_SLEEP, || {
                calls.set(calls.get() + 1);
                async {
                    if calls.get() < 3 {
                        Err(SoldrError::Network("error decoding response body".into()))
                    } else {
                        Ok("bundle")
                    }
                }
            }));
        assert_eq!(
            result.expect("should succeed on the third attempt"),
            "bundle"
        );
        assert_eq!(calls.get(), 3, "should stop as soon as an attempt succeeds");
    });

    crate::timed_test!(a_checksum_mismatch_is_never_retried, {
        // The case that must stay fatal: retrying an integrity failure would
        // turn one hard stop into several chances to accept a bad artifact.
        let calls = Cell::new(0u32);
        let result: Result<(), SoldrError> =
            block_on(with_backoff_params("test-asset", 4, NO_SLEEP, || {
                calls.set(calls.get() + 1);
                async {
                    Err(SoldrError::Other(
                        "sha256 mismatch: expected a, got b".into(),
                    ))
                }
            }));
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "a non-transient error must not be retried");
    });

    crate::timed_test!(attempts_are_bounded_and_the_last_error_survives, {
        let calls = Cell::new(0u32);
        let result: Result<(), SoldrError> =
            block_on(with_backoff_params("test-asset", 4, NO_SLEEP, || {
                calls.set(calls.get() + 1);
                // Capture the count by value: `async move` would otherwise move
                // the `Cell` itself into the coroutine, and the assertion below
                // still needs it.
                let attempt = calls.get();
                async move { Err(SoldrError::Network(format!("attempt {attempt}"))) }
            }));
        assert_eq!(calls.get(), 4, "should stop at the attempt budget");
        // The caller sees the *last* failure, not a synthesized wrapper, so the
        // original cause is still in the log.
        match result {
            Err(SoldrError::Network(message)) => assert_eq!(message, "attempt 4"),
            other => panic!("expected the final Network error, got {other:?}"),
        }
    });

    crate::timed_test!(transient_predicate_covers_network_and_not_found_only, {
        assert!(is_transient(&SoldrError::Network("timeout".into())));
        assert!(is_transient(&SoldrError::ToolNotFound(
            "not published yet".into()
        )));
        assert!(!is_transient(&SoldrError::Other("sha256 mismatch".into())));
        assert!(!is_transient(&SoldrError::Archive("tar.zst unpack".into())));
        assert!(!is_transient(&SoldrError::UnsupportedPlatform(
            "wasm".into()
        )));
    });
}
