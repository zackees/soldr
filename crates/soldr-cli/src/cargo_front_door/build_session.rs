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
//! and #1814 (the resulting double-spawn contending for `state.redb`), and
//! belongs in its own reviewed change.

use std::path::Path;

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::client::DaemonCompileLimit;

use super::new_build_record;

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
    if let Err(err) = persist_start_fallback_inner(paths, session_id, repo_root, started_at_ms) {
        eprintln!(
            "soldr warning: failed to persist build-session start fallback for {session_id}: {err}"
        );
    }
}

pub(super) fn persist_start_fallback_inner(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> Result<(), SoldrError> {
    let db_path = crate::cache_lib::data_db_path(paths);
    // Issue #2224 item 2: ONE open for the whole fallback. Each
    // `open_state_db` acquires redb's exclusive whole-file lock behind a 5 s
    // retry budget, so the previous get/upsert/append triple paid that cost
    // three times — and under contention could burn 15 s and still lose the
    // record. The handle is dropped at the end of this function, releasing
    // both the file lock and the process-wide open mutex.
    let db = crate::cache_lib::redb_lock::open_state_db(&db_path)
        .map_err(|e| SoldrError::Other(format!("open build history: {e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jobs::{JobsSource, ResolvedJobs};
    use crate::timed_test;

    fn daemon(jobs: usize, source: &str) -> DaemonCompileLimit {
        DaemonCompileLimit {
            jobs,
            source: source.to_string(),
        }
    }

    timed_test!(a_matching_limit_says_nothing, {
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
    });

    timed_test!(matching_numbers_from_different_sources_still_say_nothing, {
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
    });

    timed_test!(a_drifted_limit_names_both_values_and_the_fix, {
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
    });

    timed_test!(drift_is_reported_in_both_directions, {
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
    });
}
