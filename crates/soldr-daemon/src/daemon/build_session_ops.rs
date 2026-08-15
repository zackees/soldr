//! Build-session IPC handlers: session start merge, session end
//! finalization, and the build-log history attach (soldr#2224).
//!
//! Split out of `server.rs`, which is far over the per-file ceiling. These
//! three are the handlers that touch `state.sqlite3`, so keeping them together
//! also keeps the "one handle per logical operation" rule in one place: each
//! runs its whole read-modify-write inside a single
//! [`db_async::with_handle`] closure rather than opening the database once
//! per step.

use crate::daemon::db::{self, Event, EventKind};
use crate::daemon::db_async;
use crate::daemon::event_batcher::EventBatcher;
use crate::daemon::protocol::{BuildRecord, Response};
use std::path::Path;

pub(super) fn merge_build_session_start(
    existing: Option<BuildRecord>,
    session_id: u64,
    repo_root: String,
    started_at_ms: i64,
) -> BuildRecord {
    let mut record = existing.unwrap_or(BuildRecord {
        session_id,
        repo_root: String::new(),
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
    });
    record.session_id = session_id;
    record.repo_root = repo_root;
    record.started_at_ms = started_at_ms;
    if let Some(ended_at_ms) = record.ended_at_ms {
        record.total_wall_ms = Some((ended_at_ms - started_at_ms).max(0) as u64);
    }
    record
}

/// Finalize a build session (soldr#1536): persist the aggregated
/// BuildRecord and the SessionEnd terminator event, then flush the
/// batcher so everything is durable before the caller acknowledges the
/// wrapper.
///
/// The crate-count / slowest-crate rollup comes from the daemon-owned
/// in-memory [`EventBatcher::take_session_aggregate`] — O(current
/// session) — and only falls back to the historical
/// [`db::aggregate_session`] full-table scan when this daemon did not
/// observe the session from its `SessionStart` (daemon restart or late
/// auto-start mid-build), where redb may hold events the in-memory
/// rollup never saw. In the fallback case the staged events are flushed
/// first so the scan sees them.
pub(super) async fn finalize_build_session(
    db_path: &Path,
    event_batcher: &EventBatcher,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) -> Result<(), String> {
    let owned_aggregate = event_batcher.take_session_aggregate(session_id);
    let aggregate = match owned_aggregate.as_ref() {
        Some(aggregate) => aggregate.clone().finalize(),
        None => {
            event_batcher.flush().await.map_err(|err| err.to_string())?;
            db_async::aggregate_session(db_path, session_id)
                .await
                .map_err(|err| format!("aggregate build session: {err}"))?
        }
    };
    // One redb open + write txn for the read-modify-write (soldr#1536):
    // the per-open cost grows with db size, so don't pay it twice for a
    // get_build + upsert_build pair.
    if let Err(err) =
        db_async::finalize_build(db_path, session_id, exit_code, ended_at_ms, aggregate).await
    {
        if let Some(aggregate) = owned_aggregate {
            event_batcher.restore_session_aggregate(session_id, aggregate);
        }
        return Err(format!("persist build session: {err}"));
    }
    // The SessionEnd terminator rides the batcher, then one final flush
    // makes the terminator AND any still-staged per-compile events
    // durable before the Ack goes out.
    if let Err(err) = event_batcher
        .record(db::Event {
            ts_ms: ended_at_ms,
            session_id: Some(session_id),
            kind: db::EventKind::SessionEnd,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: Some(exit_code),
        })
        .await
    {
        return Err(format!("queue session end event: {err}"));
    }
    event_batcher
        .flush()
        .await
        .map_err(|err| format!("flush session events: {err}"))
}

/// Apply a [`Request::AttachBuildLogHistory`] merge (soldr#1814 slice 2d).
///
/// Split out of the dispatch arm so the merge semantics are testable without
/// a live socket. Mirrors what `persist_build_log_history_inner` used to do
/// CLI-side, but under the daemon's sole ownership of the table.
///
/// Takes a handle rather than a path (soldr#2224): the merge is a
/// read → aggregate → write triple, and opening `state.sqlite3` once per step
/// meant three exclusive-lock acquisitions for one logical update.
pub(super) fn attach_build_log_history(
    db: &rusqlite::Connection,
    update: &crate::daemon::protocol::BuildLogHistoryUpdate,
) -> Response {
    let mut record = match db::get_build_in(db, update.session_id) {
        Ok(Some(record)) => record,
        Ok(None) => crate::daemon::protocol::BuildRecord {
            session_id: update.session_id,
            repo_root: update.repo_root.clone(),
            started_at_ms: update.started_at_ms,
            ended_at_ms: None,
            exit_code: None,
            total_wall_ms: None,
            crate_count: 0,
            slowest_crate_us: None,
            slowest_crate_name: None,
            cache_summary: None,
            log_paths: None,
            miss_reasons: Vec::new(),
        },
        Err(err) => return Response::Error(format!("read build history: {err}")),
    };

    record.cache_summary = update.cache_summary.clone();
    record.miss_reasons = update.miss_reasons.clone();
    // soldr#1536: a daemon-acknowledged BuildSessionEnd already finalized the
    // aggregate, so only recompute when the client says it did not.
    if !update.daemon_finalized {
        let (crate_count, slowest_crate_us, slowest_crate_name) =
            db::aggregate_session_in(db, update.session_id).unwrap_or((0, None, None));
        record.crate_count = crate_count;
        record.slowest_crate_us = slowest_crate_us;
        record.slowest_crate_name = slowest_crate_name;
    }
    // First writer wins, matching the previous local behavior: an
    // already-recorded end time or exit code is authoritative.
    record.ended_at_ms = Some(record.ended_at_ms.unwrap_or(update.ended_at_ms));
    record.exit_code = Some(record.exit_code.unwrap_or(update.exit_code));
    record.total_wall_ms = Some(
        record
            .ended_at_ms
            .map(|ended| (ended - record.started_at_ms).max(0) as u64)
            .unwrap_or(0),
    );
    record.log_paths = update.log_paths.clone();

    match db::upsert_build_in(db, &record) {
        Ok(()) => Response::Ack,
        Err(err) => Response::Error(format!("write build history: {err}")),
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod finalize_build_session_tests {
    //! soldr#1536 regression guards: build-session finalization must be
    //! proportional to the CURRENT session, not to the full retained
    //! event history, while keeping the stats exact.

    use super::finalize_build_session;
    use crate::daemon::db::{self, Event, EventKind};
    use crate::daemon::event_batcher::{write_batch, EventBatcher};
    use std::time::Instant;
    use tempfile::TempDir;

    fn compile_pair(session: u64, name: &str, dur_us: u64, ts_ms: i64) -> [Event; 2] {
        let base = Event {
            ts_ms,
            session_id: Some(session),
            kind: EventKind::CompileStart,
            crate_name: Some(name.to_string()),
            duration_us: None,
            target_dir: Some("/t".into()),
            exit_code: None,
        };
        let mut end = base.clone();
        end.kind = EventKind::CompileEnd;
        end.duration_us = Some(dur_us);
        [base, end]
    }

    fn session_start(session: u64, ts_ms: i64) -> Event {
        Event {
            ts_ms,
            session_id: Some(session),
            kind: EventKind::SessionStart,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: None,
        }
    }

    /// Seed `n` events belonging to other, historical sessions in one
    /// redb transaction.
    fn seed_unrelated_history(db_path: &std::path::Path, n: usize) {
        let mut rows = Vec::with_capacity(n);
        for i in 0..n {
            rows.extend(compile_pair(
                1_000_000 + (i as u64 % 512),
                "history",
                42,
                1_600_000_000_000 + i as i64,
            ));
            if rows.len() >= n {
                rows.truncate(n);
                break;
            }
        }
        write_batch(db_path, &rows).expect("seed history");
    }

    #[test]
    fn finalize_uses_daemon_owned_aggregate_not_history_scan() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = TempDir::new().expect("tempdir");
            let db_path = temp.path().join("state.sqlite3");
            db::ensure_initialized(&db_path).expect("init");

            // Large unrelated history + poison rows carrying THIS
            // session id, planted straight into redb. If finalization
            // scanned the table, the poison rows would inflate the
            // crate count to 5 and steal the slowest slot.
            seed_unrelated_history(&db_path, 10_000);
            let mut poison = Vec::new();
            for i in 0..3 {
                poison.extend(compile_pair(
                    777,
                    "poison",
                    999_999_999,
                    1_600_000_500_000 + i,
                ));
            }
            write_batch(&db_path, &poison).expect("seed poison");

            // The daemon observed session 777 from SessionStart: two
            // real compiles.
            let batcher = EventBatcher::start(db_path.clone());
            batcher.record(session_start(777, 1_700_000_000_000)).await;
            for event in compile_pair(777, "real-a", 1_000, 1_700_000_000_100)
                .into_iter()
                .chain(compile_pair(777, "real-b", 2_000, 1_700_000_000_200))
            {
                batcher.record(event).await;
            }

            let started = Instant::now();
            finalize_build_session(&db_path, &batcher, 777, 0, 1_700_000_001_000).await;
            let elapsed = started.elapsed();
            eprintln!("finalize with 10K-row history + in-memory aggregate: {elapsed:?}");

            let record = db::get_build(&db_path, 777)
                .expect("read build")
                .expect("record");
            assert_eq!(
                record.crate_count, 2,
                "finalization must use the daemon-owned per-session aggregate, \
                 not a full-table scan (a scan would have counted the poison rows)"
            );
            assert_eq!(record.slowest_crate_us, Some(2_000));
            assert_eq!(record.slowest_crate_name.as_deref(), Some("real-b"));
            assert_eq!(record.exit_code, Some(0));
            assert_eq!(record.ended_at_ms, Some(1_700_000_001_000));

            // Durability: the SessionEnd terminator and the session's
            // compile events are flushed to redb before the Ack.
            let events = db::list_events_for_session(&db_path, 777).expect("events");
            assert!(events
                .iter()
                .any(|event| event.kind == EventKind::SessionEnd));
            assert!(events
                .iter()
                .any(|event| event.crate_name.as_deref() == Some("real-b")));
            batcher.shutdown().await;
        });
    }

    #[test]
    fn finalize_falls_back_to_scan_when_daemon_missed_session_start() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = TempDir::new().expect("tempdir");
            let db_path = temp.path().join("state.sqlite3");
            db::ensure_initialized(&db_path).expect("init");

            // Events written by a previous daemon lifetime.
            let mut rows = vec![session_start(888, 1_700_000_000_000)];
            rows.extend(compile_pair(888, "old-a", 5_000, 1_700_000_000_100));
            rows.extend(compile_pair(888, "old-b", 7_000, 1_700_000_000_200));
            write_batch(&db_path, &rows).expect("seed prior-lifetime events");

            // Fresh batcher = restarted daemon with no in-memory state
            // for 888: finalization must fall back to the historical
            // scan and still produce exact stats.
            let batcher = EventBatcher::start(db_path.clone());
            finalize_build_session(&db_path, &batcher, 888, 1, 1_700_000_001_000).await;

            let record = db::get_build(&db_path, 888)
                .expect("read build")
                .expect("record");
            assert_eq!(record.crate_count, 2);
            assert_eq!(record.slowest_crate_us, Some(7_000));
            assert_eq!(record.slowest_crate_name.as_deref(), Some("old-b"));
            assert_eq!(record.exit_code, Some(1));
            batcher.shutdown().await;
        });
    }

    // Scaling evidence for soldr#1536: the aggregate path stays flat
    // while the historical scan grows with retained history. Printed
    // timings (run with `--nocapture`) back the before/after claim;
    // only exactness is asserted so shared-CPU noise cannot flake CI.
    #[test]
    fn finalize_scaling_evidence_across_history_sizes() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            for history in [0usize, 10_000, 100_000] {
                let temp = TempDir::new().expect("tempdir");
                let db_path = temp.path().join("state.sqlite3");
                db::ensure_initialized(&db_path).expect("init");
                seed_unrelated_history(&db_path, history);

                let batcher = EventBatcher::start(db_path.clone());
                batcher
                    .record(session_start(9_999, 1_700_000_000_000))
                    .await;
                for i in 0..30u64 {
                    for event in compile_pair(9_999, &format!("c{i}"), 100 + i, 1_700_000_000_000) {
                        batcher.record(event).await;
                    }
                }

                batcher.flush().await;
                let scan_started = Instant::now();
                let scan = db::aggregate_session(&db_path, 9_999).expect("scan");
                let scan_elapsed = scan_started.elapsed();

                // Baseline for a constant-work redb round-trip at
                // this table size (open + point read), so the
                // finalize timing below can be read against the
                // per-open cost rather than attributed to scanning.
                let point_started = Instant::now();
                let _ = db::get_build(&db_path, 9_999);
                let point_elapsed = point_started.elapsed();

                let fin_started = Instant::now();
                finalize_build_session(&db_path, &batcher, 9_999, 0, 1_700_000_002_000).await;
                let fin_elapsed = fin_started.elapsed();

                let record = db::get_build(&db_path, 9_999)
                    .expect("read build")
                    .expect("record");
                assert_eq!(record.crate_count, 30, "history={history}");
                assert_eq!(record.slowest_crate_us, Some(129));
                assert_eq!(
                    (record.crate_count, record.slowest_crate_us.unwrap()),
                    (scan.0, scan.1.unwrap()),
                    "aggregate and scan must agree (history={history})"
                );
                eprintln!(
                    "history={history:>6} rows: scan={scan_elapsed:?} \
                         point-read={point_elapsed:?} finalize(aggregate)={fin_elapsed:?}"
                );
                batcher.shutdown().await;
            }
        });
    }
}
