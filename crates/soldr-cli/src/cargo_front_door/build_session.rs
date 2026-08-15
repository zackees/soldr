//! Opening a build session with the daemon, and noticing when the daemon's
//! compile limit no longer matches what this invocation resolves to
//! (soldr#2023).
//!
//! # Why the warning lives on this exact request
//!
//! A daemon resolves its compile limit once, at startup, and keeps it for
//! its whole life. So `SOLDR_JOBS=4 soldr cargo build` against an already
//! running daemon changes nothing — and, before this, said nothing. The knob
//! appeared to work and did not, which is worse than a knob that reports it
//! cannot help.
//!
//! The front door already sends exactly one `BuildSessionStart` per build,
//! so the daemon answers it with its applied limit and the comparison costs
//! no additional IPC. That mattered enough to shape the design: the obvious
//! alternative — a `Status` round-trip on the build path — would add a
//! request to a path with a documented ~740 ms fixed overhead on Windows
//! (#1843), to deliver a line that is silent almost every time.
//!
//! The warning is *only* a warning. Displacing the running daemon to apply
//! the new limit is deliberately not done here: that writes to the path
//! behind #1865 (a healthy-but-busy daemon killed for failing a 2 s ping)
//! and #1814 (the resulting double-spawn contending for `state.sqlite3`), and
//! belongs in its own reviewed change.

use std::path::Path;

#[cfg(test)]
use crate::core::SoldrError;
use crate::core::SoldrPaths;
use crate::daemon::client::DaemonCompileLimit;

/// Run [`start_and_warn_on_jobs_drift`] on a background thread so its
/// ~740 ms `BuildSessionStart` IPC (soldr#1843) overlaps cargo instead of
/// blocking the front door before it.
///
/// The caller **must** `join()` the returned handle before sending
/// `BuildSessionEnd`, so the daemon still observes START strictly before END.
/// This is sound because nothing between here and the cargo spawn consumes the
/// IPC result (the drift line is a side-effect print), and the daemon's
/// session-start/-end handlers are merge-based, so the request the daemon sees
/// is identical — only the client stops blocking on the ack.
pub(super) fn spawn_start_and_warn_on_jobs_drift(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> std::thread::JoinHandle<()> {
    let paths = paths.clone();
    let repo_root = repo_root.to_path_buf();
    std::thread::spawn(move || {
        start_and_warn_on_jobs_drift(&paths, session_id, &repo_root, started_at_ms);
    })
}

/// Open the build session, then report any drift between the daemon's
/// applied compile limit and this invocation's resolution.
///
/// Failure to reach the daemon is not an error here — it routes to the
/// durable fallback exactly as before, and simply yields no comparison.
pub(super) fn start_and_warn_on_jobs_drift(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) {
    match crate::daemon::client::build_session_start(paths, session_id, repo_root, started_at_ms) {
        Ok(daemon_limit) => {
            let local = crate::core::jobs::resolve_compile_jobs(
                paths
                    .load_config()
                    .ok()
                    .and_then(|c| c.jobs.max_parallel_compiles),
            );
            if let Some(warning) = drift_warning(&daemon_limit, local) {
                eprintln!("{warning}");
            }
        }
        Err(_) => persist_start_fallback(paths, session_id, repo_root, started_at_ms),
    }
}

/// The warning text, or `None` when the running daemon already has the
/// limit this invocation asks for.
///
/// Split out as a pure function because the interesting part is *when* it
/// stays quiet: a matching limit must produce no output even when the two
/// sides resolved it through different precedence tiers, since an identical
/// number means the build behaves exactly as asked and there is nothing to
/// tell anyone.
pub(super) fn drift_warning(
    daemon: &DaemonCompileLimit,
    local: crate::core::jobs::ResolvedJobs,
) -> Option<String> {
    if daemon.jobs == local.jobs {
        return None;
    }
    Some(format!(
        "soldr warning: this build will use {daemon_jobs} concurrent compiles \
         (from {daemon_source}), not the {local_jobs} you asked for (from {local_source}).\n\
         soldr warning: the running daemon resolved its limit when it started and keeps it \
         for its lifetime. Run `soldr daemon stop` to apply {local_jobs}; the next build \
         starts a new daemon.",
        daemon_jobs = daemon.jobs,
        daemon_source = daemon.source,
        local_jobs = local.jobs,
        local_source = local.source.describe(),
    ))
}

/// Durable stand-in for the daemon's own session-start bookkeeping, used
/// when the daemon is unreachable.
fn persist_start_fallback(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) {
    let _ = (paths, repo_root, started_at_ms);
    tracing::warn!(
        event = "build_session_start_daemon_unavailable",
        session_id,
        "build-session start was skipped because the daemon is unavailable"
    );
}

/// Turn a state-DB open failure into a message that says what to do.
///
/// `database is locked` is SQLite's busy-timeout wording, and on its own it
/// reads like corruption — soldr#2223 was filed on exactly that impression
/// (against the redb-era equivalent). It is not corruption: it means another
/// soldr process held the write lock for longer than this one's busy budget.
/// Name that.
#[cfg(test)]
pub(super) fn contention_aware_error(error: impl std::fmt::Display) -> SoldrError {
    let text = error.to_string();
    if !text.contains("database is locked") && !text.contains("database table is locked") {
        return SoldrError::Other(format!("open build history: {text}"));
    }
    SoldrError::Other(format!(
        "open build history: {text}\n\
         soldr note: this is write contention on ~/.soldr/state.sqlite3, not a corrupt database — \
         another soldr process (a concurrent build, or the daemon's maintenance sweep) held the \
         write lock longer than this build was willing to wait. The build itself is unaffected; \
         only this session's history row was skipped."
    ))
}

/// Acquires the state database **exactly once** (soldr#2224).
///
/// This ran three separate `open`s — `get_build`, `upsert_build`,
/// `append_event` — and each one is a full acquire/release of redb's
/// exclusive whole-file lock with its own 5 s contention budget. Under a
/// concurrent build that is up to three independent stalls for one logical
/// session-start, any of which can lose the record outright. One handle,
/// three `_in` operations.
#[cfg(test)]
pub(super) fn persist_start_fallback_inner(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> Result<(), SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    let db = crate::daemon::db::open_handle(&db_path).map_err(contention_aware_error)?;
    if crate::daemon::db::get_build_in(&db, session_id)
        .map_err(|e| SoldrError::Other(format!("read build history: {e}")))?
        .is_none()
    {
        let record = new_build_record(session_id, repo_root.display().to_string(), started_at_ms);
        crate::daemon::db::upsert_build_in(&db, &record)
            .map_err(|e| SoldrError::Other(format!("write build history: {e}")))?;
    }
    let _ = crate::daemon::db::append_event_in(
        &db,
        &crate::daemon::db::Event {
            ts_ms: started_at_ms,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::SessionStart,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: None,
        },
    );
    Ok(())
}

pub(super) fn new_build_record(
    session_id: u64,
    repo_root: String,
    started_at_ms: i64,
) -> crate::daemon::protocol::BuildRecord {
    crate::daemon::protocol::BuildRecord {
        session_id,
        repo_root,
        started_at_ms,
        ended_at_ms: None,
        exit_code: None,
        total_wall_ms: None,
        crate_count: 0,
        slowest_crate_us: None,
        slowest_crate_name: None,
        cache_summary: None,
        log_paths: None,
        miss_reasons: Vec::new(),
    }
}

pub(super) fn persist_build_session_end_fallback(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) {
    let _ = (paths, exit_code, ended_at_ms);
    tracing::warn!(
        event = "build_session_end_daemon_unavailable",
        session_id,
        "build-session end was skipped because the daemon is unavailable"
    );
}

/// Acquires the state database **exactly once** (soldr#2224).
///
/// Previously four opens — `get_build`, `aggregate_session`,
/// `upsert_build`, `append_event` — so one session end could burn four
/// consecutive 5 s contention budgets and still lose the row. See
/// [`persist_start_fallback_inner`] for the same treatment
/// on the start path.
#[cfg(test)]
pub(super) fn persist_build_session_end_fallback_inner(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) -> Result<(), SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    let db = crate::daemon::db::open_handle(&db_path).map_err(contention_aware_error)?;
    let mut record = crate::daemon::db::get_build_in(&db, session_id)
        .map_err(|e| SoldrError::Other(format!("read build history: {e}")))?
        .unwrap_or_else(|| {
            let repo_root = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| ".".to_string());
            new_build_record(session_id, repo_root, ended_at_ms)
        });
    let (crate_count, slowest_crate_us, slowest_crate_name) =
        crate::daemon::db::aggregate_session_in(&db, session_id).unwrap_or((0, None, None));
    record.ended_at_ms = Some(ended_at_ms);
    record.exit_code = Some(exit_code);
    record.total_wall_ms = Some((ended_at_ms - record.started_at_ms).max(0) as u64);
    record.crate_count = crate_count;
    record.slowest_crate_us = slowest_crate_us;
    record.slowest_crate_name = slowest_crate_name;
    crate::daemon::db::upsert_build_in(&db, &record)
        .map_err(|e| SoldrError::Other(format!("write build history: {e}")))?;
    let _ = crate::daemon::db::append_event_in(
        &db,
        &crate::daemon::db::Event {
            ts_ms: ended_at_ms,
            session_id: Some(session_id),
            kind: crate::daemon::db::EventKind::SessionEnd,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: Some(exit_code),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jobs::{JobsSource, ResolvedJobs};

    fn daemon(jobs: usize, source: &str) -> DaemonCompileLimit {
        DaemonCompileLimit {
            jobs,
            source: source.to_string(),
        }
    }

    // soldr#2224 acceptance: one logical session start = one acquisition
    // of `state.sqlite3`.
    //
    // It used to be three (`get_build`, `upsert_build`, `append_event`),
    // each an independent exclusive-lock acquire/release with its own 5 s
    // contention budget. Counting opens is the assertion because the count
    // is the cost — three opens under a concurrent build is up to three
    // stalls for one record.
    #[test]
    fn a_session_start_fallback_opens_the_state_db_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));

        let before = crate::cache_lib::state_store::state_db_open_count();
        persist_start_fallback_inner(&paths, 99, Path::new("/repo"), 1_000).expect("fallback");
        let opens = crate::cache_lib::state_store::state_db_open_count() - before;

        assert_eq!(
            opens, 1,
            "the session-start fallback must acquire state.sqlite3 exactly once (soldr#2224)"
        );
        let stored = crate::daemon::db::get_build(&crate::cache_lib::data_db_path(&paths), 99)
            .expect("read back")
            .expect("record persisted");
        assert_eq!(stored.repo_root, Path::new("/repo").display().to_string());
    }

    // A raw lock string reads like corruption — soldr#2223 was filed on
    // that impression. Contention must say so.
    #[test]
    fn contention_errors_explain_themselves() {
        let text = contention_aware_error("sqlite error: database is locked (code 5)").to_string();
        assert!(text.contains("not a corrupt database"), "{text}");
        assert!(text.contains("write contention"), "{text}");

        // An unrelated failure must not be dressed up as contention.
        let other = contention_aware_error("permission denied").to_string();
        assert!(!other.contains("not a corrupt database"), "{other}");
    }

    #[test]
    fn a_matching_limit_says_nothing() {
        assert_eq!(
            drift_warning(
                &daemon(8, "default"),
                ResolvedJobs {
                    jobs: 8,
                    source: JobsSource::Default,
                },
            ),
            None
        );
    }

    #[test]
    fn matching_numbers_from_different_sources_still_say_nothing() {
        // The build does exactly what was asked for. Reporting a
        // provenance difference that changes no behavior would put a
        // warning on ordinary builds -- e.g. `SOLDR_JOBS=8` on a host
        // whose default is already 8 -- and a warning nobody can act on
        // is how warnings stop being read.
        assert_eq!(
            drift_warning(
                &daemon(8, "default"),
                ResolvedJobs {
                    jobs: 8,
                    source: JobsSource::SoldrJobsEnv,
                },
            ),
            None
        );
    }

    #[test]
    fn a_drifted_limit_names_both_values_and_the_fix() {
        let text = drift_warning(
            &daemon(16, "default"),
            ResolvedJobs {
                jobs: 4,
                source: JobsSource::SoldrJobsEnv,
            },
        )
        .expect("4 != 16 must warn");
        // Both numbers, or the reader cannot tell what they got.
        assert!(text.contains("16 concurrent compiles"), "{text}");
        assert!(text.contains("not the 4 you asked for"), "{text}");
        // The provenance of each, since the whole complaint is that the
        // effective limit was undiscoverable.
        assert!(text.contains("SOLDR_JOBS"), "{text}");
        // The actual remedy. A warning that only reports the problem
        // leaves the reader exactly where #2023 found them.
        assert!(text.contains("soldr daemon stop"), "{text}");
        for line in text.lines() {
            assert!(line.starts_with("soldr warning: "), "unprefixed: {line:?}");
        }
    }

    #[test]
    fn drift_is_reported_in_both_directions() {
        // Asking for *fewer* compiles than the daemon runs is the case
        // that matters most -- it is how someone reins in a machine that
        // is thrashing, and the direction where silently getting the
        // larger number does real harm.
        let fewer = drift_warning(
            &daemon(32, "default"),
            ResolvedJobs {
                jobs: 2,
                source: JobsSource::Config,
            },
        );
        assert!(fewer.is_some());
        let more = drift_warning(
            &daemon(2, "config.toml [jobs].max_parallel_compiles"),
            ResolvedJobs {
                jobs: 32,
                source: JobsSource::SoldrJobsEnv,
            },
        );
        assert!(more.is_some());
    }

    // soldr#2224 acceptance: one logical session end = one acquisition of
    // `state.sqlite3`.
    //
    // This path was the worst offender at four opens — `get_build`,
    // `aggregate_session`, `upsert_build`, `append_event` — so a single
    // session end could burn four consecutive 5 s contention budgets and
    // still lose the row, which is the warning soldr#2223 reported.
    #[test]
    fn a_session_end_fallback_opens_the_state_db_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let db_path = crate::cache_lib::data_db_path(&paths);

        let before = crate::cache_lib::state_store::state_db_open_count();
        persist_build_session_end_fallback_inner(&paths, 7, 0, 5_000).expect("end fallback");
        let opens = crate::cache_lib::state_store::state_db_open_count() - before;

        assert_eq!(
            opens, 1,
            "the session-end fallback must acquire state.sqlite3 exactly once (soldr#2224)"
        );
        let stored = crate::daemon::db::get_build(&db_path, 7)
            .expect("read back")
            .expect("record persisted");
        assert_eq!(stored.exit_code, Some(0));
        assert_eq!(stored.ended_at_ms, Some(5_000));
    }
}
