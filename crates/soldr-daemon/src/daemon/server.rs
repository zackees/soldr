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
use crate::daemon::lifecycle::{
    append_lifecycle_event, is_live, stale_daemon_occupies_endpoint, write_pid_file,
};
use crate::daemon::protocol::{
    BuildRecord, CookStats, IpcBurstStats, Request, Response, ShutdownAck, StatusInfo, CHUNK_BYTES,
    COMPILE_BACKEND_EMBEDDED, PROTOCOL_VERSION,
};
use crate::zccache_embedded::SoldrZccacheService;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Upper bound on the drift diagnostic returned in [`Response::CookMiss`].
/// Keeps the body well under [`crate::daemon::protocol::MAX_BODY_BYTES`]
/// and matches PR 3's expectation of "most recent N prior recipe
/// hashes for this origin+target".
const COOK_DRIFT_LIMIT: usize = 8;

// The daemon is the primary owner of five-minute pressure checks and daily age
// retention.  It therefore stays resident by default; callers can still opt
// into an explicit nonzero inactivity timeout.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::MAX;
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
const IPC_BACKPRESSURE_RETRY_AFTER_MS: u32 = 25;
const IPC_QUEUE_CAPACITY_MAX: usize = 1024;

/// A bounded outer admission queue for compile IPC. Once admitted, callers
/// wait in the embedded zccache service's fair compile semaphore; holding an
/// owned permit while waiting makes the total queue bounded without changing
/// zccache's user-configured `ZCCACHE_MAX_PARALLEL_COMPILES` policy.
struct CompileAdmission {
    permits: Arc<Semaphore>,
    capacity: u64,
    expected_compile_slots: u64,
    accepted: AtomicU64,
    queued: AtomicU64,
    backpressured: AtomicU64,
    busy_retries: AtomicU64,
    active: AtomicU64,
    queue_high_water: AtomicU64,
}

struct CompileAdmissionPermit<'a> {
    _permit: OwnedSemaphorePermit,
    admission: &'a CompileAdmission,
}

impl Drop for CompileAdmissionPermit<'_> {
    fn drop(&mut self) {
        self.admission.active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl CompileAdmission {
    fn new(capacity: usize, expected_compile_slots: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(capacity)),
            capacity: capacity as u64,
            expected_compile_slots: expected_compile_slots.max(1) as u64,
            accepted: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            backpressured: AtomicU64::new(0),
            busy_retries: AtomicU64::new(0),
            active: AtomicU64::new(0),
            queue_high_water: AtomicU64::new(0),
        }
    }

    fn try_admit(&self) -> Option<CompileAdmissionPermit<'_>> {
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.backpressured.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.accepted.fetch_add(1, Ordering::Relaxed);
        if active > self.expected_compile_slots {
            self.queued.fetch_add(1, Ordering::Relaxed);
        }
        let mut high_water = self.queue_high_water.load(Ordering::Relaxed);
        while active > high_water {
            match self.queue_high_water.compare_exchange_weak(
                high_water,
                active,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => high_water = observed,
            }
        }
        Some(CompileAdmissionPermit {
            _permit: permit,
            admission: self,
        })
    }

    fn record_busy_retries(&self, retries: u32) {
        self.busy_retries
            .fetch_add(u64::from(retries), Ordering::Relaxed);
    }

    fn stats(&self) -> IpcBurstStats {
        IpcBurstStats {
            accepted: self.accepted.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            backpressured: self.backpressured.load(Ordering::Relaxed),
            busy_retries: self.busy_retries.load(Ordering::Relaxed),
            queue_high_water: self.queue_high_water.load(Ordering::Relaxed),
        }
    }
}

fn positive_env_value<F>(name: &str, lookup: F) -> Option<usize>
where
    F: FnOnce(&str) -> Option<String>,
{
    lookup(name)?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
}

fn windows_listener_pool_size_from(logical_cpus: usize, override_value: Option<usize>) -> usize {
    override_value.unwrap_or_else(|| logical_cpus.saturating_mul(4).clamp(16, 128))
}

fn windows_listener_pool_size() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    windows_listener_pool_size_from(
        logical,
        positive_env_value("SOLDR_WINDOWS_PIPE_LISTENER_POOL", |name| {
            std::env::var(name).ok()
        }),
    )
}

fn ipc_queue_capacity(listener_pool_size: usize) -> usize {
    positive_env_value("SOLDR_WINDOWS_PIPE_QUEUE_CAPACITY", |name| {
        std::env::var(name).ok()
    })
    .unwrap_or_else(|| listener_pool_size.saturating_mul(4))
    .clamp(1, IPC_QUEUE_CAPACITY_MAX)
}

/// Slots the outer `CompileAdmission` queue sizes itself for.
///
/// soldr#1761: this used to read `ZCCACHE_MAX_PARALLEL_COMPILES` and,
/// when unset, default to `available_parallelism()` — while the
/// semaphore it admits into defaulted to `available_parallelism() - 1`.
/// Two layers sized from two expressions, so the queue always believed
/// in one more slot than existed. Both now resolve through
/// [`crate::core::jobs`], which also adds `SOLDR_JOBS` and a config
/// field ahead of the zccache-namespaced variable.
fn expected_compile_slots() -> usize {
    crate::core::jobs::resolve_compile_jobs(config_compile_jobs()).jobs
}

/// `[jobs].max_parallel_compiles` from `config.toml`, or `None` when
/// the config is absent or unreadable.
///
/// A malformed config must not stop the daemon from starting — it
/// falls through to the next precedence tier, same as an unset value.
pub(crate) fn config_compile_jobs() -> Option<usize> {
    let paths = crate::core::SoldrPaths::new().ok()?;
    paths.load_config().ok()?.jobs.max_parallel_compiles
}

#[cfg(test)]
mod shutdown_backstop_tests {
    use super::*;

    crate::timed_test!(watchdog_grace_defaults_and_is_only_disabled_explicitly, {
        assert_eq!(parse_watchdog_grace(None), Some(SHUTDOWN_WATCHDOG_GRACE));
        assert_eq!(
            parse_watchdog_grace(Some("90")),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_watchdog_grace(Some(" 90 ")),
            Some(Duration::from_secs(90))
        );

        // Only a literal 0 removes the backstop.
        assert_eq!(parse_watchdog_grace(Some("0")), None);

        // A typo must NOT silently disable the only thing guaranteeing the
        // process exits — fall back to the default instead.
        for bogus in ["", "abc", "-1", "30s", "1.5"] {
            assert_eq!(
                parse_watchdog_grace(Some(bogus)),
                Some(SHUTDOWN_WATCHDOG_GRACE),
                "malformed override {bogus:?} must fall back to the default"
            );
        }
    });

    crate::timed_test!(watchdog_fires_before_the_client_stops_waiting, {
        // If the backstop outlived the client's patience it would be
        // pointless: `daemon stop` would report failure and leave the
        // process running anyway.
        assert!(
            SHUTDOWN_WATCHDOG_GRACE < crate::daemon::lifecycle::GRACEFUL_SHUTDOWN_WAIT_TIMEOUT,
            "watchdog grace must be under the client's graceful-shutdown timeout"
        );
    });

    // `wait()` must observe a `request()` that races it.
    //
    // `notify_waiters()` stores no permit, so the previous
    // `while !is_requested() { notified().await }` could park forever when
    // the request landed between the flag check and the registration. This
    // races the two sides repeatedly; the timeout turns the old hang into a
    // clean failure instead of wedging the suite.
    //
    // Probabilistic by nature — the window is a few instructions wide — so
    // it is a regression net, not a proof. The ordering guarantee itself is
    // established by `enable()`-before-check in `ShutdownSignal::wait`.
    crate::timed_test!(shutdown_wait_observes_a_racing_request, {
        use crate::daemon::maintenance::ShutdownSignal;
        use std::sync::Arc;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio rt");

        rt.block_on(async {
            for iteration in 0..500 {
                let signal = Arc::new(ShutdownSignal::default());
                let waiter = tokio::spawn({
                    let signal = signal.clone();
                    async move { signal.wait().await }
                });
                // Let the waiter get as close to its flag check as possible
                // before the request lands.
                tokio::task::yield_now().await;
                signal.request();

                tokio::time::timeout(Duration::from_secs(10), waiter)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("wait() missed the request on iteration {iteration}")
                    })
                    .expect("waiter task panicked");
            }
        });
    });
}

#[cfg(test)]
mod ipc_burst_tests {
    use super::*;
    use tokio::sync::{mpsc, Mutex};

    crate::timed_test!(listener_pool_defaults_and_override_are_bounded, {
        assert_eq!(windows_listener_pool_size_from(1, None), 16);
        assert_eq!(windows_listener_pool_size_from(12, None), 48);
        assert_eq!(windows_listener_pool_size_from(64, None), 128);
        assert_eq!(windows_listener_pool_size_from(2, Some(3)), 3);
    });

    crate::timed_test!(compile_admission_is_bounded_and_reports_backpressure, {
        let admission = CompileAdmission::new(2, 1);
        let first = admission.try_admit().expect("first request admitted");
        let second = admission.try_admit().expect("second request queued");
        assert!(
            admission.try_admit().is_none(),
            "third request must backpressure"
        );
        let stats = admission.stats();
        assert_eq!(stats.accepted, 2);
        assert_eq!(stats.queued, 1);
        assert_eq!(stats.backpressured, 1);
        assert_eq!(stats.queue_high_water, 2);
        drop(first);
        drop(second);
        assert!(
            admission.try_admit().is_some(),
            "capacity recovers after completion"
        );
    });

    crate::timed_test!(
        windows_burst_policy_keeps_four_pool_sizes_fifo_and_recovers,
        {
            // This is intentionally platform-neutral: it exercises the exact
            // bounded-admission and fair Tokio semaphore policy used by the
            // Windows named-pipe listener, so Linux Docker can validate it while
            // the Windows matrix owns the actual pipe transport.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                const LISTENER_POOL: usize = 16;
                const CLIENTS: usize = LISTENER_POOL * 4;
                let admission = Arc::new(CompileAdmission::new(CLIENTS, 1));
                let compile_gate = Arc::new(Semaphore::new(1));
                let held_compile = compile_gate.clone().acquire_owned().await.expect("permit");
                let completion_order = Arc::new(Mutex::new(Vec::with_capacity(CLIENTS)));
                let (ready_tx, mut ready_rx) = mpsc::channel(CLIENTS);
                let mut joins = Vec::with_capacity(CLIENTS);

                // Queue each request in a known order before submitting the next.
                // Tokio's semaphore preserves this waiter order when the small
                // compile gate opens, which is the FIFO contract for the burst.
                for index in 0..CLIENTS {
                    let admission = admission.clone();
                    let compile_gate = compile_gate.clone();
                    let completion_order = completion_order.clone();
                    let ready_tx = ready_tx.clone();
                    joins.push(tokio::spawn(async move {
                        let _admission = admission.try_admit().expect("queue has room");
                        ready_tx.send(()).await.expect("ready receiver");
                        let _compile = compile_gate.acquire_owned().await.expect("compile permit");
                        completion_order.lock().await.push(index);
                    }));
                    ready_rx.recv().await.expect("request admitted before next");
                }
                drop(held_compile);
                for join in joins {
                    join.await.expect("queued request completes");
                }
                assert_eq!(
                    *completion_order.lock().await,
                    (0..CLIENTS).collect::<Vec<_>>(),
                    "bounded waiting line must preserve FIFO order"
                );
                let stats = admission.stats();
                assert_eq!(stats.accepted, CLIENTS as u64);
                assert_eq!(stats.backpressured, 0);
                assert_eq!(stats.queue_high_water, CLIENTS as u64);
                assert!(
                    admission.try_admit().is_some(),
                    "all capacity recovers after the burst drains"
                );
            });
        }
    );
}

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
    shutdown: Arc<crate::daemon::maintenance::ShutdownSignal>,
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
    compile_admission: CompileAdmission,
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
            generation: self.daemon_identity.started_at_unix_ms,
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
            ipc_burst_stats: self.compile_admission.stats(),
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
) -> Result<(), String> {
    let owned_aggregate = event_batcher.take_session_aggregate(session_id);
    let aggregate = match owned_aggregate.as_ref() {
        Some(aggregate) => aggregate.clone().finalize(),
        None => {
            event_batcher.flush().await.map_err(|err| err.to_string())?;
            db::aggregate_session(db_path, session_id)
                .map_err(|err| format!("aggregate build session: {err}"))?
        }
    };
    // One redb open + write txn for the read-modify-write (soldr#1536):
    // the per-open cost grows with db size, so don't pay it twice for a
    // get_build + upsert_build pair.
    if let Err(err) = db::finalize_build(db_path, session_id, exit_code, ended_at_ms, aggregate) {
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

#[cfg(test)]
#[allow(unused_must_use)]
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
#[allow(unused_must_use)]
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

/// Drain and stop the embedded service before the daemon exits. Best-effort:
/// errors are logged but never block daemon exit.
async fn shutdown_compile_service(state: &Arc<State>) {
    match state
        .compile_service
        .as_ref()
        .clone()
        .shutdown(zccache::embedded::ShutdownMode::Graceful)
        .await
    {
        Ok(()) => tracing::debug!("soldr-daemon: embedded zccache shutdown complete"),
        Err(err) => tracing::warn!("soldr-daemon: embedded zccache shutdown failed: {err}"),
    }
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
/// Optional soldr-owned bridge to console-subscriber's recording path.
///
/// Detached daemons intentionally start from a scrubbed environment and
/// forward only `SOLDR_*`, so callers cannot rely on the upstream
/// `TOKIO_CONSOLE_RECORD_PATH` variable crossing the spawn boundary.
pub const TOKIO_CONSOLE_RECORD_PATH_ENV_VAR: &str = "SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH";

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
    let spawn = || {
        let mut builder = console_subscriber::Builder::default().with_default_env();
        if let Some(path) = std::env::var_os(TOKIO_CONSOLE_RECORD_PATH_ENV_VAR) {
            builder = builder.recording_path(path);
        }
        builder.spawn()
    };
    match std::panic::catch_unwind(spawn) {
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

    let _root_ownership = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)?
        .ok_or_else(|| {
            ServerError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                crate::daemon::lifecycle::describe_root_ownership_conflict(&paths),
            ))
        })?;

    if let Some(existing) = existing_daemon_pid(&paths) {
        return Err(ServerError::AlreadyRunning(existing));
    }
    // Claim the Unix endpoint immediately after the version-blind occupancy
    // check. Delaying bind until a detached accept task used to leave a large
    // initialization window in which an older daemon could bind and then have
    // its live socket unlinked by this process.
    #[cfg(unix)]
    let (unix_listener, unix_socket_identity) = claim_unix_endpoint(&paths)?;

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
        shutdown: Arc::new(crate::daemon::maintenance::ShutdownSignal::default()),
        event_batcher,
        compile_service,
        compile_admission: CompileAdmission::new(
            ipc_queue_capacity(windows_listener_pool_size()),
            expected_compile_slots(),
        ),
    });

    if let Err(error) = write_pid_file(&paths) {
        #[cfg(unix)]
        {
            let _ = remove_unix_socket_if_matches(&daemon_sock_path(&paths), unix_socket_identity);
        }
        return Err(match error {
            crate::daemon::lifecycle::LifecycleError::Io(error) => ServerError::Io(error),
            crate::daemon::lifecycle::LifecycleError::NoExe => ServerError::Io(
                std::io::Error::new(std::io::ErrorKind::NotFound, "current_exe unavailable"),
            ),
            crate::daemon::lifecycle::LifecycleError::Spawn(error) => ServerError::Io(error),
        });
    }
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
    #[cfg(unix)]
    let accept_handle = tokio::spawn(async move {
        let _ = run_accept_loop(unix_listener, accept_state).await;
    });
    #[cfg(windows)]
    let accept_handle = {
        let paths_for_accept = paths.clone();
        tokio::spawn(async move {
            let _ = run_accept_loop(paths_for_accept, accept_state).await;
        })
    };

    let idle_handle = (opts.idle_timeout != Duration::MAX).then(|| {
        let idle_state = state.clone();
        let idle_timeout = opts.idle_timeout;
        tokio::spawn(async move {
            run_idle_watchdog(idle_state, idle_timeout).await;
        })
    });

    let maintenance_context = crate::daemon::maintenance::MaintenanceContext {
        paths: paths.clone(),
        db_path: state.db_path.clone(),
        compile_service: Arc::clone(&state.compile_service),
        shutdown: Arc::clone(&state.shutdown),
    };
    let maintenance_handle = tokio::spawn(async move {
        crate::daemon::maintenance::run_loop(maintenance_context).await;
    });

    let signal_state = state.clone();
    tokio::spawn(async move {
        if (tokio::signal::ctrl_c().await).is_ok() {
            signal_state.shutdown.request();
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
                term_state.shutdown.request();
            }
        });
    }

    state.shutdown.wait().await;
    arm_shutdown_watchdog();
    accept_handle.abort();
    if let Some(handle) = idle_handle {
        handle.abort();
    }
    // A destructive pass that already acquired the root maintenance lease is
    // allowed to finish. In particular, await its spawn_blocking deletion
    // worker before releasing root ownership.
    let _ = maintenance_handle.await;

    // L4 (issue soldr#980): drain whatever the background event flusher
    // has staged in memory before the daemon process exits. Must run
    // BEFORE the redb file lock is released so the final write txn
    // succeeds; `shutdown()` awaits the drain task's ack.
    let _ = state.event_batcher.shutdown().await;

    // Issue #977 / #980 L1: drain the embedded zccache service before
    // any other shutdown work so pending writes flush through.
    shutdown_compile_service(&state).await;

    // Deliberately retain the PID file, version claim, and Unix socket node.
    // A check-then-unlink fence is not atomic: an older Soldr release that
    // does not honor `root-owner.lock` can publish a successor between the
    // check and unlink, and the retiring daemon would delete the successor's
    // state. Startup already probes liveness, overwrites stale claims, and
    // reclaims a stale socket before binding, so retaining these artifacts is
    // safe and closes that cross-version race.
    let event = if state.exit_via_idle.load(Ordering::Relaxed) {
        "died-idle"
    } else {
        "died-shutdown"
    };
    append_lifecycle_event(&paths, event);
    Ok(())
}

/// Env override for [`SHUTDOWN_WATCHDOG_GRACE`], in seconds. `0` disables.
pub const SHUTDOWN_WATCHDOG_ENV_VAR: &str = "SOLDR_SHUTDOWN_WATCHDOG_SECS";

/// How long teardown may run before the process exits regardless.
///
/// Deliberately under `GRACEFUL_SHUTDOWN_WAIT_TIMEOUT` (300s) so the process
/// is gone before the client gives up and reports failure.
pub const SHUTDOWN_WATCHDOG_GRACE: Duration = Duration::from_secs(240);

pub(crate) fn shutdown_watchdog_grace() -> Option<Duration> {
    parse_watchdog_grace(std::env::var(SHUTDOWN_WATCHDOG_ENV_VAR).ok().as_deref())
}

/// `None` means "no backstop". Only an explicit `0` may produce it — a
/// malformed override falls back to the default rather than silently
/// disabling the one thing guaranteeing the process exits.
pub(crate) fn parse_watchdog_grace(raw: Option<&str>) -> Option<Duration> {
    let Some(raw) = raw else {
        return Some(SHUTDOWN_WATCHDOG_GRACE);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => Some(SHUTDOWN_WATCHDOG_GRACE),
    }
}

/// Guarantee the process exits once teardown has started.
///
/// Every step after this point is a best-effort flush, and several are
/// unbounded: the maintenance join waits out an in-flight pass whose
/// `spawn_blocking` deletion worker cannot be cancelled, the embedded
/// zccache drain takes an untimed publication write barrier and joins its
/// index writer, and dropping the multi-thread runtime waits for every
/// outstanding blocking task. Any one of them wedging left a daemon alive
/// forever holding the pid file, version claim, and endpoint — and the CLI
/// deliberately never force-kills a daemon that acknowledged shutdown, so
/// nothing else bounded it.
///
/// A detached OS thread, NOT a tokio task: it has to fire even when the
/// runtime is stalled or its blocking pool is fully occupied, which is
/// exactly the situation it exists for.
fn arm_shutdown_watchdog() {
    let Some(grace) = shutdown_watchdog_grace() else {
        return;
    };
    std::thread::Builder::new()
        .name("soldr-shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep(grace);
            tracing::error!(
                grace_secs = grace.as_secs(),
                "graceful shutdown did not complete within the watchdog grace; \
                 exiting now. Cache state may not be fully flushed."
            );
            std::process::exit(0);
        })
        .ok();
}

/// Apply a [`Request::AttachBuildLogHistory`] merge (soldr#1814 slice 2d).
///
/// Split out of the dispatch arm so the merge semantics are testable without
/// a live socket. Mirrors what `persist_build_log_history_inner` used to do
/// CLI-side, but under the daemon's sole ownership of the table.
fn attach_build_log_history(
    db_path: &Path,
    update: &crate::daemon::protocol::BuildLogHistoryUpdate,
) -> Response {
    let mut record = match db::get_build(db_path, update.session_id) {
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
            db::aggregate_session(db_path, update.session_id).unwrap_or((0, None, None));
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

    match db::upsert_build(db_path, &record) {
        Ok(()) => Response::Ack,
        Err(err) => Response::Error(format!("write build history: {err}")),
    }
}

fn existing_daemon_pid(paths: &SoldrPaths) -> Option<u32> {
    select_existing_daemon_pid(
        crate::daemon::lifecycle::stale_daemon_occupies_endpoint(paths),
        is_live(paths),
    )
}

fn select_existing_daemon_pid(
    version_blind: Option<u32>,
    protocol_aware: Option<u32>,
) -> Option<u32> {
    version_blind.or(protocol_aware)
}

#[cfg(test)]
mod endpoint_occupancy_tests {
    use super::select_existing_daemon_pid;

    crate::timed_test!(protocol_mismatched_daemon_still_blocks_direct_startup, {
        assert_eq!(select_existing_daemon_pid(Some(41), None), Some(41));
        assert_eq!(select_existing_daemon_pid(None, Some(42)), Some(42));
        assert_eq!(select_existing_daemon_pid(None, None), None);
    });

    #[cfg(unix)]
    crate::timed_test!(claimed_unix_endpoint_cannot_be_bound_by_a_second_daemon, {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let temp = tempfile::tempdir().unwrap();
                let paths = crate::core::SoldrPaths::with_root(temp.path().join("owned"));
                std::fs::create_dir_all(crate::cache_lib::soldr_daemon_dir(&paths)).unwrap();
                let (listener, _) = super::claim_unix_endpoint(&paths).unwrap();
                let second =
                    tokio::net::UnixListener::bind(crate::cache_lib::daemon_sock_path(&paths));
                assert!(second.is_err(), "the endpoint claim must be exclusive");
                drop(listener);
            });
    });
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UnixSocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn unix_socket_identity(path: &Path) -> std::io::Result<UnixSocketIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(UnixSocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn remove_unix_socket_if_matches(
    path: &Path,
    expected: UnixSocketIdentity,
) -> std::io::Result<bool> {
    let actual = match unix_socket_identity(path) {
        Ok(actual) => actual,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if actual != expected {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

#[cfg(unix)]
fn claim_unix_endpoint(
    paths: &SoldrPaths,
) -> std::io::Result<(tokio::net::UnixListener, UnixSocketIdentity)> {
    let sock = daemon_sock_path(paths);
    match std::fs::remove_file(&sock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = tokio::net::UnixListener::bind(&sock)?;
    let identity = match unix_socket_identity(&sock) {
        Ok(identity) => identity,
        Err(error) => {
            drop(listener);
            let _ = std::fs::remove_file(&sock);
            return Err(error);
        }
    };
    Ok((listener, identity))
}

#[cfg(unix)]
async fn run_accept_loop(
    listener: tokio::net::UnixListener,
    state: Arc<State>,
) -> std::io::Result<()> {
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

#[cfg(all(test, unix))]
mod unix_endpoint_ownership_tests {
    use super::*;

    crate::timed_test!(retiring_daemon_does_not_unlink_replacement_socket, {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("soldr.sock");

        let old_listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind old socket");
        let old_identity = unix_socket_identity(&socket_path).expect("old identity");

        std::fs::remove_file(&socket_path).expect("unlink old socket name");
        let replacement_listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind replacement");
        let replacement_identity =
            unix_socket_identity(&socket_path).expect("replacement identity");
        assert_ne!(old_identity, replacement_identity);

        assert!(
            !remove_unix_socket_if_matches(&socket_path, old_identity).expect("fenced old cleanup"),
            "old daemon must not remove the replacement socket"
        );
        assert!(socket_path.exists());
        assert!(
            remove_unix_socket_if_matches(&socket_path, replacement_identity)
                .expect("replacement cleanup")
        );

        drop(replacement_listener);
        drop(old_listener);
    });
}

#[cfg(windows)]
async fn run_accept_loop(paths: SoldrPaths, state: Arc<State>) -> std::io::Result<()> {
    // soldr#1808: identity failure is fatal here — this loop *is* the
    // endpoint. Propagating beats a fallback name no client would dial.
    let pipe_name = format!(
        r"\\.\pipe\{}",
        crate::cache_lib::daemon_pipe_name(&paths).map_err(std::io::Error::other)?
    );
    let pool_size = windows_listener_pool_size();
    tracing::info!(
        pool_size,
        queue_capacity = state.compile_admission.capacity,
        expected_compile_slots = state.compile_admission.expected_compile_slots,
        "soldr-daemon Windows named-pipe listener pool ready"
    );
    for index in 0..pool_size {
        spawn_windows_pipe_instance(pipe_name.clone(), state.clone(), index == 0);
    }
    // Park until shutdown rather than forever. The pool instances are
    // detached and self-replenishing, so aborting this task cannot stop
    // them — each instance observes the same signal and drops its own pipe
    // handle. Returning here is what lets the caller's `.await` complete.
    state.shutdown.wait().await;
    Ok(())
}

#[cfg(windows)]
fn spawn_windows_pipe_instance(pipe_name: String, state: Arc<State>, first_pipe_instance: bool) {
    // Keep this launcher synchronous. Calling `tokio::spawn` directly from
    // `accept_windows_pipe_instance` would make the async function's opaque
    // future recursively depend on itself, which Windows rejects because its
    // `Send` bound cannot be inferred.
    tokio::spawn(async move {
        if let Err(error) =
            accept_windows_pipe_instance(pipe_name, state, first_pipe_instance).await
        {
            tracing::debug!(%error, "Windows named-pipe listener exited");
        }
    });
}

#[cfg(windows)]
async fn accept_windows_pipe_instance(
    pipe_name: String,
    state: Arc<State>,
    first_pipe_instance: bool,
) -> std::io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    // Never open a fresh instance once teardown has begun; that would
    // re-arm the endpoint the shutdown path is trying to retire.
    if state.shutdown.is_requested() {
        return Ok(());
    }
    let server = ServerOptions::new()
        .first_pipe_instance(first_pipe_instance)
        .create(&pipe_name)?;

    // Stop waiting for a client the moment shutdown is requested, and drop
    // `server` on the way out so the pipe instance is released.
    //
    // Without this the pool stayed live for the whole graceful drain (tens
    // of seconds). A wrapper connecting in that window reached a daemon
    // whose compile service had already latched shut, so it got back an
    // `Error` frame -> `ClientError::Protocol`, which
    // `client_error_indicates_daemon_unavailable` deliberately classifies
    // as NOT unavailable — denying the direct-rustc fallback and failing
    // the build. Unix never had this hole: aborting its accept task drops
    // the `UnixListener`, so the next connect fails with `Io` and degrades
    // cleanly. Releasing the handle here restores that behavior.
    let connected = tokio::select! {
        result = server.connect() => result.is_ok(),
        _ = state.shutdown.wait() => return Ok(()),
    };

    if state.shutdown.is_requested() {
        return Ok(());
    }
    if connected {
        // Replenish before parsing the connected request, keeping the pool
        // admission capacity independent from compile execution throughput.
        spawn_windows_pipe_instance(pipe_name, state.clone(), false);
        let _ = handle_connection(server, state).await;
    } else {
        spawn_windows_pipe_instance(pipe_name, state, false);
    }
    Ok(())
}

/// Read budget for draining a doomed connection (#1853). Short on purpose:
/// we only need to clear what the peer already queued, not wait for more.
const DRAIN_READ_TIMEOUT: Duration = Duration::from_millis(250);

/// Tell a peer whose protocol we cannot speak *why* we are hanging up, then
/// close cleanly (#1853).
///
/// The reject record is version-independent by construction (see
/// [`crate::daemon::ipc::REJECT_RECORD_VERSION`]), so even a client several
/// versions old gets a diagnosable `InvalidData` with a message instead of an
/// opaque `ECONNRESET` after burning its whole retry budget.
async fn reject_version_mismatch<S>(stream: &mut S, peer_version: u32)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;
    tracing::warn!(
        peer_version,
        daemon_version = PROTOCOL_VERSION,
        "soldr-daemon: rejecting IPC peer speaking a different protocol version",
    );
    let record = crate::daemon::ipc::encode_reject_record(&format!(
        "protocol version mismatch: peer={peer_version}, daemon={PROTOCOL_VERSION}"
    ));
    // Bounded by the drain budget, not the handshake budget: we are already
    // hanging up, so a peer that will not read must not hold a worker.
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.write_all(&record)).await;
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.flush()).await;
    drain_then_close(stream).await;
}

/// Half-close, then discard whatever the peer already queued, before dropping
/// the connection (#1853).
///
/// On an AF_UNIX SOCK_STREAM socket, closing while bytes remain in our receive
/// queue makes the kernel raise `ECONNRESET` on the peer (Linux
/// `unix_release_sock`), which a client cannot distinguish from a daemon
/// crash — so it retries for its entire budget and then hard-fails with no
/// fallback. Draining first turns that into a clean EOF, which every existing
/// client already classifies as "daemon unavailable" and degrades on. That is
/// what makes this fix reach clients that were shipped long before it.
/// Unix only, deliberately. `ECONNRESET`-on-unread-data is an AF_UNIX /
/// SOCK_STREAM behavior; Windows named pipes have no RST concept, so there is
/// nothing to prevent there — and paying a shutdown/drain round trip on every
/// rejected connection measurably slows the Windows hot path.
#[cfg(unix)]
async fn drain_then_close<S>(stream: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;
    // Half-close first so a peer still streaming a large body sees EOF and
    // stops writing, instead of us draining against a live producer.
    //
    // Every step here is bounded by DRAIN_READ_TIMEOUT, not the handshake
    // budget: this is a courtesy on a connection we have already decided to
    // drop, so it must never become a latency source. An earlier revision
    // used the 5s handshake timeout here and pushed
    // `dependency_failure_cancels_sibling_lint_children` past its 5s budget.
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.shutdown()).await;
    let cap = u64::from(crate::daemon::protocol::MAX_BODY_BYTES) + LEGACY_FRAME_HEADER_BYTES as u64;
    let mut sink = [0_u8; 8192];
    let mut drained = 0_u64;
    while drained < cap {
        match timeout(DRAIN_READ_TIMEOUT, stream.read(&mut sink)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => drained += n as u64,
        }
    }
}

/// No-op on Windows: named pipes cannot raise `ECONNRESET`, so dropping the
/// handle already yields the clean end-of-stream the Unix path has to work for.
#[cfg(windows)]
async fn drain_then_close<S>(_stream: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
            Ok(MuxPoll::Payload { .. }) | Err(_) => {
                // #1853: never drop a socket with the peer's bytes still
                // queued — see `drain_then_close`.
                drain_then_close(&mut stream).await;
                return Ok(());
            }
        }
    }

    // #1853: check the peer's version explicitly rather than letting the
    // decode fail. A version mismatch is a known, diagnosable condition, and
    // reporting it as an opaque transport reset is what made this cost a day
    // to attribute downstream.
    if buffered.len() >= LEGACY_FRAME_HEADER_BYTES {
        let peer_version = u32::from_le_bytes(
            buffered[4..LEGACY_FRAME_HEADER_BYTES]
                .try_into()
                .expect("slice of LEGACY_FRAME_HEADER_BYTES-4 bytes is always 4 bytes wide"),
        );
        if peer_version != PROTOCOL_VERSION {
            reject_version_mismatch(&mut stream, peer_version).await;
            return Ok(());
        }
    }

    let req: Request = match read_frame_async_with_prefix(&mut stream, &buffered).await {
        Ok(r) => r,
        Err(error) => {
            tracing::debug!(%error, "soldr-daemon: dropping undecodable IPC frame");
            drain_then_close(&mut stream).await;
            return Ok(());
        }
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
            let _ = write_frame_async(
                &mut stream,
                &Response::ShuttingDown(ShutdownAck {
                    pid: std::process::id(),
                    generation: state.daemon_identity.started_at_unix_ms,
                }),
            )
            .await;
            state.shutdown.request();
        }
        Request::FlushCaches => {
            // Issue #1286 (F1): checkpoint the embedded zccache state
            // (artifact index, depgraph snapshot, metadata cache) to
            // disk without shutting down. `soldr save` / `soldr cache
            // flush` call this before archiving — otherwise the state
            // is memory-only until a graceful daemon exit and archives
            // taken from a live daemon restore with zero rustc hits.
            let response = match state.event_batcher.flush().await {
                Err(err) => Response::Error(format!("event persistence flush failed: {err}")),
                Ok(()) => match state.compile_service.flush().await {
                    Ok(report) => Response::CacheFlushed(report),
                    Err(err) => Response::Error(format!("embedded zccache flush failed: {err}")),
                },
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
            // The cargo front door owns an OS-held BuildActivityLease before
            // sending this request and retains it through history
            // publication. Do not mirror that lease in the daemon: this
            // request has no durable connection, so a crashed client could
            // otherwise block maintenance for the daemon's whole lifetime.
            let response = async {
                let existing = db::get_build(&state.db_path, session_id)
                    .map_err(|err| format!("read build session: {err}"))?;
                let record =
                    merge_build_session_start(existing, session_id, repo_root, started_at_ms);
                db::upsert_build(&state.db_path, &record)
                    .map_err(|err| format!("persist build session start: {err}"))?;
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
                    .await
                    .map_err(|err| format!("queue session start event: {err}"))?;
                state
                    .event_batcher
                    .flush()
                    .await
                    .map_err(|err| format!("flush session start event: {err}"))?;
                Ok::<(), String>(())
            }
            .await;
            let response = match response {
                Ok(()) => Response::Ack,
                Err(err) => Response::Error(err),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        } => {
            let response = match finalize_build_session(
                &state.db_path,
                &state.event_batcher,
                session_id,
                exit_code,
                ended_at_ms,
            )
            .await
            {
                Ok(()) => Response::Ack,
                Err(err) => Response::Error(err),
            };
            // soldr#1536: acknowledge the finalization. When the wrapper
            // sees this Ack, the BuildRecord aggregate is persisted and
            // every staged session event is durable in redb, so it can
            // skip its own full-table aggregate re-scan.
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildLogInputs { session_id } => {
            // soldr#1814 slice 2a: the daemon owns these tables, so it answers
            // both reads in one round trip. The CLI previously opened
            // state.redb twice per build to get them.
            let response = match db::list_events_for_session(&state.db_path, session_id) {
                Ok(events) => Response::BuildLogInputs {
                    events,
                    // A missing row is normal, not an error — the log renders
                    // without it. Only a genuine read failure is reported.
                    record: db::get_build(&state.db_path, session_id)
                        .ok()
                        .flatten()
                        .map(Box::new),
                },
                Err(err) => Response::Error(format!("build log inputs: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::AttachBuildLogHistory(update) => {
            // soldr#1814 slice 2d. The whole get/mutate/upsert runs here, under
            // the daemon's own ownership of the table, so two processes cannot
            // interleave a read and a write and lose each other's fields.
            //
            // The merge deliberately reproduces the semantics the CLI-side code
            // had: `ended_at_ms` / `exit_code` keep an already-recorded value
            // rather than being overwritten (first writer wins), and the
            // crate-count aggregate is only recomputed when the client says
            // `BuildSessionEnd` did not already finalize it (soldr#1536).
            let response = attach_build_log_history(&state.db_path, &update);
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::ShouldWarnCargoDebugDefault { repo_root } => {
            // soldr#1814 slice 2c: the daemon owns state_db's tables, so it
            // performs this read-modify-write (record the repo, prune expired
            // rows) instead of every front-door invocation opening the file.
            let db_path = crate::cache_lib::state_db_path(&state.paths);
            let emit = crate::cache_lib::state_db::StateDb::open(&db_path)
                .and_then(|db| {
                    db.should_emit_cargo_debug_default_warning(std::path::Path::new(&repo_root))
                })
                // Fail open, matching the pre-#1814 caller: a state-DB problem
                // must not silently suppress a warning the user should see.
                .unwrap_or(true);
            let _ = write_frame_async(&mut stream, &Response::CargoDebugWarning { emit }).await;
        }
        Request::ListBuilds { limit, since_ms } => {
            let response = match db::list_builds(&state.db_path, limit, since_ms) {
                Ok(rows) => Response::Builds(rows),
                Err(err) => Response::Error(format!("list builds: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::ListSlowBuilds {
            threshold_ms,
            limit,
        } => {
            let response = match db::list_slow_builds(&state.db_path, threshold_ms, limit) {
                Ok(rows) => Response::Builds(rows),
                Err(err) => Response::Error(format!("list slow builds: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
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
            // Admission applies on every transport (soldr#1853). This was
            // `#[cfg(windows)]`-only, which left the AF_UNIX listener with no
            // bound at all: under `cargo -j N` it admitted every wrapper at
            // once and shed the excess by resetting sockets, which reached the
            // client as ECONNRESET and failed the build. Windows passed
            // precisely because it had this cap. The policy itself was already
            // written to be portable — see
            // `windows_burst_policy_keeps_four_pool_sizes_fifo_and_recovers`,
            // which validates it on Linux — only its application was gated.
            let _admission = match state.compile_admission.try_admit() {
                Some(permit) => permit,
                None => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Backpressure {
                            retry_after_ms: IPC_BACKPRESSURE_RETRY_AFTER_MS,
                        },
                    )
                    .await;
                    return Ok(());
                }
            };
            state
                .compile_admission
                .record_busy_retries(req.ipc_busy_retries);
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
                // soldr#1838 Phase 2: while retiring, the compile service has
                // latched shut, so every error it returns means "not serving
                // work" rather than "something is broken inside me". Saying
                // which one it is decides whether the wrapper degrades to
                // direct rustc or hard-fails the build: `Error` becomes
                // `ClientError::Protocol`, which is deliberately classified as
                // NOT daemon-unavailable so a real daemon bug is never masked.
                // Reporting a normal drain that way failed builds (#1837).
                let reply = if state.shutdown.is_requested() {
                    tracing::info!(
                        target: "soldr::daemon::compile_stream",
                        compile_id = compile_id.as_str(),
                        "compile arrived during shutdown; answering Retiring so the                          wrapper degrades to direct rustc",
                    );
                    Response::Retiring
                } else {
                    Response::Error(format!("embedded zccache compile failed: {err}"))
                };
                return write_frame_async(stream, &reply).await;
            }
        },
        DispatchOutcome::ClientDisconnected(reason) => {
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
            // soldr#1857: the trace above is inert unless
            // SOLDR_DAEMON_TRACE is set, i.e. never in a real build.
            // This one is always on, so "the wrapper vanished mid-
            // compile" is countable after the fact instead of being a
            // hypothesis nobody can test.
            record_undelivered(
                state,
                &compile_id,
                lifecycle.as_ref(),
                inner_started,
                crate::daemon::compile_delivery::UndeliveredKind::ClientDisconnected,
                &reason.detail(),
                None,
            );
            tracing::info!(
                target: "soldr::daemon::compile_stream",
                compile_id = compile_id.as_str(),
                reason = reason.detail().as_str(),
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
        if let Err(err) =
            write_frame_async(stream, &Response::CompileStdoutChunk(chunk.to_vec())).await
        {
            return Err(report_reply_write_failure(
                state,
                &compile_id,
                lifecycle.as_ref(),
                inner_started,
                "stdout_chunk",
                err,
                body.exit_code,
            ));
        }
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
        if let Err(err) =
            write_frame_async(stream, &Response::CompileStderrChunk(chunk.to_vec())).await
        {
            return Err(report_reply_write_failure(
                state,
                &compile_id,
                lifecycle.as_ref(),
                inner_started,
                "stderr_chunk",
                err,
                body.exit_code,
            ));
        }
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
    let res = write_frame_async(stream, &done).await.map_err(|err| {
        report_reply_write_failure(
            state,
            &compile_id,
            lifecycle.as_ref(),
            inner_started,
            "done",
            err,
            body.exit_code,
        )
    });
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

/// Append one durable "the daemon ran this compile and could not hand
/// it back" row (soldr#1857). Best-effort — see
/// [`crate::daemon::compile_delivery`].
fn record_undelivered(
    state: &Arc<State>,
    compile_id: &str,
    lifecycle: Option<&crate::daemon::protocol::CompileLifecycle>,
    started: std::time::Instant,
    kind: crate::daemon::compile_delivery::UndeliveredKind,
    detail: &str,
    exit_code: Option<i32>,
) {
    crate::daemon::compile_delivery::record(
        &state.paths,
        &crate::daemon::compile_delivery::Undelivered {
            kind,
            detail,
            compile_id,
            crate_name: lifecycle.map(|l| l.crate_name.as_str()),
            target_dir: lifecycle.map(|l| l.target_dir.as_str()),
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            exit_code,
        },
    );
}

/// A finished compile whose reply could not be written to the wrapper.
///
/// This is the precise shape soldr#1857 reports: zccache journals
/// `exit_code: 0`, the wrapper reports failure to cargo, and no
/// diagnostic exists anywhere because the only record of the loss was a
/// `tracing::warn!` on a detached daemon whose stderr goes nowhere.
/// Record it durably, then return the original error unchanged so the
/// connection teardown path is untouched.
fn report_reply_write_failure(
    state: &Arc<State>,
    compile_id: &str,
    lifecycle: Option<&crate::daemon::protocol::CompileLifecycle>,
    started: std::time::Instant,
    stage: &str,
    err: std::io::Error,
    exit_code: i32,
) -> std::io::Error {
    let detail = format!("{stage}:{}", io_error_kind_name(&err));
    record_undelivered(
        state,
        compile_id,
        lifecycle,
        started,
        crate::daemon::compile_delivery::UndeliveredKind::ReplyWriteFailed,
        &detail,
        Some(exit_code),
    );
    tracing::warn!(
        target: "soldr::daemon::compile_stream",
        compile_id,
        stage,
        exit_code,
        "compile finished but its reply could not be delivered to the wrapper",
    );
    err
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
        let _ = event_batcher
            .record(compile_lifecycle_event(metadata, None))
            .await;
    }
    let outcome = race_against_disconnect(reader, fut).await;
    if let (DispatchOutcome::Completed(_), Some(metadata)) = (&outcome, lifecycle) {
        let duration_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let _ = event_batcher
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
    ClientDisconnected(DisconnectReason),
}

/// Which of the three disconnect signals fired, kept so the durable
/// [`crate::daemon::compile_delivery`] row says *how* the wrapper went
/// away rather than only that it did (soldr#1857). A clean `eof` is a
/// wrapper that exited or was killed; a `read_error` is the OS tearing
/// the pipe down underneath it; `unexpected_bytes` is a protocol
/// violation and means something quite different from either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisconnectReason {
    /// `read` returned `Ok(0)` — the client closed its end.
    Eof,
    /// `read` failed. Carries the `io::ErrorKind` name.
    ReadError(&'static str),
    /// The client sent bytes mid-compile, which the request-response
    /// protocol forbids. Carries how many arrived.
    UnexpectedBytes(usize),
}

impl DisconnectReason {
    /// Stable, low-cardinality string for the JSONL `detail` field.
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Eof => "eof".to_string(),
            Self::ReadError(kind) => format!("read_error:{kind}"),
            Self::UnexpectedBytes(n) => format!("unexpected_bytes:{n}"),
        }
    }
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
///
/// **But bias must not destroy work that is already done** (soldr#1857).
/// `biased` polls the reader first on *every* tick, so a compile future
/// that became ready in the same tick as the disconnect signal was
/// discarded — the compile had run to completion, zccache had journaled
/// `exit_code: 0`, and the daemon threw the result away. The wrapper
/// then saw an unexplained failure for a compile that had succeeded,
/// which is exactly the shape #1857 reports. So when the reader branch
/// wins, give `fut` one final poll: if it is already `Ready`, that
/// result is real and gets shipped (writing into a half-closed pipe
/// merely errors, which the caller already handles). Only a genuinely
/// still-pending compile is cancelled.
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
            // three mean "the wrapper is gone or wedged".
            let reason = match read {
                Ok(0) => DisconnectReason::Eof,
                Ok(n) => DisconnectReason::UnexpectedBytes(n),
                Err(err) => DisconnectReason::ReadError(io_error_kind_name(&err)),
            };
            // Last-poll-wins: never cancel a compile that already finished.
            match poll_once(fut.as_mut()) {
                std::task::Poll::Ready(out) => DispatchOutcome::Completed(out),
                std::task::Poll::Pending => DispatchOutcome::ClientDisconnected(reason),
            }
        }
        out = &mut fut => DispatchOutcome::Completed(out),
    }
}

/// Poll `fut` exactly once, returning immediately either way. Used to
/// harvest a compile that completed in the same tick the disconnect
/// signal arrived.
fn poll_once<F: std::future::Future>(mut fut: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    fut.as_mut().poll(&mut cx)
}

/// Stable `'static` name for an `io::ErrorKind`, for the JSONL `detail`
/// field. `Debug` on `ErrorKind` is already stable enough in practice,
/// but this keeps the set explicit and allocation-free.
fn io_error_kind_name(err: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::BrokenPipe => "BrokenPipe",
        ErrorKind::ConnectionReset => "ConnectionReset",
        ErrorKind::ConnectionAborted => "ConnectionAborted",
        ErrorKind::NotConnected => "NotConnected",
        ErrorKind::UnexpectedEof => "UnexpectedEof",
        ErrorKind::TimedOut => "TimedOut",
        ErrorKind::WouldBlock => "WouldBlock",
        ErrorKind::Interrupted => "Interrupted",
        _ => "Other",
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
            // Tag the exit reason BEFORE notifying so the main task's
            // post-shutdown lifecycle JSONL emit picks `died-idle`.
            state.exit_via_idle.store(true, Ordering::Relaxed);
            state.shutdown.request();
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
        // soldr#1808: infallible by signature and called from ~34 sites, so
        // threading `Result` through it is its own change. Failing loudly is
        // still correct: the process cannot serve or dial an endpoint whose
        // identity it cannot establish, and the alternative this replaced --
        // a shared `"soldr"` literal -- silently put every user on one pipe.
        PathBuf::from(format!(
            r"\\.\pipe\{}",
            crate::cache_lib::daemon_pipe_name(paths).unwrap_or_else(|err| panic!("{err}"))
        ))
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
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
            assert!(matches!(outcome, DispatchOutcome::ClientDisconnected(_)));
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
                    matches!(outcome, DispatchOutcome::ClientDisconnected(_)),
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
                    DispatchOutcome::ClientDisconnected(reason) => {
                        panic!("unexpected disconnect ({reason:?}) — client end was held open");
                    }
                }
                assert!(
                    !aborted.load(Ordering::SeqCst),
                    "inner future was cancelled despite running to completion"
                );
            });
        }
    );

    // soldr#1857 regression: the `biased` select polls the reader on
    // every tick, so a compile that finished in the SAME tick as the
    // disconnect signal used to be thrown away — zccache had journaled
    // `exit_code: 0` and the wrapper still reported failure to cargo
    // with nothing to show for it. Here the client end is already gone
    // (EOF is immediately ready) and the compile is already complete;
    // the finished result must win.
    timed_test!(
        completed_compile_is_not_discarded_by_a_simultaneous_disconnect,
        Duration::from_secs(10),
        {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let (server_side, client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);
                // Client is gone before the race even starts: the read
                // probe resolves to EOF on its very first poll.
                drop(client_side);

                let outcome = race_against_disconnect(&mut server_reader, async { 4_2_u32 }).await;

                match outcome {
                    DispatchOutcome::Completed(value) => assert_eq!(value, 42),
                    DispatchOutcome::ClientDisconnected(reason) => panic!(
                        "a compile that had already completed was discarded as \
                         {reason:?}. That is soldr#1857: the daemon runs the \
                         compile, journals exit 0, throws the result away, and \
                         cargo reports an unexplained failure. The disconnect \
                         branch must poll the future once before giving up."
                    ),
                }
            });
        }
    );

    // The durable JSONL row says *how* the wrapper went away; these are
    // the three signals that map onto it.
    timed_test!(disconnect_reason_details_are_stable_and_distinct, {
        use super::DisconnectReason;
        assert_eq!(DisconnectReason::Eof.detail(), "eof");
        assert_eq!(
            DisconnectReason::ReadError("BrokenPipe").detail(),
            "read_error:BrokenPipe"
        );
        assert_eq!(
            DisconnectReason::UnexpectedBytes(4).detail(),
            "unexpected_bytes:4"
        );
    });

    // A wrapper that violates the request-response contract by sending
    // bytes mid-compile is a different fault from one that died, and
    // the record has to be able to tell them apart.
    timed_test!(
        stray_bytes_mid_compile_are_recorded_as_a_protocol_violation,
        Duration::from_secs(10),
        {
            use tokio::io::AsyncWriteExt;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt");
            rt.block_on(async {
                let (server_side, mut client_side) = tokio::io::duplex(64);
                let (mut server_reader, _server_writer) = tokio::io::split(server_side);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let _ = client_side.write_all(b"x").await;
                    // Hold the connection open so this is unambiguously
                    // "stray bytes", not "EOF".
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                let outcome = race_against_disconnect(&mut server_reader, async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                })
                .await;
                match outcome {
                    DispatchOutcome::ClientDisconnected(reason) => {
                        assert_eq!(reason.detail(), "unexpected_bytes:1");
                    }
                    DispatchOutcome::Completed(_) => {
                        panic!("the 1h sleep cannot have completed");
                    }
                }
            });
        }
    );
}
