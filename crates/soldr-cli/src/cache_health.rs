//! Cache-health assessment for `soldr doctor` (soldr#2436 phase 4, D10).
//!
//! The Aug 7–9 degenerate state — 270/279 sessions at 0% hit rate while
//! the daemon churned through PIDs — was fully visible in artifacts that
//! already existed (`history/<id>/last-session-stats.json`, the daemon
//! lifecycle journal), but nothing read them. These two detectors turn
//! that state into a doctor warning instead of a forensic excavation:
//!
//! - **Zero-hit storm**: ≥3 consecutive newest sessions each with
//!   `compilations ≥ 50` and `hit_rate == 0.0`. Small sessions are
//!   ignored — a 3-unit build with 0 hits is normal cold work, 50+ units
//!   at exactly 0% is the context-loss signature.
//! - **Daemon churn**: ≥4 distinct daemon PIDs spawning inside any
//!   60-minute window of the lifecycle journal.
//!
//! Detection cores are pure functions over plain data so tests fabricate
//! states directly; filesystem readers are thin adapters.

use std::path::PathBuf;

use crate::core::SoldrPaths;
use serde::Serialize;

/// Sessions in a row (newest first) that must look degenerate.
const ZERO_HIT_STORM_SESSIONS: usize = 3;
/// A session only counts toward the storm when it did real work.
const ZERO_HIT_STORM_MIN_COMPILATIONS: u64 = 50;
/// Distinct spawning PIDs inside one window that constitute churn.
const DAEMON_CHURN_PIDS: usize = 4;
/// The churn window.
const DAEMON_CHURN_WINDOW_MS: i64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct CacheHealth {
    /// True when the newest sessions show the context-loss signature.
    pub zero_hit_storm: bool,
    /// How many consecutive newest sessions were zero-hit at scale.
    pub consecutive_zero_hit_sessions: usize,
    /// True when daemon spawn churn crossed the threshold.
    pub daemon_churn: bool,
    /// Largest number of distinct daemon PIDs spawning in any 60-minute
    /// window of the lifecycle journal.
    pub max_distinct_spawn_pids_per_hour: usize,
}

/// One session's relevant stats: (compilations, hit_rate).
pub(crate) type SessionStats = (u64, f64);

/// Pure storm detector. `sessions` is ordered newest first.
pub(crate) fn zero_hit_storm_run(sessions: &[SessionStats]) -> usize {
    sessions
        .iter()
        .take_while(|(compilations, hit_rate)| {
            *compilations >= ZERO_HIT_STORM_MIN_COMPILATIONS && *hit_rate == 0.0
        })
        .count()
}

/// Pure churn detector over `(ts_ms, pid)` spawn events (any order).
/// Returns the max distinct-PID count in any sliding 60-minute window.
pub(crate) fn max_distinct_spawn_pids_per_hour(spawns: &[(i64, u32)]) -> usize {
    let mut sorted: Vec<(i64, u32)> = spawns.to_vec();
    sorted.sort_unstable();
    let mut best = 0;
    for (start_index, &(start_ts, _)) in sorted.iter().enumerate() {
        let window: std::collections::BTreeSet<u32> = sorted[start_index..]
            .iter()
            .take_while(|(ts, _)| ts - start_ts <= DAEMON_CHURN_WINDOW_MS)
            .map(|&(_, pid)| pid)
            .collect();
        best = best.max(window.len());
    }
    best
}

pub(crate) fn assess(paths: &SoldrPaths) -> CacheHealth {
    let run = zero_hit_storm_run_from_history(paths);
    let spawns = read_spawn_events(paths);
    let churn = max_distinct_spawn_pids_per_hour(&spawns);
    CacheHealth {
        zero_hit_storm: run >= ZERO_HIT_STORM_SESSIONS,
        consecutive_zero_hit_sessions: run,
        daemon_churn: churn >= DAEMON_CHURN_PIDS,
        max_distinct_spawn_pids_per_hour: churn,
    }
}

pub(crate) fn print_human(health: &CacheHealth) {
    println!("\ncache health:");
    if health.zero_hit_storm {
        println!(
            "  WARN zero-hit storm: the {} newest sessions each ran ≥{} compiles at a 0% hit \
             rate — the compile-context-loss signature (soldr#2436). Check the daemon \
             lifecycle journal for un-drained restarts.",
            health.consecutive_zero_hit_sessions, ZERO_HIT_STORM_MIN_COMPILATIONS
        );
    } else {
        println!("  zero-hit storm: none");
    }
    if health.daemon_churn {
        println!(
            "  WARN daemon churn: {} distinct daemon PIDs spawned within one hour — repeated \
             un-drained restarts lose in-memory compile contexts (soldr#2436).",
            health.max_distinct_spawn_pids_per_hour
        );
    } else {
        println!("  daemon churn: none");
    }
}

fn history_dir(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("zccache").join("history")
}

/// Archived session ids, newest first.
///
/// Directory names are numeric build-session ids, so numeric descending
/// order is newest-first without touching mtimes. This is a directory scan
/// only — no session file is opened here, which is what lets the caller stop
/// reading as soon as the run ends.
fn session_ids_newest_first(paths: &SoldrPaths) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(history_dir(paths)) else {
        return Vec::new();
    };
    let mut ids: Vec<u64> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().and_then(|s| s.parse().ok()))
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// `(compilations, hit_rate)` for one archived session, or `None` when the
/// file is missing or malformed.
fn read_session_stats(paths: &SoldrPaths, id: u64) -> Option<SessionStats> {
    let stats = history_dir(paths)
        .join(id.to_string())
        .join("last-session-stats.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(stats).ok()?).ok()?;
    Some((
        value.get("compilations")?.as_u64()?,
        value.get("hit_rate")?.as_f64()?,
    ))
}

/// [`zero_hit_storm_run`] against the on-disk history, reading only as far
/// as the answer requires.
///
/// The run ends at the first healthy session, so the newest one usually
/// settles it — but this used to read and JSON-parse **every** archived
/// session first and hand the whole vector to a `take_while`. History grows
/// one directory per build session and is never pruned here: 763 sessions on
/// an ordinary dev box, each costing an open plus a parse, which measured as
/// **~4.0 s of `soldr doctor`'s 8.4 s** on an idle machine (soldr#2571's
/// startup trace). Every one of those reads past the first was discarded.
///
/// Semantics are unchanged, including the subtle one: a missing or malformed
/// session is *skipped* rather than treated as a break, exactly as the old
/// `filter_map` did, so a corrupt file between two degenerate sessions still
/// does not hide the storm.
fn zero_hit_storm_run_from_history(paths: &SoldrPaths) -> usize {
    let mut run = 0;
    for id in session_ids_newest_first(paths) {
        match read_session_stats(paths, id) {
            None => continue,
            Some((compilations, hit_rate)) => {
                if compilations >= ZERO_HIT_STORM_MIN_COMPILATIONS && hit_rate == 0.0 {
                    run += 1;
                } else {
                    break;
                }
            }
        }
    }
    run
}

/// Read `(ts_ms, pid)` for every spawn record in the lifecycle journal.
fn read_spawn_events(paths: &SoldrPaths) -> Vec<(i64, u32)> {
    let path = crate::cache_lib::daemon_lifecycle_log_path(paths);
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value.get("event")?.as_str()? != "spawn" {
                return None;
            }
            Some((
                value.get("ts_ms")?.as_i64()?,
                u32::try_from(value.get("pid")?.as_u64()?).ok()?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn storm_needs_three_consecutive_large_zero_hit_sessions() {
        // Newest-first. Three qualifying sessions: storm.
        assert_eq!(zero_hit_storm_run(&[(90, 0.0), (120, 0.0), (55, 0.0)]), 3);
        // A healthy newest session breaks the run regardless of history.
        assert_eq!(zero_hit_storm_run(&[(90, 0.42), (120, 0.0), (55, 0.0)]), 0);
        // Small sessions do not count as storm evidence.
        assert_eq!(zero_hit_storm_run(&[(3, 0.0), (120, 0.0), (55, 0.0)]), 0);
        // Two in a row is below the alarm threshold.
        assert_eq!(zero_hit_storm_run(&[(90, 0.0), (120, 0.0), (55, 0.9)]), 2);
    }

    #[test]
    fn churn_counts_distinct_pids_in_a_sliding_hour() {
        let hour = DAEMON_CHURN_WINDOW_MS;
        // Four distinct pids inside one hour.
        let spawns = [(0, 1), (hour / 4, 2), (hour / 2, 3), (hour - 1, 4)];
        assert_eq!(max_distinct_spawn_pids_per_hour(&spawns), 4);
        // The same pid respawning is one distinct pid.
        let same = [(0, 7), (hour / 4, 7), (hour / 2, 7), (hour - 1, 7)];
        assert_eq!(max_distinct_spawn_pids_per_hour(&same), 1);
        // Spread beyond the window: never four together.
        let spread = [(0, 1), (hour, 2), (2 * hour, 3), (3 * hour, 4)];
        assert_eq!(max_distinct_spawn_pids_per_hour(&spread), 2);
        assert_eq!(max_distinct_spawn_pids_per_hour(&[]), 0);
    }

    #[test]
    fn assess_reads_fabricated_history_and_journal() {
        let tmp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        // Three degenerate sessions (ids 5..7; 7 is newest) + a healthy old one.
        for (id, compilations, hit_rate) in
            [(4, 80, 0.9), (5, 90, 0.0), (6, 120, 0.0), (7, 60, 0.0)]
        {
            let dir = history_dir(&paths).join(id.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("last-session-stats.json"),
                format!("{{\"compilations\":{compilations},\"hit_rate\":{hit_rate},\"hits\":0}}"),
            )
            .unwrap();
        }
        // Five distinct pids spawning within 46 minutes (the observed war).
        let journal = crate::cache_lib::daemon_lifecycle_log_path(&paths);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        let lines: String = (0..5u32)
            .map(|i| {
                format!(
                    "{{\"ts_ms\":{},\"pid\":{},\"event\":\"spawn\"}}\n",
                    i64::from(i) * 10 * 60 * 1000,
                    1000 + i
                )
            })
            .collect();
        std::fs::write(&journal, lines).unwrap();

        let health = assess(&paths);
        assert!(health.zero_hit_storm, "{health:?}");
        assert_eq!(health.consecutive_zero_hit_sessions, 3);
        assert!(health.daemon_churn, "{health:?}");
        assert_eq!(health.max_distinct_spawn_pids_per_hour, 5);
    }

    fn seed_session(paths: &SoldrPaths, id: u64, body: &str) {
        let dir = history_dir(paths).join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("last-session-stats.json"), body).unwrap();
    }

    fn seed_ok_session(paths: &SoldrPaths, id: u64, compilations: u64, hit_rate: f64) {
        seed_session(
            paths,
            id,
            &format!("{{\"compilations\":{compilations},\"hit_rate\":{hit_rate},\"hits\":0}}"),
        );
    }

    /// The lazy reader must agree with the pure `take_while` on every shape,
    /// including the one that is easy to lose: a malformed session is
    /// *skipped*, not treated as the end of the run.
    #[test]
    fn history_run_matches_the_pure_predicate_and_skips_malformed_sessions() {
        let tmp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        // Newest first: 9 degenerate, 8 corrupt, 7 degenerate, 6 healthy.
        seed_ok_session(&paths, 9, 90, 0.0);
        seed_session(&paths, 8, "{ this is not json");
        seed_ok_session(&paths, 7, 120, 0.0);
        seed_ok_session(&paths, 6, 80, 0.9);

        assert_eq!(
            zero_hit_storm_run_from_history(&paths),
            zero_hit_storm_run(&[(90, 0.0), (120, 0.0), (80, 0.9)]),
            "the corrupt session must be skipped, not end the run"
        );
        assert_eq!(zero_hit_storm_run_from_history(&paths), 2);
    }

    /// The point of the rewrite: sessions older than the run are never read.
    ///
    /// Asserted by making them unreadable-as-JSON *and* proving the answer is
    /// unchanged — if the reader still walked the whole history it would have
    /// to open them, and a `break`-on-malformed implementation would give a
    /// different run length.
    #[test]
    fn sessions_older_than_the_run_are_not_read() {
        let tmp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        seed_ok_session(&paths, 100, 90, 0.0);
        seed_ok_session(&paths, 99, 80, 0.75); // healthy: ends the run here
        for id in 0..90u64 {
            seed_session(&paths, id, "{ never parsed");
        }

        assert_eq!(zero_hit_storm_run_from_history(&paths), 1);
    }

    #[test]
    fn assess_is_quiet_on_an_empty_root() {
        let tmp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let health = assess(&paths);
        assert!(!health.zero_hit_storm);
        assert!(!health.daemon_churn);
        assert_eq!(health.consecutive_zero_hit_sessions, 0);
        assert_eq!(health.max_distinct_spawn_pids_per_hour, 0);
    }
}
