//! Read-only rollup of compile-daemon fallback events for `soldr doctor`
//! (soldr#1838 Phase 4).
//!
//! When a cacheable compile cannot reach the daemon it degrades to direct
//! rustc and appends a JSONL record to `logs/compile-daemon-fallbacks.jsonl`
//! (`compile_dispatch::append_compile_daemon_fallback_event`). Nothing ever
//! surfaced that journal, so a build silently running uncached -- the exact
//! "quietly 10-50x slower, indefinitely" failure #1838 calls out -- was
//! invisible unless you already knew to open that file. This summarises it
//! into `soldr doctor`, next to the timeout section from Phase 3.
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
    entries.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
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

/// Human section shared by `soldr doctor` and `soldr status`. The wording
/// is context-neutral so it reads correctly in both (doctor prints the
/// timeout table too; status does not).
pub(crate) fn print_section(rollup: &FallbackRollup) {
    if rollup.total == 0 {
        println!("compile-daemon fallbacks: none recorded (the compile cache was never bypassed)");
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    println!(
        "compile-daemon fallbacks: {} recorded -- these builds ran UNCACHED via direct rustc \
         (soldr#1838); a persistent count here is the silent-slowdown symptom",
        rollup.total
    );
    for entry in &rollup.recent {
        println!("  - {}: {}", format_age(entry.ts_ms, now_ms), entry.reason);
    }
    println!(
        "  recover: `soldr --no-cache cargo ...` bypasses the wrapper cleanly; a wedged daemon \
         is cleared by `soldr daemon stop`. `soldr doctor` shows the active timeout bounds."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;

    const FALLBACK_A: &str = r#"{"schema_version":1,"event":"compile_daemon_fallback","ts_ms":1000,"reason":"daemon unavailable"}"#;
    const FALLBACK_B: &str = r#"{"schema_version":1,"event":"compile_daemon_fallback","ts_ms":3000,"reason":"reply timed out"}"#;

    timed_test!(an_empty_journal_is_an_empty_rollup, {
        assert_eq!(summarize("", 5), FallbackRollup::default());
        assert_eq!(summarize("   \n\n  ", 5), FallbackRollup::default());
    });

    timed_test!(counts_every_fallback_and_orders_recent_newest_first, {
        let jsonl = format!("{FALLBACK_A}\n{FALLBACK_B}\n");
        let rollup = summarize(&jsonl, 5);
        assert_eq!(rollup.total, 2);
        // Newest (ts 3000) first even though it is second in the journal.
        assert_eq!(rollup.recent[0].reason, "reply timed out");
        assert_eq!(rollup.recent[1].reason, "daemon unavailable");
    });

    timed_test!(max_recent_bounds_the_window_but_not_the_total, {
        let jsonl = format!("{FALLBACK_A}\n{FALLBACK_B}\n{FALLBACK_A}\n{FALLBACK_B}\n");
        let rollup = summarize(&jsonl, 2);
        assert_eq!(rollup.total, 4, "total counts all records");
        assert_eq!(rollup.recent.len(), 2, "window is bounded");
    });

    timed_test!(malformed_lines_and_foreign_events_are_skipped_not_fatal, {
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
    });

    timed_test!(a_missing_timestamp_or_reason_degrades_but_still_counts, {
        let line = r#"{"event":"compile_daemon_fallback"}"#;
        let rollup = summarize(line, 5);
        assert_eq!(rollup.total, 1);
        assert_eq!(rollup.recent[0].ts_ms, 0);
        assert_eq!(rollup.recent[0].reason, "(reason unrecorded)");
    });

    timed_test!(age_rendering_picks_a_sensible_unit, {
        let now = 1_000_000_000u64;
        assert_eq!(format_age(0, now), "time unknown");
        assert_eq!(format_age(now - 5_000, now), "5s ago");
        assert_eq!(format_age(now - 600_000, now), "10m ago");
        assert_eq!(format_age(now - 7_200_000, now), "2h ago");
        assert_eq!(format_age(now - 259_200_000, now), "3d ago");
    });
}
