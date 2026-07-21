//! Tokio-based soldr-daemon server. Accepts connections on a platform
//! socket / named pipe, decodes one frame per connection, dispatches
//! through the redb-backed target registry, and tracks lightweight
//! per-process state (request count, last activity, optional linked
//! zccache pid placeholder for Phase 3).

use crate::cache_lib::cook_index::{self, CookEntry, CookKey};
#[cfg(unix)]
use crate::cache_lib::daemon_sock_path;
use crate::cache_lib::target_registry::TargetRegistry;
use crate::cache_lib::{data_db_path, soldr_daemon_dir};
use crate::core::SoldrPaths;
use crate::daemon::backend_handle_adoption::{
    current_daemon_process, soldr_backend_endpoint_mux, LEGACY_FRAME_HEADER_BYTES,
};
use crate::daemon::db;
use crate::daemon::event_batcher::EventBatcher;
use crate::daemon::ipc::{read_frame_async_with_prefix, write_frame_async};
use crate::daemon::lifecycle::{append_lifecycle_event, is_live, remove_pid_file, write_pid_file};
use crate::daemon::protocol::{
    BuildRecord, CookStats, Request, Response, StatusInfo, CHUNK_BYTES, COMPILE_BACKEND_EMBEDDED,
    PROTOCOL_VERSION,
};
use crate::zccache_embedded::SoldrZccacheService;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Upper bound on the drift diagnostic returned in [`Response::CookMiss`].
/// Keeps the body well under [`crate::daemon::protocol::MAX_BODY_BYTES`]
/// and matches PR 3's expectation of "most recent N prior recipe
/// hashes for this origin+target".
const COOK_DRIFT_LIMIT: usize = 8;

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub idle_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

#[derive(Debug)]
pub enum ServerError {
    AlreadyRunning(u32),
    #[allow(dead_code)] // surfaced via {:?} on the Err arm in `run`
    Io(std::io::Error),
    #[allow(dead_code)]
    Registry(crate::cache_lib::target_registry::RegistryError),
    #[allow(dead_code)]
    Paths(crate::core::SoldrError),
}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        ServerError::Io(e)
    }
}

impl From<crate::cache_lib::target_registry::RegistryError> for ServerError {
    fn from(e: crate::cache_lib::target_registry::RegistryError) -> Self {
        ServerError::Registry(e)
    }
}

impl From<crate::core::SoldrError> for ServerError {
    fn from(e: crate::core::SoldrError) -> Self {
        ServerError::Paths(e)
    }
}

struct State {
    /// Path to the shared `state.redb`. The daemon opens this on
    /// demand for each write rather than holding the redb handle for
    /// its lifetime — redb refuses concurrent multi-process opens
    /// ("Database already open. Cannot acquire lock."), and the
    /// pre-existing CLI surface (`soldr gc list`, `soldr cache ...`)
    /// also opens the same file directly. Per-request opens are
    /// microseconds in steady state; the daemon's value is single-
    /// writer ordering and build-session correlation, not avoiding
    /// the per-write fs::open cost.
    db_path: PathBuf,
    /// Resolved `~/.soldr/` layout cached for the daemon's lifetime
    /// so cook-artifact path construction does not need to re-probe
    /// HOME/SOLDR_CACHE_DIR on every IPC request.
    paths: SoldrPaths,
    /// Identity returned to `running-process` BackendHandle endpoint
    /// probes. The client-side liveness check only trusts the PID
    /// file after this endpoint response matches.
    daemon_identity: running_process::broker::backend_handle::DaemonProcess,
    start_instant: Instant,
    request_count: AtomicU64,
    last_activity_ms: AtomicU64,
    /// True when the idle watchdog drove the shutdown. Lets the main
    /// task tag the lifecycle event as `died-idle` instead of
    /// `died-shutdown`.
    exit_via_idle: AtomicBool,
    /// In-process count of [`Request::CookLookup`] hits served since
    /// this daemon's last startup. Surfaced via [`Request::Status`]
    /// for `soldr daemon status` and `soldr doctor`. Resets across
    /// daemon restarts by design — long-lived totals belong in the
    /// redb `cook_index_v1` table, not in process-local state.
    cook_hits_this_session: AtomicU64,
    shutdown: Notify,
    /// L4 (issue soldr#980) — background drain task that coalesces
    /// per-compile redb event writes. The `Request::Compile` handler
    /// pushes into a tokio mpsc instead of opening redb directly. See
    /// [`crate::daemon::event_batcher`] for the batching contract.
    event_batcher: EventBatcher,
    /// In-process zccache compile service (issue #977 / #980 L1). The
    /// daemon always owns one — the wrapped/fork-zccache.exe
    /// alternative was removed in the L1 second pass. `start()` is
    /// fallible at daemon boot; if it fails the daemon refuses to
    /// start rather than silently degrade.
    compile_service: Arc<SoldrZccacheService>,
}

impl State {
    fn touch_activity(&self) {
        let elapsed_ms = self.start_instant.elapsed().as_millis() as u64;
        self.last_activity_ms.store(elapsed_ms, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last_ms = self.last_activity_ms.load(Ordering::Relaxed);
        let now_ms = self.start_instant.elapsed().as_millis() as u64;
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }

    fn status(&self) -> StatusInfo {
        // Serialization of concurrent redb opens against `state.redb`
        // is handled inside `cook_index::stats` itself via the shared
        // `redb_lock::state_db_open_lock` (#608) — no extra mutex
        // needed here.
        let (entries, total_bytes) = cook_index::stats(&self.db_path).unwrap_or((0, 0));
        StatusInfo {
            version: PROTOCOL_VERSION,
            pid: std::process::id(),
            uptime_secs: self.start_instant.elapsed().as_secs(),
            request_count: self.request_count.load(Ordering::Relaxed),
            cook_stats: Some(CookStats {
                entries,
                total_bytes,
                hits_this_session: self.cook_hits_this_session.load(Ordering::Relaxed),
            }),
            // Always "embedded" since #980 L1 second pass; field
            // retained for telemetry stability.
            compile_backend: COMPILE_BACKEND_EMBEDDED.to_string(),
        }
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn merge_build_session_start(
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
async fn finalize_build_session(
    db_path: &Path,
    event_batcher: &EventBatcher,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) {
    let aggregate = match event_batcher.take_session_aggregate(session_id) {
        Some(aggregate) => aggregate.finalize(),
        None => {
            event_batcher.flush().await;
            db::aggregate_session(db_path, session_id).unwrap_or((0u32, None, None))
        }
    };
    // One redb open + write txn for the read-modify-write (soldr#1536):
    // the per-open cost grows with db size, so don't pay it twice for a
    // get_build + upsert_build pair.
    let _ = db::finalize_build(db_path, session_id, exit_code, ended_at_ms, aggregate);
    // The SessionEnd terminator rides the batcher, then one final flush
    // makes the terminator AND any still-staged per-compile events
    // durable before the Ack goes out.
    event_batcher
        .record(db::Event {
            ts_ms: ended_at_ms,
            session_id: Some(session_id),
            kind: db::EventKind::SessionEnd,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: Some(exit_code),
        })
        .await;
    event_batcher.flush().await;
}

#[cfg(test)]
mod finalize_build_session_tests {
    //! soldr#1536 regression guards: build-session finalization must be
    //! proportional to the CURRENT session, not to the full retained
    //! event history, while keeping the stats exact.

    use super::finalize_build_session;
    use crate::daemon::db::{self, Event, EventKind};
    use crate::daemon::event_batcher::{write_batch, EventBatcher};
    use crate::timed_test;
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

    timed_test!(finalize_uses_daemon_owned_aggregate_not_history_scan, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = TempDir::new().expect("tempdir");
            let db_path = temp.path().join("state.redb");
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
    });

    timed_test!(
        finalize_falls_back_to_scan_when_daemon_missed_session_start,
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let temp = TempDir::new().expect("tempdir");
                let db_path = temp.path().join("state.redb");
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
    );

    // Scaling evidence for soldr#1536: the aggregate path stays flat
    // while the historical scan grows with retained history. Printed
    // timings (run with `--nocapture`) back the before/after claim;
    // only exactness is asserted so shared-CPU noise cannot flake CI.
    timed_test!(
        finalize_scaling_evidence_across_history_sizes,
        std::time::Duration::from_secs(300),
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                for history in [0usize, 10_000, 100_000] {
                    let temp = TempDir::new().expect("tempdir");
                    let db_path = temp.path().join("state.redb");
                    db::ensure_initialized(&db_path).expect("init");
                    seed_unrelated_history(&db_path, history);

                    let batcher = EventBatcher::start(db_path.clone());
                    batcher
                        .record(session_start(9_999, 1_700_000_000_000))
                        .await;
                    for i in 0..30u64 {
                        for event in
                            compile_pair(9_999, &format!("c{i}"), 100 + i, 1_700_000_000_000)
                        {
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
    );
}

#[cfg(test)]
mod build_session_start_tests {
    use super::merge_build_session_start;
    use crate::daemon::protocol::{BuildCacheSummary, BuildLogPaths, BuildMissReason, BuildRecord};

    crate::timed_test!(late_start_preserves_finalized_build_history, {
        let cache_summary = BuildCacheSummary {
            hits: 1,
            misses: 2,
            non_cacheable: 3,
            errors: 4,
            compilations: 5,
            time_saved_ms: 6,
        };
        let log_paths = BuildLogPaths {
            zccache_session_id: Some("zccache-session".to_string()),
            cache_dir: Some("cache".to_string()),
            session_log_path: Some("last-session.log".to_string()),
            journal_path: Some("last-session.jsonl".to_string()),
            session_stats_path: Some("last-session-stats.json".to_string()),
            compile_journal_path: Some("compile_journal.jsonl".to_string()),
            archived_session_log_path: Some("history/last-session.log".to_string()),
            archived_journal_path: Some("history/last-session.jsonl".to_string()),
            archived_session_stats_path: Some("history/last-session-stats.json".to_string()),
            archived_compile_journal_path: Some("history/compile_journal.jsonl".to_string()),
            private_daemon_name: Some("private-daemon".to_string()),
        };
        let miss_reason = BuildMissReason {
            reason: "source changed".to_string(),
            count: 7,
        };
        let existing = BuildRecord {
            session_id: 42,
            repo_root: "stale".to_string(),
            started_at_ms: 2000,
            ended_at_ms: Some(2600),
            exit_code: Some(0),
            total_wall_ms: Some(600),
            crate_count: 8,
            slowest_crate_us: Some(900),
            slowest_crate_name: Some("slow_crate".to_string()),
            cache_summary: Some(cache_summary.clone()),
            log_paths: Some(log_paths.clone()),
            miss_reasons: vec![miss_reason.clone()],
        };

        let merged = merge_build_session_start(Some(existing), 42, "repo".to_string(), 1000);

        assert_eq!(merged.session_id, 42);
        assert_eq!(merged.repo_root, "repo");
        assert_eq!(merged.started_at_ms, 1000);
        assert_eq!(merged.ended_at_ms, Some(2600));
        assert_eq!(merged.exit_code, Some(0));
        assert_eq!(merged.total_wall_ms, Some(1600));
        assert_eq!(merged.crate_count, 8);
        assert_eq!(merged.slowest_crate_us, Some(900));
        assert_eq!(merged.slowest_crate_name.as_deref(), Some("slow_crate"));
        assert_eq!(merged.cache_summary, Some(cache_summary));
        assert_eq!(merged.log_paths, Some(log_paths));
        assert_eq!(merged.miss_reasons, vec![miss_reason]);
    });
}

/// Start the embedded zccache compile service at daemon boot. Issue
/// #977 / #980 L1 — embedded is mandatory; on failure the daemon
/// itself refuses to start so callers see a hard error instead of a
/// silent degrade. No env-var gating, no feature gating, no fallback.
async fn start_compile_service(
    paths: &SoldrPaths,
    daemon_identity: &running_process::broker::backend_handle::DaemonProcess,
) -> Result<Arc<SoldrZccacheService>, ServerError> {
    match SoldrZccacheService::start(paths, daemon_identity).await {
        Ok(svc) => {
            tracing::info!("soldr-daemon: embedded zccache backend active");
            Ok(Arc::new(svc))
        }
        Err(err) => Err(ServerError::Io(std::io::Error::other(format!(
            "embedded zccache service failed to start: {err}"
        )))),
    }
}

/// Drain the embedded service before the daemon exits. Best-effort:
/// errors are logged but never block daemon exit. The actual shutdown
/// is delegated to `Drop` because `State` still holds an `Arc` clone
/// at this point in `run_async`.
async fn shutdown_compile_service(state: &Arc<State>) {
    if let Err(err) = state.compile_service.flush().await {
        tracing::warn!("soldr-daemon: embedded zccache flush failed: {err}");
    }
    tracing::debug!("soldr-daemon: embedded zccache flushed (service Drop will finalize shutdown)");
}

/// Synchronous entry point used by both the `soldr-daemon` bin target
/// and `soldr daemon start --foreground`. Builds a tokio runtime and
/// blocks until the daemon exits.
/// Env var that turns on the tokio-console layer for the daemon:
/// `SOLDR_DAEMON_TOKIO_CONSOLE=1`. Only functional when soldr-cli is
/// built with the `tokio-console` feature and `RUSTFLAGS="--cfg
/// tokio_unstable"`; otherwise it degrades to a warning (see
/// [`maybe_init_tokio_console`]).
pub const TOKIO_CONSOLE_ENV_VAR: &str = "SOLDR_DAEMON_TOKIO_CONSOLE";

fn tokio_console_requested() -> bool {
    std::env::var(TOKIO_CONSOLE_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Install the tokio-console subscriber layer when
/// [`TOKIO_CONSOLE_ENV_VAR`] is truthy, so `tokio-console` can attach to
/// the daemon's runtime and expose per-task poll/idle time and worker
/// behaviour — the tooling needed to pin down the daemon-worker
/// `sched_yield` spin from soldr#1334 at the async-task level.
///
/// `console_subscriber::spawn()` panics unless the crate was compiled
/// with `--cfg tokio_unstable` (the tokio task instrumentation is
/// otherwise absent). We `catch_unwind` that so a normal release build
/// simply logs a hint instead of crashing the daemon. Mirrors the
/// established `zccache-daemon` pattern. No-op (and no subscriber
/// installed — daemon stays silent as before) when the env var is unset.
#[cfg(feature = "tokio-console")]
fn maybe_init_tokio_console() {
    if !tokio_console_requested() {
        return;
    }
    match std::panic::catch_unwind(console_subscriber::spawn) {
        Ok(console_layer) => {
            use tracing_subscriber::prelude::*;
            let _ = tracing_subscriber::registry()
                .with(console_layer)
                .try_init();
            tracing::info!(
                "tokio-console enabled for soldr-daemon; connect with `tokio-console` \
                 (default 127.0.0.1:6669)"
            );
        }
        Err(_) => {
            eprintln!(
                "soldr-daemon: {TOKIO_CONSOLE_ENV_VAR} set but console-subscriber is \
                 inert — rebuild with RUSTFLAGS=\"--cfg tokio_unstable\" to use tokio-console"
            );
        }
    }
}

#[cfg(not(feature = "tokio-console"))]
fn maybe_init_tokio_console() {
    if tokio_console_requested() {
        eprintln!(
            "soldr-daemon: {TOKIO_CONSOLE_ENV_VAR} set but soldr-cli was built without \
             the `tokio-console` feature; rebuild with `--features tokio-console` and \
             RUSTFLAGS=\"--cfg tokio_unstable\" to use tokio-console"
        );
    }
}

pub fn run(opts: ServerOptions) -> Result<(), ServerError> {
    // Opt-in async-runtime instrumentation. Must run before the runtime
    // is built so console-subscriber's aggregator is in place.
    maybe_init_tokio_console();

    // L6 (soldr#980): cap the daemon Tokio runtime to a small fixed
    // worker count so it doesn't oversubscribe the CPU during cargo
    // builds. The daemon handles IPC + bookkeeping; rustc is the CPU
    // bottleneck. Off-CPU profiling showed `tokio-rt-worker` context
    // switches were ~3x higher cold vs bare and `sched_yield` events
    // tripled — capping at 2-4 workers cuts the unused-worker noise.
    // L6 reverted post-#980 perf measurement: capping the worker pool
    // to 2-4 starved the daemon's Compile dispatch (each compile awaits
    // its rustc subprocess on a worker; the cap forced serial dispatch
    // when cargo wanted N-way parallelism). Use the full host
    // parallelism — the tokio scheduler keeps workers cheap when idle.
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let workers = available.max(2);
    tracing::info!("soldr-daemon Tokio runtime: {workers} workers (host parallelism: {available})");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    runtime.block_on(run_async(opts))
}

/// Async daemon entry point. Use this when calling from inside an
/// existing tokio runtime (e.g. `soldr daemon start --foreground`
/// dispatched from `main`'s `#[tokio::main]` runtime). The synchronous
/// `run` builds its own multi-thread runtime and is the right entry
/// point for the `soldr-daemon` bin target whose `main` has no
/// ambient runtime. Calling `run` from within an ambient runtime
/// panics with "Cannot start a runtime from within a runtime" — that
/// was the failure on soldr#985's perf-matrix CI run.
pub async fn run_async(opts: ServerOptions) -> Result<(), ServerError> {
    let paths = SoldrPaths::new()?;
    std::fs::create_dir_all(soldr_daemon_dir(&paths))?;
    init_embedded_service_file_tracing(&paths);

    if let Some(existing) = is_live(&paths) {
        return Err(ServerError::AlreadyRunning(existing));
    }

    let db_path = data_db_path(&paths);
    // Touch the file at startup so a path error (no parent dir, no
    // permissions) surfaces immediately rather than on the first
    // RecordTargetTouch. Drop the handle right away — see State::db_path.
    let _ = TargetRegistry::open(&db_path)?;
    // Initialize the Phase 2 daemon tables (idempotent). Errors here
    // are non-fatal — Phase 1 target tracking still works without them.
    let _ = db::ensure_initialized(&db_path);
    // Initialize the cook_index_v1 table (issue #576). Idempotent and
    // non-fatal — old soldr versions ignore this table entirely.
    let _ = cook_index::ensure_initialized(&db_path);
    let start_instant = Instant::now();
    let idle_timeout_secs = u32::try_from(opts.idle_timeout.as_secs()).ok();
    let daemon_identity = current_daemon_process(&paths, idle_timeout_secs)
        .map_err(|err| ServerError::Io(std::io::Error::other(err.to_string())))?;

    // Issue #977 / #980 L1 — start the embedded zccache compile
    // service here so the daemon's tokio runtime owns its background
    // tasks. tokio-console sees the union of soldr + zccache work
    // from a single attach. Embedded is mandatory; if start fails,
    // the daemon refuses to come up.
    let compile_service = start_compile_service(&paths, &daemon_identity).await?;

    // L4 (issue soldr#980): start the background event-flusher BEFORE we
    // accept any IPC traffic so the very first compile event lands on
    // a live channel. The drain task lives in the daemon's tokio runtime
    // and exits cleanly via `shutdown()` below.
    let event_batcher = EventBatcher::start(db_path.clone());

    let state = Arc::new(State {
        db_path,
        paths: paths.clone(),
        daemon_identity,
        start_instant,
        request_count: AtomicU64::new(0),
        last_activity_ms: AtomicU64::new(0),
        exit_via_idle: AtomicBool::new(false),
        cook_hits_this_session: AtomicU64::new(0),
        shutdown: Notify::new(),
        event_batcher,
        compile_service,
    });

    write_pid_file(&paths).map_err(|e| match e {
        crate::daemon::lifecycle::LifecycleError::Io(e) => ServerError::Io(e),
        crate::daemon::lifecycle::LifecycleError::NoExe => ServerError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "current_exe unavailable",
        )),
        crate::daemon::lifecycle::LifecycleError::Spawn(e) => ServerError::Io(e),
    })?;
    append_lifecycle_event(&paths, "spawn");

    // soldr#1495: publish this daemon's version claim so a newer client
    // can detect a stale daemon and displace it. Best-effort — a failure
    // to write the manifest (e.g. permission-restricted root on an
    // unusual host) must not stop the daemon; version-aware liveness then
    // simply treats this daemon as version-unknown.
    if let Err(err) = crate::daemon::broker_discovery::write_root_version_claim(&paths) {
        tracing::warn!(target: "soldr::daemon", "failed to publish version claim: {err}");
    }
    let _ = crate::daemon::broker_discovery::publish_cache_manifest(&paths);

    let accept_state = state.clone();
    let paths_for_accept = paths.clone();
    let accept_handle = tokio::spawn(async move {
        let _ = run_accept_loop(paths_for_accept, accept_state).await;
    });

    let idle_state = state.clone();
    let idle_timeout = opts.idle_timeout;
    let idle_handle = tokio::spawn(async move {
        run_idle_watchdog(idle_state, idle_timeout).await;
    });

    let signal_state = state.clone();
    tokio::spawn(async move {
        if (tokio::signal::ctrl_c().await).is_ok() {
            signal_state.shutdown.notify_waiters();
        }
    });
    // Issue #1286 (F1): SIGTERM previously killed the daemon without
    // the graceful-drain path below, silently discarding the embedded
    // zccache's in-memory artifact index + depgraph — a full cold
    // cache on the next daemon start. `pkill soldr-daemon`, container
    // stop, and service managers all send TERM, not INT.
    #[cfg(unix)]
    {
        let term_state = state.clone();
        tokio::spawn(async move {
            let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                return;
            };
            if term.recv().await.is_some() {
                term_state.shutdown.notify_waiters();
            }
        });
    }

    state.shutdown.notified().await;
    accept_handle.abort();
    idle_handle.abort();

    // L4 (issue soldr#980): drain whatever the background event flusher
    // has staged in memory before the daemon process exits. Must run
    // BEFORE the redb file lock is released so the final write txn
    // succeeds; `shutdown()` awaits the drain task's ack.
    state.event_batcher.shutdown().await;

    // Issue #977 / #980 L1: drain the embedded zccache service before
    // any other shutdown work so pending writes flush through.
    shutdown_compile_service(&state).await;

    // Best-effort: remove the endpoint file so a stale socket doesn't
    // confuse the next start. On Windows the named pipe is destroyed
    // when the last handle is closed by the runtime drop below — no
    // cleanup needed there.
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(daemon_sock_path(&paths));
    }
    remove_pid_file(&paths);
    // soldr#1495: drop the version claim so a stale manifest can't outlive
    // its writer and make the next client think a daemon is still live.
    crate::daemon::broker_discovery::remove_root_version_claim(&paths);
    let event = if state.exit_via_idle.load(Ordering::Relaxed) {
        "died-idle"
    } else {
        "died-shutdown"
    };
    append_lifecycle_event(&paths, event);
    Ok(())
}

/// The embedded zccache service runs in-process, so its `tracing::warn!`
/// events otherwise have no durable destination in the normal daemon mode.
/// Keep a daily rolling file under soldr's daemon state for post-build
/// investigation; startup must remain best-effort when the cache is on a
/// read-only or otherwise unusual filesystem.
fn init_embedded_service_file_tracing(paths: &SoldrPaths) {
    let log_dir = embedded_service_log_dir(paths);
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "soldr-daemon: cannot create embedded zccache warning log directory {}: {error}",
            log_dir.display()
        );
        return;
    }
    let appender = tracing_appender::rolling::daily(log_dir, "embedded-zccache.warn.log");
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(appender)
        .try_init();
}

fn embedded_service_log_dir(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join("logs")
}

#[cfg(unix)]
async fn run_accept_loop(paths: SoldrPaths, state: Arc<State>) -> std::io::Result<()> {
    use tokio::net::UnixListener;
    let sock = daemon_sock_path(&paths);
    // Stale socket left over from a previous run blocks bind. The PID
    // file check at startup already proved no live daemon owns it.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, state).await;
        });
    }
}

#[cfg(windows)]
async fn run_accept_loop(paths: SoldrPaths, state: Arc<State>) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let pipe_name = format!(r"\\.\pipe\{}", crate::cache_lib::daemon_pipe_name(&paths));
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe_name)?;
    loop {
        if server.connect().await.is_err() {
            // Re-arm the listener on connect error.
            server = ServerOptions::new().create(&pipe_name)?;
            continue;
        }
        let connected = server;
        // Pre-create the next listener instance so we don't drop the
        // pipe name between clients (issue analogous to socket re-bind).
        server = ServerOptions::new().create(&pipe_name)?;
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_connection(connected, state).await;
        });
    }
}

async fn handle_connection<S>(mut stream: S, state: Arc<State>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use running_process::broker::backend_sdk::MuxPoll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    let mux = soldr_backend_endpoint_mux(state.daemon_identity.clone());
    let mut prefix = [0_u8; LEGACY_FRAME_HEADER_BYTES];
    if !matches!(
        timeout(HANDSHAKE_READ_TIMEOUT, stream.read_exact(&mut prefix)).await,
        Ok(Ok(_))
    ) {
        return Ok(());
    }
    let mut buffered = prefix.to_vec();
    loop {
        match mux.poll(&buffered) {
            Ok(MuxPoll::Legacy) => break,
            Ok(MuxPoll::NeedMoreBytes) => {
                let mut chunk = [0_u8; 4096];
                let read = match timeout(HANDSHAKE_READ_TIMEOUT, stream.read(&mut chunk)).await {
                    Ok(Ok(0)) | Err(_) => return Ok(()),
                    Ok(Ok(n)) => n,
                    Ok(Err(_)) => return Ok(()),
                };
                buffered.extend_from_slice(&chunk[..read]);
            }
            Ok(MuxPoll::ProbeAnswered { reply, .. }) => {
                let _ = timeout(HANDSHAKE_READ_TIMEOUT, stream.write_all(&reply)).await;
                let _ = timeout(HANDSHAKE_READ_TIMEOUT, stream.flush()).await;
                return Ok(());
            }
            Ok(MuxPoll::Payload { .. }) | Err(_) => return Ok(()),
        }
    }

    let req: Request = match read_frame_async_with_prefix(&mut stream, &buffered).await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    state.request_count.fetch_add(1, Ordering::Relaxed);
    state.touch_activity();
    match req {
        Request::RecordTargetTouch { path, unix_seconds } => {
            // Fire-and-forget: open redb just for this write, drop
            // the handle immediately so a concurrent CLI process
            // (`soldr gc list`, `soldr cache report`) can still open
            // the same file. Errors are silent by design.
            if let Ok(registry) = TargetRegistry::open(&state.db_path) {
                let _ = registry.upsert_with_time(Path::new(&path), unix_seconds);
            }
        }
        Request::Status => {
            let info = state.status();
            let _ = write_frame_async(&mut stream, &Response::Status(info)).await;
        }
        Request::Shutdown => {
            let _ = write_frame_async(&mut stream, &Response::ShuttingDown).await;
            state.shutdown.notify_waiters();
        }
        Request::FlushCaches => {
            // Issue #1286 (F1): checkpoint the embedded zccache state
            // (artifact index, depgraph snapshot, metadata cache) to
            // disk without shutting down. `soldr save` / `soldr cache
            // flush` call this before archiving — otherwise the state
            // is memory-only until a graceful daemon exit and archives
            // taken from a live daemon restore with zero rustc hits.
            state.event_batcher.flush().await;
            let response = match state.compile_service.flush().await {
                Ok(_) => Response::Ack,
                Err(err) => Response::Error(format!("embedded zccache flush failed: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::CompileStats => {
            // soldr#1368: return the embedded zccache service's cumulative
            // compile counters so `soldr session start/end` can diff two
            // snapshots into per-session hit/miss stats.
            let response = match state.compile_service.stats().await {
                Ok(info) => Response::CompileStats(info),
                Err(err) => Response::Error(format!("embedded zccache stats failed: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildSessionStart {
            session_id,
            repo_root,
            started_at_ms,
        } => {
            // Issue #980 L7: mark the daemon as "build in progress" so
            // any periodic worker living inside the daemon process
            // (today: none; tomorrow: an inotify cache watcher and a
            // daemon-resident auto-GC tick) skips its tick. Cleared on
            // `BuildSessionEnd` below.
            crate::cache_lib::build_active::set(true);
            let existing = db::get_build(&state.db_path, session_id).ok().flatten();
            let record = merge_build_session_start(existing, session_id, repo_root, started_at_ms);
            let _ = db::upsert_build(&state.db_path, &record);
            // L4 (issue soldr#980): route the SessionStart event through
            // the batcher so we don't compete with the build's first
            // compile-event burst for the redb writer.
            state
                .event_batcher
                .record(db::Event {
                    ts_ms: started_at_ms,
                    session_id: Some(session_id),
                    kind: db::EventKind::SessionStart,
                    crate_name: None,
                    duration_us: None,
                    target_dir: None,
                    exit_code: None,
                })
                .await;
        }
        Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        } => {
            // Issue #980 L7: paired with the `set(true)` in
            // BuildSessionStart above. Re-enables periodic workers.
            crate::cache_lib::build_active::set(false);
            finalize_build_session(
                &state.db_path,
                &state.event_batcher,
                session_id,
                exit_code,
                ended_at_ms,
            )
            .await;
            // soldr#1536: acknowledge the finalization. When the wrapper
            // sees this Ack, the BuildRecord aggregate is persisted and
            // every staged session event is durable in redb, so it can
            // skip its own full-table aggregate re-scan.
            let _ = write_frame_async(&mut stream, &Response::Ack).await;
        }
        Request::ListBuilds { limit, since_ms } => {
            let rows = db::list_builds(&state.db_path, limit, since_ms).unwrap_or_default();
            let _ = write_frame_async(&mut stream, &Response::Builds(rows)).await;
        }
        Request::ListSlowBuilds {
            threshold_ms,
            limit,
        } => {
            let rows =
                db::list_slow_builds(&state.db_path, threshold_ms, limit).unwrap_or_default();
            let _ = write_frame_async(&mut stream, &Response::Builds(rows)).await;
        }
        Request::CookLookup {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            origin_url_normalized,
            branch_lineage,
        } => {
            let key = CookKey {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
            };
            // The two cook_index calls below each serialize their own
            // redb open via `redb_lock::state_db_open_lock` (#608), so
            // no outer mutex is needed. The window between the two
            // opens admits a concurrent writer; the worst case is a
            // stale `previous_origin_recipe_hashes` drift list, which
            // is purely advisory.
            let reply = {
                match cook_index::lookup(&state.db_path, &key) {
                    Ok(Some(entry)) => {
                        state.cook_hits_this_session.fetch_add(1, Ordering::Relaxed);
                        let path = cook_artifact_path(&state.paths, &entry.sha256)
                            .display()
                            .to_string();
                        Response::CookHit {
                            sha256: entry.sha256,
                            path,
                            size_bytes: entry.size_bytes,
                            origin_url_normalized: entry.origin_url_normalized,
                            matched_recipe_hash: Some(recipe_hash),
                            exact_recipe_match: true,
                            branch_name: entry.branch_name,
                        }
                    }
                    Ok(None) => {
                        match cook_index::lookup_origin_fallback(
                            &state.db_path,
                            &key,
                            origin_url_normalized.as_deref(),
                            &branch_lineage,
                        ) {
                            Ok(Some((matched_key, entry))) => {
                                state.cook_hits_this_session.fetch_add(1, Ordering::Relaxed);
                                let path = cook_artifact_path(&state.paths, &entry.sha256)
                                    .display()
                                    .to_string();
                                Response::CookHit {
                                    sha256: entry.sha256,
                                    path,
                                    size_bytes: entry.size_bytes,
                                    origin_url_normalized: entry.origin_url_normalized,
                                    matched_recipe_hash: Some(matched_key.recipe_hash),
                                    exact_recipe_match: false,
                                    branch_name: entry.branch_name,
                                }
                            }
                            Ok(None) => {
                                let previous = cook_index::drift_recipe_hashes(
                                    &state.db_path,
                                    &key,
                                    origin_url_normalized.as_deref(),
                                    COOK_DRIFT_LIMIT,
                                )
                                .unwrap_or_default();
                                Response::CookMiss {
                                    previous_origin_recipe_hashes: previous,
                                }
                            }
                            Err(e) => {
                                Response::Error(format!("cook_index fallback lookup failed: {e}"))
                            }
                        }
                    }
                    Err(e) => Response::Error(format!("cook_index lookup failed: {e}")),
                }
            };
            let _ = write_frame_async(&mut stream, &reply).await;
        }
        Request::CookRecord {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            sha256,
            size_bytes,
            origin_url_normalized,
            branch_name,
            cook_cmd_summary,
        } => {
            let key = CookKey {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
            };
            let now_ms = current_unix_ms();
            let entry = CookEntry {
                sha256,
                size_bytes,
                created_unix_ms: now_ms,
                last_used_unix_ms: now_ms,
                origin_url_normalized,
                cook_cmd_summary,
                branch_name,
            };
            let result = cook_index::upsert(&state.db_path, &key, &entry);
            match result {
                Ok(()) => {
                    let _ = write_frame_async(&mut stream, &Response::Ack).await;
                }
                Err(e) => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Error(format!("cook_index upsert failed: {e}")),
                    )
                    .await;
                }
            }
        }
        Request::CookTouch { sha256 } => {
            // Fire-and-forget bump of last_used_unix_ms. Silent on
            // failure — the caller already moved on.
            let _ = cook_index::touch(&state.db_path, &sha256, current_unix_ms());
        }
        Request::Compile(req) => {
            // Issue #977 / #980 L1: dispatch the rustc compile through
            // the daemon's embedded zccache service. There is no
            // fallback path — embedded is mandatory.
            //
            // #983 Phase 5b: stream the captured stdout/stderr back to
            // the wrapper as a sequence of chunk frames followed by
            // exactly one CompileDone frame. `dispatch_compile_streaming`
            // owns the writer for the duration of the call.
            //
            // Cancellation: `dispatch_compile_streaming` watches the IPC
            // read side for disconnect concurrently with the in-flight
            // compile. If the client (rustc-wrapper) terminates — Ctrl-C
            // on the parent cargo, a hung wrapper killed by the user —
            // the daemon drops the compile future immediately so rustc
            // is cleaned up by its `kill_on_drop` chain rather than
            // grinding to completion on output no one will read.
            if let Err(err) = dispatch_compile_streaming(&state, req, &mut stream).await {
                tracing::warn!("soldr-daemon: streaming compile dispatch failed: {err}");
            }
        }
    }
    Ok(())
}

/// Daemon-side streaming compile dispatcher (issue #983 Phase 5b /
/// soldr#981).
///
/// Calls `SoldrZccacheService::compile`, then splits the captured
/// stdout/stderr `Vec<u8>` into `CHUNK_BYTES`-sized frames before
/// writing them to the connection. The terminal `CompileDone` frame
/// carries the exit code, cache outcome, and (today empty) compile id.
///
/// **Wire contract locked in `tests/phase5_contract.rs`** — that
/// regression test asserts the chunked `Response::CompileStdoutChunk`
/// / `CompileStderrChunk` / `CompileDone` variants round-trip
/// byte-for-byte over the prost codec. If anyone re-introduces the
/// single-frame `Response::Compile(body)` shape from the v6-era
/// fork-zccache.exe path, that test fails with a directive message
/// pointing at #981.
///
/// **Phase 5b1 caveat:** the underlying `compile_service.compile`
/// still returns a fully buffered `CompileResponseBody`, so the
/// daemon briefly holds the entire rustc output in memory before
/// chunking it out. The on-wire saving (smaller per-frame prost
/// encode + zero wrapper-side accumulation) is the immediate win;
/// **Phase 5b2** lifts the daemon-side buffering by switching to the
/// already-published `compile_service.compile_streaming(req,
/// |chunk| …)` API, whose producer side will start emitting chunks
/// incrementally once `zccache#937` (cross-cutting daemon-pipeline
/// streaming) lands in `_vender/zccache/`. The consumer surface is
/// already in place: this function chunks output identically to what
/// `compile_streaming` emits today, so the migration is mechanical
/// and the wire bytes don't change.
async fn dispatch_compile_streaming<S>(
    state: &Arc<State>,
    req: crate::daemon::protocol::CompileRequest,
    stream: &mut S,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Per-compile id for the JSONL phase trace (soldr#981). Cheap —
    // monotonic counter, only meaningful within a single daemon
    // lifetime, which is exactly the scope we need for offline
    // post-cold-build analysis.
    let compile_id = next_compile_id();

    let total = std::time::Instant::now();

    // soldr#1537: lifecycle telemetry uses this already-open compile
    // connection. A disconnected wrapper intentionally leaves a start-only
    // event, preserving the cancellation signal used by build history.
    let lifecycle = req.lifecycle.clone();
    let inner_started = std::time::Instant::now();
    // Keep zccache's compile future behind one heap indirection before it
    // enters the nested lifecycle/disconnect select chain. Staged-output
    // support substantially increased that future's concrete size; carrying
    // it inline through both generic async helpers exhausted Tokio's 2 MiB
    // worker stack under a parallel Cargo cold build.
    let compile_fut = Box::pin(state.compile_service.compile(req));
    let body = match race_compile_with_lifecycle(
        stream,
        compile_fut,
        lifecycle.as_ref(),
        &state.event_batcher,
    )
    .await
    {
        DispatchOutcome::Completed(result) => match result {
            Ok(body) => body,
            Err(err) => {
                crate::daemon::compile_trace::record(
                    "inner_compile_err",
                    inner_started.elapsed().as_micros() as u64,
                    &compile_id,
                );
                let reply = Response::Error(format!("embedded zccache compile failed: {err}"));
                return write_frame_async(stream, &reply).await;
            }
        },
        DispatchOutcome::ClientDisconnected => {
            // The wrapper is gone — its IPC fd closed mid-compile, so
            // the embedded zccache future was dropped at the `select!`
            // boundary above. Don't attempt to write a reply (the pipe
            // is dead) and don't burn CPU finishing a rustc whose
            // output no one will read. Record the disconnect for the
            // per-compile trace so postmortems can correlate.
            crate::daemon::compile_trace::record(
                "client_disconnect_cancelled",
                inner_started.elapsed().as_micros() as u64,
                &compile_id,
            );
            tracing::info!(
                target: "soldr::daemon::compile_stream",
                compile_id = compile_id.as_str(),
                "client disconnected during compile — aborting in-flight work",
            );
            return Ok(());
        }
    };
    crate::daemon::compile_trace::record(
        "inner_compile",
        inner_started.elapsed().as_micros() as u64,
        &compile_id,
    );

    let stdout_len = body.stdout.len();
    let stderr_len = body.stderr.len();
    let mut stdout_chunks = 0usize;
    let mut stderr_chunks = 0usize;

    let wire_stdout_started = std::time::Instant::now();
    for chunk in body.stdout.chunks(CHUNK_BYTES) {
        write_frame_async(stream, &Response::CompileStdoutChunk(chunk.to_vec())).await?;
        stdout_chunks += 1;
        tracing::debug!(
            target: "soldr::daemon::compile_stream",
            bytes = chunk.len(),
            chunk_index = stdout_chunks - 1,
            "stdout chunk emitted",
        );
    }
    crate::daemon::compile_trace::record(
        "wire_stdout",
        wire_stdout_started.elapsed().as_micros() as u64,
        &compile_id,
    );

    let wire_stderr_started = std::time::Instant::now();
    for chunk in body.stderr.chunks(CHUNK_BYTES) {
        write_frame_async(stream, &Response::CompileStderrChunk(chunk.to_vec())).await?;
        stderr_chunks += 1;
        tracing::debug!(
            target: "soldr::daemon::compile_stream",
            bytes = chunk.len(),
            chunk_index = stderr_chunks - 1,
            "stderr chunk emitted",
        );
    }
    crate::daemon::compile_trace::record(
        "wire_stderr",
        wire_stderr_started.elapsed().as_micros() as u64,
        &compile_id,
    );

    let done = Response::CompileDone {
        exit_code: body.exit_code,
        cached: body.cached,
        cache_outcome: body.cache_outcome,
        compile_id: String::new(),
    };
    tracing::debug!(
        target: "soldr::daemon::compile_stream",
        exit_code = body.exit_code,
        cached = body.cached,
        cache_outcome = body.cache_outcome,
        stdout_bytes = stdout_len,
        stderr_bytes = stderr_len,
        stdout_chunks,
        stderr_chunks,
        "compile done — streaming reply complete",
    );
    let wire_done_started = std::time::Instant::now();
    let res = write_frame_async(stream, &done).await;
    crate::daemon::compile_trace::record(
        "wire_done",
        wire_done_started.elapsed().as_micros() as u64,
        &compile_id,
    );
    crate::daemon::compile_trace::record(
        "total_dispatch",
        total.elapsed().as_micros() as u64,
        &compile_id,
    );
    // Co-record per-compile output bytes for cross-axis analysis.
    crate::daemon::compile_trace::record("stdout_bytes", stdout_len as u64, &compile_id);
    crate::daemon::compile_trace::record("stderr_bytes", stderr_len as u64, &compile_id);
    res
}

fn compile_lifecycle_event(
    lifecycle: &crate::daemon::protocol::CompileLifecycle,
    duration_us: Option<u64>,
) -> db::Event {
    db::Event {
        ts_ms: lifecycle.started_at_ms,
        session_id: Some(lifecycle.session_id),
        kind: if duration_us.is_some() {
            db::EventKind::CompileEnd
        } else {
            db::EventKind::CompileStart
        },
        crate_name: Some(lifecycle.crate_name.clone()),
        duration_us,
        target_dir: Some(lifecycle.target_dir.clone()),
        exit_code: None,
    }
}

/// Race a compile against client disconnect while emitting lifecycle events
/// through the daemon's existing batcher. Every accepted session compile gets
/// a start; only a completed future (success or service error) gets an end.
async fn race_compile_with_lifecycle<R, F>(
    reader: &mut R,
    fut: F,
    lifecycle: Option<&crate::daemon::protocol::CompileLifecycle>,
    event_batcher: &crate::daemon::event_batcher::EventBatcher,
) -> DispatchOutcome<F::Output>
where
    R: tokio::io::AsyncRead + Unpin,
    F: std::future::Future,
{
    let started = std::time::Instant::now();
    if let Some(metadata) = lifecycle {
        event_batcher
            .record(compile_lifecycle_event(metadata, None))
            .await;
    }
    let outcome = race_against_disconnect(reader, fut).await;
    if let (DispatchOutcome::Completed(_), Some(metadata)) = (&outcome, lifecycle) {
        let duration_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        event_batcher
            .record(compile_lifecycle_event(metadata, Some(duration_us)))
            .await;
    }
    outcome
}

/// Outcome of [`race_against_disconnect`]. Separated from the inner
/// future's result so the caller can distinguish "the compile finished
/// with an error" (still want to ship the error back to the wrapper)
/// from "the wrapper is gone" (don't write anything; the connection is
/// dead).
pub(crate) enum DispatchOutcome<T> {
    /// The future ran to completion. Carries whatever the future
    /// returned — typically `Result<CompileResponseBody, _>`.
    Completed(T),
    /// The client closed the IPC connection (EOF on `read`) or the OS
    /// reported a broken pipe before the future completed. The future
    /// has been dropped at the `select!` boundary; any RAII cleanup
    /// (notably `kill_on_drop`-marked rustc child processes inside the
    /// embedded zccache service) has been invoked.
    ClientDisconnected,
}

/// Drive `fut` to completion while concurrently watching `reader` for a
/// client disconnect. Returns instantly (within the OS's EOF surfacing
/// latency — microseconds on Unix sockets and Windows named pipes) when
/// the client closes its end of the IPC channel, dropping `fut` so any
/// inflight work it owns is cancelled at the same instant.
///
/// The protocol contract on a `Request::Compile` exchange is strictly
/// request-response: once the wrapper sends the Compile frame it sits
/// blocked waiting for the daemon's response, so any byte arriving on
/// `reader` mid-compile is also treated as a disconnect (it can only
/// be a protocol violation or a stale-frame leftover, and either way
/// the safe action is to abort and close).
///
/// The `biased` `select!` is intentional: when the reader and the
/// compile-future are ready in the same poll tick, prefer the
/// disconnect branch so we don't accidentally write a response into a
/// half-closed pipe.
pub(crate) async fn race_against_disconnect<R, F>(
    reader: &mut R,
    fut: F,
) -> DispatchOutcome<F::Output>
where
    R: tokio::io::AsyncRead + Unpin,
    F: std::future::Future,
{
    use tokio::io::AsyncReadExt;
    tokio::pin!(fut);
    let mut probe = [0_u8; 1];
    tokio::select! {
        biased;
        read = reader.read(&mut probe) => {
            // Ok(0) = clean EOF, Err = broken pipe / reset, Ok(n>0) =
            // unexpected protocol-violating bytes mid-compile. All
            // three mean "the wrapper is gone or wedged" — drop the
            // future and tell the caller to close the connection.
            let _ = read;
            DispatchOutcome::ClientDisconnected
        }
        out = &mut fut => DispatchOutcome::Completed(out),
    }
}

/// Monotonic per-daemon compile counter. The id is stable within one
/// daemon process and meaningless across restarts — exactly the scope
/// the `SOLDR_DAEMON_TRACE` JSONL is designed for.
fn next_compile_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering as AOrdering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, AOrdering::Relaxed);
    format!("c{n:08x}")
}

/// Best-effort resolver for the `~/.soldr/cache/cook/<sha256>.tar.zst`
/// path. Returns the canonical content-addressed file even when the
/// file does not yet exist on disk — the path is informational only;
/// PR 3 (`Response::CookHit` consumer) is responsible for verifying
/// the sha256 of the bytes it reads.
fn cook_artifact_path(paths: &SoldrPaths, sha256: &[u8; 32]) -> PathBuf {
    paths
        .cache
        .join("cook")
        .join(format!("{}.tar.zst", hex_lower(sha256)))
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

async fn run_idle_watchdog(state: Arc<State>, idle_timeout: Duration) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if state.idle_for() >= idle_timeout {
            // Issue #980 L7: a daemon hitting the idle timeout has not
            // seen any IPC traffic for `idle_timeout`, which means any
            // BuildSessionStart that never received a matching
            // BuildSessionEnd should be treated as stale. Clear the
            // flag so a future daemon-resident worker doesn't sleep
            // forever on a leftover `true` value.
            crate::cache_lib::build_active::set(false);
            // Tag the exit reason BEFORE notifying so the main task's
            // post-shutdown lifecycle JSONL emit picks `died-idle`.
            state.exit_via_idle.store(true, Ordering::Relaxed);
            state.shutdown.notify_waiters();
            return;
        }
    }
}

/// Used by the `soldr daemon` CLI to derive sockets and paths in one
/// place. Mirrors [`crate::daemon::client::default_sock_path`].
pub fn server_sock_path(paths: &SoldrPaths) -> PathBuf {
    #[cfg(unix)]
    {
        daemon_sock_path(paths)
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\{}",
            crate::cache_lib::daemon_pipe_name(paths)
        ))
    }
}

#[cfg(test)]
mod cancel_on_disconnect_tests {
    //! TDD regression guard for: "when the soldr CLI is terminated, the
    //! soldr daemon should cancel its outstanding build, and do so
    //! instantly."
    //!
    //! The contract under test is [`race_against_disconnect`]:
    //!
    //!   1. When the IPC reader sees EOF, the inner future is dropped
    //!      synchronously at the `select!` boundary (proven by a
    //!      drop-tracker that flips an atomic from inside `Drop`).
    //!   2. Detection latency is bounded — well under 500ms in practice
    //!      and asserted here at 250ms so that a regression that
    //!      reintroduces a wait-for-timeout style cancellation fails
    //!      the test loudly instead of just running slow.
    //!
    //! These two properties together are what makes daemon-side
    //! cancellation actually useful: if the cancellation either took
    //! seconds to fire or didn't drop the inner work, the daemon would
    //! still be sitting on a rustc compile whose output no one will
    //! read.

    use super::{
        compile_lifecycle_event, race_against_disconnect, race_compile_with_lifecycle,
        DispatchOutcome,
    };
    use crate::daemon::protocol::CompileLifecycle;
    use crate::timed_test;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn test_lifecycle(session_id: u64) -> CompileLifecycle {
        CompileLifecycle {
            session_id,
            crate_name: "demo".to_string(),
            target_dir: "/work/target".to_string(),
            started_at_ms: 1_700_000_000_123,
        }
    }

    async fn flushed_events(
        batcher: &crate::daemon::event_batcher::EventBatcher,
        db_path: &std::path::Path,
        session_id: u64,
    ) -> Vec<crate::daemon::db::Event> {
        batcher.flush().await;
        crate::daemon::db::list_events_for_session(db_path, session_id).expect("list events")
    }

    timed_test!(compile_lifecycle_events_preserve_history_fields, {
        let lifecycle = test_lifecycle(42);
        let start = compile_lifecycle_event(&lifecycle, None);
        assert_eq!(start.kind, crate::daemon::db::EventKind::CompileStart);
        assert_eq!(start.session_id, Some(42));
        assert_eq!(start.crate_name.as_deref(), Some("demo"));
        assert_eq!(start.target_dir.as_deref(), Some("/work/target"));
        assert_eq!(start.ts_ms, 1_700_000_000_123);

        let end = compile_lifecycle_event(&lifecycle, Some(987_654));
        assert_eq!(end.kind, crate::daemon::db::EventKind::CompileEnd);
        assert_eq!(end.duration_us, Some(987_654));
        assert_eq!(end.session_id, start.session_id);
        assert_eq!(end.crate_name, start.crate_name);
        assert_eq!(end.target_dir, start.target_dir);
    });

    timed_test!(successful_compile_records_exactly_start_and_end, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("state.redb");
            let batcher = crate::daemon::event_batcher::EventBatcher::start(db_path.clone());
            let (server, _client) = tokio::io::duplex(64);
            let (mut reader, _writer) = tokio::io::split(server);
            let lifecycle = test_lifecycle(101);
            let outcome = race_compile_with_lifecycle(
                &mut reader,
                async { Ok::<_, &'static str>(7_u32) },
                Some(&lifecycle),
                &batcher,
            )
            .await;
            assert!(matches!(outcome, DispatchOutcome::Completed(Ok(7))));
            let events = flushed_events(&batcher, &db_path, 101).await;
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].kind, crate::daemon::db::EventKind::CompileStart);
            assert_eq!(events[1].kind, crate::daemon::db::EventKind::CompileEnd);
            assert!(events[1].duration_us.is_some());
        });
    });

    timed_test!(compile_service_error_still_records_end, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("state.redb");
            let batcher = crate::daemon::event_batcher::EventBatcher::start(db_path.clone());
            let (server, _client) = tokio::io::duplex(64);
            let (mut reader, _writer) = tokio::io::split(server);
            let lifecycle = test_lifecycle(102);
            let outcome = race_compile_with_lifecycle(
                &mut reader,
                async { Err::<u32, _>("compile service failed") },
                Some(&lifecycle),
                &batcher,
            )
            .await;
            assert!(matches!(outcome, DispatchOutcome::Completed(Err(_))));
            let events = flushed_events(&batcher, &db_path, 102).await;
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].kind, crate::daemon::db::EventKind::CompileStart);
            assert_eq!(events[1].kind, crate::daemon::db::EventKind::CompileEnd);
        });
    });

    timed_test!(client_disconnect_records_start_without_completion, {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let db_path = temp.path().join("state.redb");
            let batcher = crate::daemon::event_batcher::EventBatcher::start(db_path.clone());
            let (server, client) = tokio::io::duplex(64);
            let (mut reader, _writer) = tokio::io::split(server);
            let lifecycle = test_lifecycle(103);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(client);
            });
            let outcome = race_compile_with_lifecycle(
                &mut reader,
                async { tokio::time::sleep(Duration::from_secs(3600)).await },
                Some(&lifecycle),
                &batcher,
            )
            .await;
            assert!(matches!(outcome, DispatchOutcome::ClientDisconnected));
            let events = flushed_events(&batcher, &db_path, 103).await;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].kind, crate::daemon::db::EventKind::CompileStart);
            assert!(events[0].duration_us.is_none());
        });
    });

    /// A future that flips `aborted` to `true` if it is dropped before
    /// completing. Lets the test prove the helper actually cancelled
    /// the in-flight work, not merely stopped polling it.
    struct CancelTracker {
        aborted: Arc<AtomicBool>,
        completed: bool,
    }

    impl Drop for CancelTracker {
        fn drop(&mut self) {
            if !self.completed {
                self.aborted.store(true, Ordering::SeqCst);
            }
        }
    }

    timed_test!(
        race_against_disconnect_aborts_inflight_future_when_client_disconnects,
        Duration::from_secs(10),
        {
            // Use a multi-thread runtime so the disconnect-spawner task
            // can make progress on a different worker while
            // race_against_disconnect parks the calling task on the
            // select!. A current-thread runtime would serialize them,
            // making the latency measurement uninterpretable.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                // tokio::io::duplex gives us a paired in-memory
                // bidirectional stream — exactly the shape of the
                // daemon's per-connection AsyncRead+AsyncWrite stream
                // (Unix socket / Windows named pipe) without the OS
                // round-trip. Dropping one half makes `read` on the
                // other half return Ok(0) immediately, which is the
                // disconnect signal `race_against_disconnect` is built
                // around.
                let (server_side, client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);

                let aborted = Arc::new(AtomicBool::new(false));
                let polled_once = Arc::new(AtomicBool::new(false));
                let aborted_inner = Arc::clone(&aborted);
                let polled_inner = Arc::clone(&polled_once);

                // A "compile" that would sit for an hour if uninterrupted
                // — well past the timed_test! 10s watchdog so the test
                // can only pass via real cancellation. The `tracker` is
                // constructed on the first poll, so we MUST give the
                // select! at least one polling round before the
                // disconnect fires (see the spawn below) — otherwise
                // the inner async block never executes, no tracker is
                // ever created, and the Drop side of the cancellation
                // contract is untestable.
                let slow_compile = async move {
                    let mut tracker = CancelTracker {
                        aborted: aborted_inner,
                        completed: false,
                    };
                    polled_inner.store(true, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    tracker.completed = true;
                    "compile_done"
                };

                // Simulate the CLI dying SHORTLY AFTER the daemon has
                // begun the compile. The 50ms head-start lets the
                // select! poll `slow_compile` at least once (it goes
                // Pending on the 1h sleep), establishing the async
                // state machine WITH `tracker` constructed. Then we
                // drop the client end; the server's reader returns
                // EOF; select! drops the pinned slow_compile; the
                // state-machine drop runs `CancelTracker::drop`,
                // setting `aborted = true` — proving the cancellation
                // really did happen mid-execution.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    drop(client_side);
                });

                let start = Instant::now();
                let outcome = race_against_disconnect(&mut server_reader, slow_compile).await;
                let elapsed = start.elapsed();

                assert!(
                    matches!(outcome, DispatchOutcome::ClientDisconnected),
                    "expected ClientDisconnected, got a Completed variant — \
                     race_against_disconnect did not detect EOF"
                );
                assert!(
                    polled_once.load(Ordering::SeqCst),
                    "slow_compile was never polled — test setup did not give \
                     the future a chance to start. Cancellation is being \
                     tested on a never-started future, which does not match \
                     production reality."
                );
                assert!(
                    elapsed < Duration::from_millis(500),
                    "disconnect detection took {elapsed:?}; the contract is \
                     'instantly' (<500ms including the 50ms scheduled delay \
                     before the disconnect). A regression here usually means \
                     the helper is no longer running the disconnect probe \
                     concurrently with the inner future."
                );
                assert!(
                    aborted.load(Ordering::SeqCst),
                    "slow_compile future was NOT dropped on disconnect — \
                     the inflight build would have continued running. The \
                     `select!` arm must drop the pinned future at its \
                     boundary."
                );
            });
        }
    );

    // Sanity counter-test: when the client does NOT disconnect and the
    // future completes normally, we get `Completed` and the inner
    // future ran to its natural end (no spurious cancellation).
    timed_test!(
        race_against_disconnect_returns_completed_on_happy_path,
        Duration::from_secs(10),
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let (server_side, _client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);

                let aborted = Arc::new(AtomicBool::new(false));
                let aborted_inner = Arc::clone(&aborted);
                let fast = async move {
                    let mut tracker = CancelTracker {
                        aborted: aborted_inner,
                        completed: false,
                    };
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    tracker.completed = true;
                    42_u32
                };

                let outcome = race_against_disconnect(&mut server_reader, fast).await;
                match outcome {
                    DispatchOutcome::Completed(value) => assert_eq!(value, 42),
                    DispatchOutcome::ClientDisconnected => {
                        panic!("unexpected disconnect — client end was held open");
                    }
                }
                assert!(
                    !aborted.load(Ordering::SeqCst),
                    "inner future was cancelled despite running to completion"
                );
            });
        }
    );
}
