//! Read-only rollup of compile-daemon fallback events for `soldr doctor`
//! (soldr#1838 Phase 4).
//!
//! **The mechanism this summarises no longer exists.** Before the mandatory
//! broker cutover, a cacheable compile that could not reach the daemon
//! degraded to direct rustc and appended a JSONL record to
//! `logs/compile-daemon-fallbacks.jsonl`. Nothing surfaced that journal, so a
//! build silently running uncached -- the exact "quietly 10-50x slower,
//! indefinitely" failure soldr#1838 calls out -- was invisible unless you
//! already knew to open the file. This summarised it into `soldr doctor`.
//!
//! Post-cutover, cacheable compiler work never silently bypasses the
//! broker/daemon: infrastructure errors hard-fail instead, and
//! `compile_dispatch` no longer has a writer at all (its absence is enforced
//! by `daemon_console_policy_guard`). So every record this can find is
//! historical, written by an older soldr and kept only so upgrading does not
//! erase diagnostic history. The reader stays because those records are still
//! worth reading; what changed is that a non-zero count is a fact about the
//! past, not a condition to recover from (soldr#2424).
//!
//! Strictly read-only: it never writes the journal, and every parse failure
//! degrades to "skip that line", so a truncated or corrupt journal can never
//! make `doctor` itself fail.

use crate::core::SoldrPaths;
use serde::Serialize;

/// One fallback occurrence. Only the fields a reader acts on are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct FallbackEntry {
    /// Unix milliseconds when the fallback was recorded; `0` when the
    /// journal line omitted or malformed the timestamp.
    pub ts_ms: u64,
    /// The dispatch error that forced the bypass.
    pub reason: String,
}

/// Summary surfaced in `soldr doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct FallbackRollup {
    /// Total fallback records in the journal.
    pub total: usize,
    /// The most recent few, newest first.
    pub recent: Vec<FallbackEntry>,
}

/// Parse the fallback JSONL into a rollup. Pure, so it is fixture-tested
/// on every platform.
///
/// Only `event == "compile_daemon_fallback"` records count. Blank lines,
/// malformed JSON, and foreign events are skipped rather than failing --
/// the journal is append-only best-effort and a single bad line must not
/// blind the whole rollup.
pub(crate) fn summarize(jsonl: &str, max_recent: usize) -> FallbackRollup {
    let mut entries: Vec<FallbackEntry> = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("event").and_then(|v| v.as_str()) != Some("compile_daemon_fallback") {
            continue;
        }
        let ts_ms = value.get("ts_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let reason = value
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("(reason unrecorded)")
            .to_string();
        entries.push(FallbackEntry { ts_ms, reason });
    }
    let total = entries.len();
    // Newest first. A stable sort keeps journal order among equal
    // timestamps, so the "recent" window is deterministic.
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.ts_ms));
    entries.truncate(max_recent);
    FallbackRollup {
        total,
        recent: entries,
    }
}

/// Read + summarise the journal for `paths`. Best-effort: a missing or
/// unreadable file yields an empty rollup, which reads as "healthy --
/// the cache was never bypassed".
pub(crate) fn collect(paths: &SoldrPaths, max_recent: usize) -> FallbackRollup {
    let path = crate::compile_dispatch::compile_daemon_fallback_log_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(text) => summarize(&text, max_recent),
        Err(_) => FallbackRollup::default(),
    }
}

/// How many recent fallbacks `soldr doctor` lists inline.
pub(crate) const DOCTOR_RECENT_LIMIT: usize = 5;

/// Render one entry's age relative to `now_ms`. Pure so the human path
/// has deterministic coverage; `print_section` passes the wall clock.
fn format_age(ts_ms: u64, now_ms: u64) -> String {
    if ts_ms == 0 {
        return "time unknown".to_string();
    }
    let secs = now_ms.saturating_sub(ts_ms) / 1000;
    if secs < 90 {
        format!("{secs}s ago")
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else if secs < 172_800 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Human section shared by `soldr doctor` and `soldr status`, as lines.
///
/// Pure so the wording is unit-testable without capturing stdout, matching
/// `startup_trace::render_line`. The framing is the point and is easy to rot
/// back: these records describe pre-cutover runs, so the section must not
/// read as a live condition or offer a recovery for one (soldr#2424).
///
/// The wording is context-neutral so it reads correctly in both callers
/// (doctor prints the timeout table too; status does not).
fn render_section(rollup: &FallbackRollup, now_ms: u64) -> Vec<String> {
    if rollup.total == 0 {
        return vec![
            "compile-daemon fallbacks: none recorded (the compile cache was never bypassed)"
                .to_string(),
        ];
    }
    let mut lines = vec![format!(
        "compile-daemon fallbacks: {} historical record(s) -- these builds ran UNCACHED via \
         direct rustc (soldr#1838) under a pre-cutover soldr. Nothing appends here any more; \
         cacheable work now hard-fails instead of silently bypassing the daemon",
        rollup.total
    )];
    lines.extend(
        rollup
            .recent
            .iter()
            .map(|entry| format!("  - {}: {}", format_age(entry.ts_ms, now_ms), entry.reason)),
    );
    lines.push(
        "  nothing to recover: this is a record of past runs, not a current condition. For a \
         wedged cache now, see `soldr doctor` / `soldr status` / `soldr logs paths`, then \
         `soldr daemon stop` followed by `soldr daemon start`"
            .to_string(),
    );
    lines
}

/// Human section shared by `soldr doctor` and `soldr status`.
pub(crate) fn print_section(rollup: &FallbackRollup) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for line in render_section(rollup, now_ms) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK_A: &str = r#"{"schema_version":1,"event":"compile_daemon_fallback","ts_ms":1000,"reason":"daemon unavailable"}"#;
    const FALLBACK_B: &str = r#"{"schema_version":1,"event":"compile_daemon_fallback","ts_ms":3000,"reason":"reply timed out"}"#;

    #[test]
    fn an_empty_journal_is_an_empty_rollup() {
        assert_eq!(summarize("", 5), FallbackRollup::default());
        assert_eq!(summarize("   \n\n  ", 5), FallbackRollup::default());
    }

    #[test]
    fn counts_every_fallback_and_orders_recent_newest_first() {
        let jsonl = format!("{FALLBACK_A}\n{FALLBACK_B}\n");
        let rollup = summarize(&jsonl, 5);
        assert_eq!(rollup.total, 2);
        // Newest (ts 3000) first even though it is second in the journal.
        assert_eq!(rollup.recent[0].reason, "reply timed out");
        assert_eq!(rollup.recent[1].reason, "daemon unavailable");
    }

    #[test]
    fn max_recent_bounds_the_window_but_not_the_total() {
        let jsonl = format!("{FALLBACK_A}\n{FALLBACK_B}\n{FALLBACK_A}\n{FALLBACK_B}\n");
        let rollup = summarize(&jsonl, 2);
        assert_eq!(rollup.total, 4, "total counts all records");
        assert_eq!(rollup.recent.len(), 2, "window is bounded");
    }

    #[test]
    fn malformed_lines_and_foreign_events_are_skipped_not_fatal() {
        // A corrupt journal must never make the rollup (and thus doctor) fail.
        let jsonl = format!(
            "not json at all\n\
             {{\"event\":\"something_else\",\"ts_ms\":9}}\n\
             {FALLBACK_A}\n\
             {{ truncated",
        );
        let rollup = summarize(&jsonl, 5);
        assert_eq!(rollup.total, 1, "only the one valid fallback counts");
        assert_eq!(rollup.recent[0].reason, "daemon unavailable");
    }

    #[test]
    fn a_missing_timestamp_or_reason_degrades_but_still_counts() {
        let line = r#"{"event":"compile_daemon_fallback"}"#;
        let rollup = summarize(line, 5);
        assert_eq!(rollup.total, 1);
        assert_eq!(rollup.recent[0].ts_ms, 0);
        assert_eq!(rollup.recent[0].reason, "(reason unrecorded)");
    }

    #[test]
    fn age_rendering_picks_a_sensible_unit() {
        let now = 1_000_000_000u64;
        assert_eq!(format_age(0, now), "time unknown");
        assert_eq!(format_age(now - 5_000, now), "5s ago");
        assert_eq!(format_age(now - 600_000, now), "10m ago");
        assert_eq!(format_age(now - 7_200_000, now), "2h ago");
        assert_eq!(format_age(now - 259_200_000, now), "3d ago");
    }

    /// soldr#2424: the section must not present historical records as a
    /// live condition, nor offer a recovery for one. This is wording, and
    /// wording rots -- pin the three claims that carry the meaning.
    #[test]
    fn a_non_empty_rollup_reads_as_history_not_as_a_live_condition() {
        let rollup = FallbackRollup {
            total: 2,
            recent: vec![FallbackEntry {
                ts_ms: 1_000,
                reason: "daemon unreachable".into(),
            }],
        };

        let rendered = render_section(&rollup, 61_000).join("\n");

        assert!(
            rendered.contains("historical record(s)"),
            "must be framed as history: {rendered}"
        );
        assert!(
            rendered.contains("Nothing appends here any more"),
            "must say the mechanism is gone: {rendered}"
        );
        assert!(
            rendered.contains("nothing to recover"),
            "must not offer a recovery for a past run: {rendered}"
        );
        // The pre-cutover advice was to bypass the cache. That is exactly
        // the stale advice soldr#2424 asks to purge, and the current
        // recovery story is doctor/status/logs + a daemon restart.
        assert!(
            !rendered.contains("--no-cache"),
            "must not recommend bypassing the cache: {rendered}"
        );
        assert!(
            rendered.contains("soldr daemon stop"),
            "must point at the current recovery: {rendered}"
        );
        // The entry itself still has to be readable.
        assert!(rendered.contains("daemon unreachable"), "{rendered}");
    }

    #[test]
    fn an_empty_rollup_still_reads_as_healthy() {
        let rendered = render_section(&FallbackRollup::default(), 0).join("\n");
        assert!(rendered.contains("none recorded"), "{rendered}");
        assert!(!rendered.contains("nothing to recover"), "{rendered}");
    }
}
