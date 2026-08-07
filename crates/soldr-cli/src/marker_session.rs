//! Session-awareness for the daemon-unavailable cooldown marker (soldr#2317).
//!
//! The `compile-daemon-unavailable` marker is a cross-process circuit breaker:
//! when one rustc wrapper proves the daemon unreachable, its siblings in the
//! same `soldr cargo` build skip the (up to 30 s) spawn-retry budget for a
//! cooldown window and fall straight to direct rustc. That is correct *within*
//! one invocation.
//!
//! The bug it caused: the marker persists on disk across invocations, so a
//! *fresh, human-invoked* `soldr cargo …` run started within the cooldown
//! inherited a marker written by the previous run and skipped its own first
//! real spawn attempt — an operator retrying after a transient failure got a
//! silent 0 ms skip and no chance to recover the daemon.
//!
//! The fix here lets a wrapper tell "a sibling of *my* session wrote this
//! marker" from "a previous invocation left it": the marker is honored only
//! when it was written at/after the current session began. The session start
//! is recovered from the `SOLDR_BUILD_SESSION_ID` the front door already
//! stamps onto every wrapper — its high 32 bits are the truncated unix-ms of
//! the session start (see `cargo_front_door::generate_build_session_id`) — so
//! no new environment variable or front-door plumbing is required.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix-ms at which the enclosing front-door session began, recovered from
/// `SOLDR_BUILD_SESSION_ID`. `None` when this wrapper runs outside a
/// soldr cargo session (a bare rustc-shim invocation), in which case callers
/// fall back to the marker's TTL as the only freshness signal — the
/// pre-soldr#2317 behavior.
///
/// `generate_build_session_id` packs `(unix_ms & 0xFFFF_FFFF)` into the high 32
/// bits. Those bits wrap every ~49.7 days, so reconstruct the full timestamp
/// from the current time's high bits, subtracting one wrap when the estimate
/// lands in the future. A build session is always far younger than one wrap, so
/// the result is exact in practice.
pub(crate) fn session_started_ms() -> Option<i64> {
    let id: u64 = std::env::var(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(decode_session_started_ms(id, now_unix_ms()))
}

/// Pure inverse of the front door's id packing (unit-tested without env):
/// recover the full session-start unix-ms from an id whose high 32 bits hold
/// `(started_ms & 0xFFFF_FFFF)`, using `now_ms`'s high bits and correcting for
/// the ~49.7-day wrap when the estimate lands in the future.
fn decode_session_started_ms(id: u64, now_ms: i64) -> i64 {
    let low = (id >> 32) & 0xFFFF_FFFF;
    let now = now_ms.max(0) as u64;
    let mut est = (now & !0xFFFF_FFFF_u64) | low;
    if est > now {
        est = est.wrapping_sub(1 << 32);
    }
    est as i64
}

/// Filesystem mtime of the marker as unix-ms, or `None` if it can't be read.
pub(crate) fn marker_modified_unix_ms(marker_path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(marker_path).ok()?.modified().ok()?;
    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            // A pre-epoch mtime is nonsensical here; treat it as "very old".
            .unwrap_or(0),
    )
}

/// Pure decision core (unit-tested): a marker written at `marker_ms` predates a
/// session that began at `session_started_ms`. No session (`None`) ⇒ never
/// predates, preserving TTL-only freshness for shims outside a cargo session.
pub(crate) fn marker_ms_predates_session(marker_ms: i64, session_started_ms: Option<i64>) -> bool {
    match session_started_ms {
        Some(started) => marker_ms < started,
        None => false,
    }
}

/// The marker was written by a *previous* front-door session, not a sibling
/// rustc wrapper of the current one — so it must not suppress this session's
/// first real spawn attempt (soldr#2317).
pub(crate) fn marker_predates_current_session(marker_path: &Path) -> bool {
    let session = session_started_ms();
    let Some(marker_ms) = marker_modified_unix_ms(marker_path) else {
        return false;
    };
    marker_ms_predates_session(marker_ms, session)
}

/// Build the human-readable `spawn_err` for a suppressed retry. Beyond the
/// original `prior_failure`, it names the marker path and how long the cooldown
/// still suppresses retries (soldr#2317, observability half) so the skip is not
/// a dead end an operator cannot see or clear.
pub(crate) fn skipped_retry_message(marker_path: &Path, reason: Option<String>) -> String {
    let expiry = marker_modified_unix_ms(marker_path)
        .map(|marker_ms| {
            let age = now_unix_ms().saturating_sub(marker_ms).max(0);
            let ttl = crate::compile_dispatch::DAEMON_UNAVAILABLE_MARKER_TTL.as_millis() as i64;
            let remaining = (ttl - age).max(0) / 1000;
            format!(
                "; expires_in={remaining}s; marker={}",
                marker_path.display()
            )
        })
        .unwrap_or_default();
    match reason {
        Some(r) => format!(
            "recent daemon-unavailable marker present; skipped spawn retry{expiry}; \
             prior_failure={r}"
        ),
        None => format!("recent daemon-unavailable marker present; skipped spawn retry{expiry}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#2317 decision core: a marker strictly older than session start is
    // from a prior invocation; one at/after belongs to this session; with no
    // session context nothing predates (TTL-only legacy behavior).
    #[test]
    fn marker_predates_session_only_when_strictly_older() {
        assert!(marker_ms_predates_session(1_000, Some(2_000)));
        assert!(!marker_ms_predates_session(2_000, Some(2_000)));
        assert!(!marker_ms_predates_session(3_000, Some(2_000)));
        assert!(!marker_ms_predates_session(1_000, None));
    }

    #[test]
    fn marker_modified_unix_ms_reads_existing_and_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("compile-daemon-unavailable");
        assert!(marker_modified_unix_ms(&marker).is_none(), "absent -> None");
        std::fs::write(&marker, "daemon unavailable\n").expect("write marker");
        let ms = marker_modified_unix_ms(&marker).expect("present -> Some");
        assert!(ms > 1_000_000_000_000, "implausibly small mtime: {ms}");
    }

    // The decode must invert the front door's id packing: an id whose high 32
    // bits are `(started_ms & 0xFFFF_FFFF)` recovers `started_ms` exactly for a
    // recent session, and correctly unwraps across a 32-bit ms boundary.
    #[test]
    fn decode_session_started_ms_inverts_id_high_bits() {
        let started: i64 = 1_780_000_000_123;
        let id: u64 = (((started as u64) & 0xFFFF_FFFF) << 32) | 0xDEAD_BEEF;
        // "now" a few seconds after start, same 32-bit window.
        assert_eq!(decode_session_started_ms(id, started + 5_000), started);
        // "now" just after start crossed a 2^32-ms boundary: the low bits look
        // larger than now's low bits, so the decode must subtract one wrap.
        let near_wrap: i64 = ((5_u64 << 32) | 10) as i64; // low bits = 10
        let start_before = near_wrap - 20; // low bits wrap below 10 -> in prev window
        let id2: u64 = (((start_before as u64) & 0xFFFF_FFFF) << 32) | 1;
        assert_eq!(decode_session_started_ms(id2, near_wrap), start_before);
    }

    #[test]
    fn skipped_retry_message_carries_marker_and_expiry_and_reason() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("compile-daemon-unavailable");
        std::fs::write(&marker, "daemon unavailable\n").expect("write marker");
        let msg = skipped_retry_message(&marker, Some("boom".to_string()));
        assert!(msg.contains("skipped spawn retry"), "{msg}");
        assert!(msg.contains("expires_in="), "must surface TTL: {msg}");
        assert!(msg.contains("marker="), "must name the marker path: {msg}");
        assert!(
            msg.contains("prior_failure=boom"),
            "must keep the reason: {msg}"
        );
    }
}
