//! Daemon-owned, exact-root cache maintenance (#1762–#1764).

use crate::cache_lib::gc::{self, GcOptions};
use crate::cache_lib::target_registry::TargetRegistry;
use crate::core::SoldrPaths;
use crate::daemon::{db, history_gc};
use crate::zccache_embedded::{
    EmbeddedDiskMaintenanceReport, EmbeddedDiskPolicy, SoldrZccacheService,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub const PRESSURE_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const FULL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const EVENT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PRESSURE_STALE_AGE: Duration = Duration::from_secs(4 * 24 * 60 * 60);
const FULL_STALE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const STATUS_SCHEMA_VERSION: u32 = 2;
const FULL_MARKER: &str = "last-full-v1";
const FULL_ATTEMPT_MARKER: &str = "last-full-attempt-v1";

#[derive(Clone)]
pub struct MaintenanceContext {
    pub paths: SoldrPaths,
    pub db_path: PathBuf,
    pub compile_service: Arc<SoldrZccacheService>,
    pub shutdown: Arc<ShutdownSignal>,
}

#[derive(Default)]
pub struct ShutdownSignal {
    requested: AtomicBool,
    notify: Notify,
}

impl ShutdownSignal {
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub async fn wait(&self) {
        loop {
            // Register with the Notify BEFORE re-checking the flag.
            //
            // `notify_waiters()` stores no permit: a `Notified` future
            // snapshots the waiter generation when it is *enabled*, so the
            // naive `while !is_requested() { notified().await }` loses the
            // wakeup for this interleaving and parks forever —
            //
            //   waiter:    is_requested() -> false
            //   requester: store(true); notify_waiters()
            //   waiter:    notified().await   <- missed it, never re-checks
            //
            // Enabling first means a `request()` landing anywhere after this
            // point either sets the flag we are about to read, or wakes the
            // future we already registered.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceKind {
    Pressure,
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentOutcome {
    pub items_removed: u64,
    pub bytes_reclaimed: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceStatus {
    pub schema_version: u32,
    pub owning_root: String,
    pub daemon_identity: String,
    pub embedded_cache_root: String,
    pub disk_policy: EmbeddedDiskPolicy,
    pub attempted_at_ms: i64,
    pub successful_at_ms: Option<i64>,
    pub last_full_at_ms: Option<i64>,
    pub kind: MaintenanceKind,
    pub deferred_reason: Option<String>,
    pub filesystem_capacity_bytes: Option<u64>,
    pub filesystem_free_bytes: Option<u64>,
    pub filesystem_free_percent: Option<f64>,
    pub zccache: Option<EmbeddedDiskMaintenanceReport>,
    pub zccache_error: Option<String>,
    pub cook: ComponentOutcome,
    pub history: ComponentOutcome,
    pub pep517_targets: ComponentOutcome,
    pub pep517_wheels: ComponentOutcome,
    pub trash: ComponentOutcome,
    pub workspace_targets: ComponentOutcome,
    pub daemon_events: ComponentOutcome,
    pub legacy_zccache: ComponentOutcome,
}

impl MaintenanceStatus {
    fn new(context: &MaintenanceContext, kind: MaintenanceKind, now: SystemTime) -> Self {
        let (capacity, free) = filesystem_space(&context.paths.root);
        let free_percent = capacity
            .zip(free)
            .filter(|(capacity, _)| *capacity > 0)
            .map(|(capacity, free)| free as f64 * 100.0 / capacity as f64);
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            owning_root: context.paths.root.display().to_string(),
            daemon_identity: format!(
                "{}:{}",
                context.compile_service.identity().product,
                context.compile_service.identity().instance_id
            ),
            embedded_cache_root: context.compile_service.cache_root().display().to_string(),
            disk_policy: context.compile_service.disk_policy().clone(),
            attempted_at_ms: unix_millis(now),
            successful_at_ms: None,
            last_full_at_ms: read_last_full(&context.paths).map(unix_millis),
            kind,
            deferred_reason: None,
            filesystem_capacity_bytes: capacity,
            filesystem_free_bytes: free,
            filesystem_free_percent: free_percent,
            zccache: None,
            zccache_error: None,
            cook: ComponentOutcome::default(),
            history: ComponentOutcome::default(),
            pep517_targets: ComponentOutcome::default(),
            pep517_wheels: ComponentOutcome::default(),
            trash: ComponentOutcome::default(),
            workspace_targets: ComponentOutcome::default(),
            daemon_events: ComponentOutcome::default(),
            legacy_zccache: ComponentOutcome::default(),
        }
    }
}

pub fn status_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .cache
        .join("soldr-daemon")
        .join("maintenance-status-v1.json")
}

pub fn read_status(paths: &SoldrPaths) -> Option<MaintenanceStatus> {
    let body = std::fs::read_to_string(status_path(paths)).ok()?;
    serde_json::from_str(&body).ok()
}

pub async fn run_loop(context: MaintenanceContext) {
    let paths = context.paths.clone();
    let shutdown = Arc::clone(&context.shutdown);
    run_loop_inner(&paths, shutdown, PRESSURE_INTERVAL, |kind, now| {
        let context = &context;
        async move {
            let outcome = run_once_with_lease_state(context, kind, now).await;
            let status = outcome.status;
            let succeeded = status.successful_at_ms.is_some();
            if succeeded && kind == MaintenanceKind::Full {
                let _ = record_last_full(&context.paths, now);
            }
            let _ = write_status(&context.paths, &status);
            outcome.lease_acquired
        }
    })
    .await;
}

async fn run_loop_inner<F, Fut>(
    paths: &SoldrPaths,
    shutdown: Arc<ShutdownSignal>,
    interval_duration: Duration,
    mut run_pass: F,
) where
    F: FnMut(MaintenanceKind, SystemTime) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut last_pressure = None;
    // A persistent component failure remains visible in status, but must not
    // promote every five-minute pressure tick into another expensive full
    // scan. Successful completion has its own marker; this attempt marker is
    // only scheduling backoff.
    let mut last_full_attempt = read_last_full_attempt(paths).or_else(|| read_last_full(paths));
    // The first tick is immediate.  An absent/overdue full marker yields an
    // immediate catch-up; otherwise startup still receives a pressure pass.
    let mut interval = tokio::time::interval(interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // soldr#1987: a daemon whose own executable was deleted keeps the
    // root-ownership lock forever, and nothing external can clear it --
    // `soldr daemon stop` probes the pipe while the orphan holds the
    // filesystem lock. Such a daemon can never legitimately own the root
    // again, so it stands down itself.
    let mut missing_image = crate::daemon::lifecycle::MissingImageDetector::default();
    loop {
        tokio::select! {
            _ = shutdown.wait() => break,
            _ = interval.tick() => {}
        }
        if shutdown.is_requested() {
            break;
        }
        if missing_image.observe(crate::daemon::lifecycle::daemon_image_present()) {
            // Loud on purpose: this is the only notice that a daemon
            // disappeared on its own, and a silent exit would look like the
            // crash it is not.
            tracing::warn!(
                event = "daemon_image_deleted",
                strikes = missing_image.strikes(),
                "this daemon's executable no longer exists; releasing the soldr                  root and shutting down so a new daemon can start (soldr#1987)"
            );
            eprintln!(
                "soldr-daemon: own executable no longer exists after {} checks;                  shutting down to release the soldr root (soldr#1987)",
                missing_image.strikes()
            );
            shutdown.request();
            break;
        }
        let now = SystemTime::now();
        let kind =
            due_kind(last_full_attempt, last_pressure, now).unwrap_or(MaintenanceKind::Pressure);
        let pass_started = run_pass(kind, now).await;
        if kind == MaintenanceKind::Full && pass_started {
            last_full_attempt = Some(now);
            let _ = record_last_full_attempt(paths, now);
        }
        last_pressure = Some(now);
    }
}

pub async fn run_once(
    context: &MaintenanceContext,
    kind: MaintenanceKind,
    now: SystemTime,
) -> MaintenanceStatus {
    run_once_with_lease_state(context, kind, now).await.status
}

struct RunOnceOutcome {
    status: MaintenanceStatus,
    lease_acquired: bool,
}

async fn run_once_with_lease_state(
    context: &MaintenanceContext,
    kind: MaintenanceKind,
    now: SystemTime,
) -> RunOnceOutcome {
    let mut status = MaintenanceStatus::new(context, kind, now);
    let _maintenance_lease =
        match crate::cache_lib::build_active::MaintenanceLease::try_acquire(&context.paths) {
            Ok(None) => {
                status.deferred_reason = Some("build_active".to_string());
                return RunOnceOutcome {
                    status,
                    lease_acquired: false,
                };
            }
            Err(error) => {
                status.deferred_reason = Some(format!("build_lease_probe_failed: {error}"));
                return RunOnceOutcome {
                    status,
                    lease_acquired: false,
                };
            }
            Ok(Some(lease)) => lease,
        };

    match context
        .compile_service
        .maintain_disk(kind == MaintenanceKind::Full)
        .await
    {
        Ok(report) => status.zccache = Some(report),
        Err(error) => status.zccache_error = Some(error.to_string()),
    }
    let zccache_pressure = status
        .zccache
        .as_ref()
        .is_some_and(|report| report.pressure != "none");
    let local_pressure =
        root_below_pressure_threshold(&context.paths, status.filesystem_free_bytes);
    let do_pressure_cleanup = kind == MaintenanceKind::Full || zccache_pressure || local_pressure;

    let paths = context.paths.clone();
    let db_path = context.db_path.clone();
    let local = tokio::task::spawn_blocking(move || {
        run_local_components(&paths, &db_path, kind, now, do_pressure_cleanup)
    })
    .await;
    match local {
        Ok(local) => {
            status.cook = local.cook;
            status.history = local.history;
            status.pep517_targets = local.pep517_targets;
            status.pep517_wheels = local.pep517_wheels;
            status.trash = local.trash;
            status.workspace_targets = local.workspace_targets;
            status.daemon_events = local.daemon_events;
            status.legacy_zccache = local.legacy_zccache;
            let components_ok = status.zccache_error.is_none()
                && [
                    &status.cook,
                    &status.history,
                    &status.pep517_targets,
                    &status.pep517_wheels,
                    &status.trash,
                    &status.workspace_targets,
                    &status.daemon_events,
                    &status.legacy_zccache,
                ]
                .iter()
                .all(|component| component.error.is_none());
            if components_ok {
                status.successful_at_ms = Some(unix_millis(now));
                if kind == MaintenanceKind::Full {
                    status.last_full_at_ms = Some(unix_millis(now));
                }
            } else {
                status.deferred_reason = Some("one_or_more_components_failed".to_string());
            }
        }
        Err(error) => {
            status.deferred_reason = Some(format!("maintenance_worker_failed: {error}"));
        }
    }
    RunOnceOutcome {
        status,
        lease_acquired: true,
    }
}

/// Run a full pass for one explicitly supplied orphaned root.  This is the
/// only manual cross-invocation surface: it does not discover home-directory
/// siblings and refuses a root whose daemon is live.
pub async fn run_manual_root(root: PathBuf) -> Result<MaintenanceStatus, String> {
    if !root.is_absolute() {
        return Err("manual maintenance --root must be an absolute path".to_string());
    }
    let metadata = std::fs::symlink_metadata(&root)
        .map_err(|error| format!("manual maintenance root is unavailable: {error}"))?;
    if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
        return Err("manual maintenance --root must name a real directory, not a link".to_string());
    }
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("canonicalize manual maintenance root: {error}"))?;
    let paths = SoldrPaths::with_root(root);
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &paths.root)
        .map_err(|error| format!("unsafe manual maintenance root: {error}"))?;
    let _root_owner = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
        .map_err(|error| format!("acquire root ownership: {error}"))?
        .ok_or_else(|| "refusing orphan-root maintenance: root ownership is busy".to_string())?;
    if let Some(pid) = crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths) {
        return Err(format!(
            "refusing orphan-root maintenance: daemon pid {pid} owns {}",
            paths.root.display()
        ));
    }
    std::fs::create_dir_all(&paths.cache)
        .map_err(|error| format!("create maintenance cache root: {error}"))?;
    let db_path = crate::cache_lib::data_db_path(&paths);
    TargetRegistry::open(&db_path)
        .map_err(|error| format!("open maintenance state database: {error}"))?;
    let identity = crate::daemon::backend_handle_adoption::current_daemon_process(&paths, None)
        .map_err(|error| format!("resolve manual maintenance identity: {error}"))?;
    let service = Arc::new(
        SoldrZccacheService::start(&paths, &identity)
            .await
            .map_err(|error| error.to_string())?,
    );
    let context = MaintenanceContext {
        paths: paths.clone(),
        db_path,
        compile_service: Arc::clone(&service),
        shutdown: Arc::new(ShutdownSignal::default()),
    };
    let now = SystemTime::now();
    let status = run_once(&context, MaintenanceKind::Full, now).await;
    drop(context);
    if status.successful_at_ms.is_some() {
        record_last_full(&paths, now).map_err(|error| error.to_string())?;
    }
    write_status(&paths, &status).map_err(|error| error.to_string())?;
    if let Ok(service) = Arc::try_unwrap(service) {
        service
            .shutdown(zccache::embedded::ShutdownMode::Graceful)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(status)
}

#[derive(Default)]
struct LocalOutcomes {
    cook: ComponentOutcome,
    history: ComponentOutcome,
    pep517_targets: ComponentOutcome,
    pep517_wheels: ComponentOutcome,
    trash: ComponentOutcome,
    workspace_targets: ComponentOutcome,
    daemon_events: ComponentOutcome,
    legacy_zccache: ComponentOutcome,
}

fn run_local_components(
    paths: &SoldrPaths,
    db_path: &Path,
    kind: MaintenanceKind,
    now: SystemTime,
    _pressure: bool,
) -> LocalOutcomes {
    let mut out = LocalOutcomes::default();
    let config = paths.load_config();
    let policy_context = crate::cache_lib::gc_policy::GcContext {
        driver: crate::cache_lib::gc_policy::Driver::Daemon,
        tick: if kind == MaintenanceKind::Full {
            crate::cache_lib::gc_policy::TickKind::Full
        } else {
            crate::cache_lib::gc_policy::TickKind::Pressure
        },
        free_by_volume: Vec::new(),
        config: config.as_ref().cloned().unwrap_or_default(),
        daemon_events_available: true,
        daemon_live: true,
    };
    let policy_actions = crate::cache_lib::gc_policy::plan(
        &crate::cache_lib::gc_policy::registry(),
        &policy_context,
    );
    let has_action = |id: &str| policy_actions.iter().any(|action| action.category_id == id);
    match &config {
        Ok(config) if has_action("cook") => {
            let cook = crate::cache_lib::cook_gc::cook_evict_pass_with_absolute_age(
                paths,
                &config.cook,
                policy_actions
                    .iter()
                    .find(|action| action.category_id == "cook")
                    .and_then(|action| action.older_than),
            );
            out.cook = ComponentOutcome {
                items_removed: (cook.time_evicted + cook.size_evicted + cook.quarantine_evicted)
                    as u64,
                bytes_reclaimed: cook.bytes_freed,
                error: (cook.errors > 0).then(|| format!("{} cook eviction errors", cook.errors)),
            };
        }
        Ok(_) => {}
        Err(error) => {
            let message = Some(format!("invalid_config: {error}"));
            out.cook.error = message.clone();
            out.workspace_targets.error = message;
        }
    }

    let history = history_gc::sweep(
        paths,
        db_path,
        &history_gc::HistoryGcOptions {
            now,
            max_age: history_gc::DEFAULT_MAX_AGE,
            max_bytes: history_gc::DEFAULT_MAX_BYTES,
            migrate_pre_redaction: true,
        },
    );
    out.history = ComponentOutcome {
        items_removed: (history.age_removed
            + history.size_removed
            + history.migration_removed
            + history.legacy_files_removed) as u64,
        bytes_reclaimed: history.bytes_reclaimed,
        error: (history.failed > 0).then(|| format!("{} history deletion errors", history.failed)),
    };

    if has_action("pep517_targets") {
        let max_age = policy_actions
            .iter()
            .find(|action| action.category_id == "pep517_targets")
            .and_then(|action| action.older_than)
            .unwrap_or(crate::cache_lib::pep517_gc::PRESSURE_MAX_AGE);
        let pep = crate::cache_lib::pep517_gc::sweep(paths, now, max_age);
        out.pep517_targets = ComponentOutcome {
            items_removed: pep.removed as u64,
            bytes_reclaimed: pep.bytes_reclaimed,
            error: (pep.failed > 0).then(|| format!("{} PEP517 target errors", pep.failed)),
        };
        let wheels = crate::cache_lib::pep517_gc::sweep_wheels(paths, now, max_age);
        out.pep517_wheels = ComponentOutcome {
            items_removed: wheels.removed as u64,
            bytes_reclaimed: wheels.bytes_reclaimed,
            error: (wheels.failed > 0).then(|| format!("{} PEP517 wheel errors", wheels.failed)),
        };
    }

    if has_action("trash") {
        match crate::cache_lib::trash_gc::sweep_trash(paths) {
            Ok(trash) => {
                out.trash.items_removed = trash.removed;
                out.trash.error = (trash.retained > 0)
                    .then(|| format!("{} trash entries retained", trash.retained));
            }
            Err(error) => out.trash.error = Some(error.to_string()),
        }
    }

    if let Ok(config) = &config {
        if has_action("workspace_targets") {
            out.workspace_targets = sweep_workspace_targets(paths, config, kind);
        }
    }
    if has_action("daemon_events") {
        let cutoff = unix_millis(now).saturating_sub(EVENT_RETENTION.as_millis() as i64);
        match db::prune_events_older_than(db_path, cutoff) {
            Ok(removed) => out.daemon_events.items_removed = removed,
            Err(error) => out.daemon_events.error = Some(error.to_string()),
        }
    }
    let legacy_age = if kind == MaintenanceKind::Full {
        FULL_STALE_AGE
    } else {
        PRESSURE_STALE_AGE
    };
    if has_action("legacy_zccache") {
        let legacy_age = policy_actions
            .iter()
            .find(|action| action.category_id == "legacy_zccache")
            .and_then(|action| action.older_than)
            .unwrap_or(legacy_age);
        let legacy = crate::zccache_embedded::sweep_legacy_cache_roots(paths, now, legacy_age);
        out.legacy_zccache = ComponentOutcome {
            items_removed: legacy.removed as u64,
            bytes_reclaimed: legacy.bytes_reclaimed,
            error: (legacy.failed > 0).then(|| format!("{} legacy roots retained", legacy.failed)),
        };
    }
    out
}

/// The daemon's periodic `target/` eviction pass.
///
/// **Never holds the `state.redb` handle across filesystem work**
/// (soldr#2224). `TargetRegistry::open` takes redb's exclusive whole-file
/// lock *and* the process-wide `state_db_open_lock` for the handle's whole
/// lifetime (#608), and this sweep's middle phase — directory sizing plus
/// recursive `remove_dir_all` of every candidate — is unbounded in
/// wall-clock. Holding the handle across it locked out the `soldr cargo`
/// front door, the per-compile rustc wrapper, and the reporting CLI for
/// however long the deletion took, which is how a background build's
/// maintenance tick produced `Database already open. Cannot acquire lock.`
/// in a concurrent foreground build (soldr#2223).
///
/// The CLI-side GC learned this in #1681; the phases here mirror it:
/// snapshot-and-release → scan/delete with no handle → bounded reopen to
/// record the outcomes.
pub(crate) fn sweep_workspace_targets(
    paths: &SoldrPaths,
    config: &crate::core::SoldrConfig,
    kind: MaintenanceKind,
) -> ComponentOutcome {
    let db_path = crate::cache_lib::data_db_path(paths);
    let options = GcOptions {
        older_than_seconds: if kind == MaintenanceKind::Full {
            FULL_STALE_AGE.as_secs()
        } else {
            PRESSURE_STALE_AGE.as_secs()
        }
        .max(config.auto_gc.min_age_secs),
        larger_than_bytes: 0,
        dev_roots: configured_gc_roots(config),
        dry_run: false,
    };
    // Phase 1 — open, snapshot the rows, drop the handle before returning.
    let report = match gc::scan_released(&db_path, &options) {
        Ok(report) => report,
        Err(error) => {
            return ComponentOutcome {
                error: Some(error.to_string()),
                ..ComponentOutcome::default()
            };
        }
    };
    // Phase 2 — sizing and recursive deletion, with no database handle
    // held. The long, unbounded part of the sweep lives entirely here.
    fs_phase_barrier();
    let outcomes: Vec<_> = report
        .candidates
        .into_iter()
        .map(gc::delete_candidate_dir)
        .collect();
    // Phase 3 — bounded reopen: every directory is already gone, so this
    // is one write txn per deleted row and nothing else.
    let registry = match TargetRegistry::open(&db_path) {
        Ok(registry) => registry,
        Err(error) => {
            return ComponentOutcome {
                error: Some(error.to_string()),
                ..ComponentOutcome::default()
            };
        }
    };
    let applied = gc::apply_purge_outcomes(&registry, outcomes);
    drop(registry);
    match applied {
        Ok(report) => ComponentOutcome {
            items_removed: report.succeeded_count as u64,
            bytes_reclaimed: report.reclaimed_bytes,
            error: (report.failed_count > 0)
                .then(|| format!("{} workspace target deletion errors", report.failed_count)),
        },
        Err(error) => ComponentOutcome {
            error: Some(error.to_string()),
            ..ComponentOutcome::default()
        },
    }
}

/// Test seam marking the start of the handle-free filesystem phase.
///
/// A real sweep's phase 2 is slow because it is deleting gigabytes; a test
/// cannot wait for that and cannot make it deterministic. Instead the test
/// binary parks here until the sibling process has finished proving the
/// state DB is reachable, which asserts the property directly ("no handle
/// is held during phase 2") instead of racing it.
///
/// Compiled only into the test binary — the shipping daemon calls the
/// no-op below.
#[cfg(test)]
fn fs_phase_barrier() {
    let Some(dir) = std::env::var_os(tests::FS_BARRIER_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let _ = std::fs::write(dir.join("sweeping"), b"");
    let release = dir.join("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !release.exists() {
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(not(test))]
fn fs_phase_barrier() {}

fn configured_gc_roots(config: &crate::core::SoldrConfig) -> Vec<PathBuf> {
    let configured = config
        .gc
        .allowlist_roots
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|root| !root.trim().is_empty())
        .map(|root| crate::core::expand_user_home(&root))
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        return configured;
    }
    crate::core::user_home_dir()
        .map(|home| vec![home.join("dev")])
        .unwrap_or_default()
}

fn root_below_pressure_threshold(paths: &SoldrPaths, free_bytes: Option<u64>) -> bool {
    let Ok(config) = paths.load_config() else {
        return false;
    };
    config.auto_gc.enabled
        && free_bytes.is_some_and(|free| {
            free < config
                .auto_gc
                .trigger_free_gb
                .saturating_mul(crate::cache_lib::auto_gc::GIB)
        })
}

pub fn due_kind(
    last_full: Option<SystemTime>,
    last_pressure: Option<SystemTime>,
    now: SystemTime,
) -> Option<MaintenanceKind> {
    let full_due = last_full
        .is_none_or(|last| now.duration_since(last).unwrap_or(FULL_INTERVAL) >= FULL_INTERVAL);
    if full_due {
        return Some(MaintenanceKind::Full);
    }
    let pressure_due = last_pressure.is_none_or(|last| {
        now.duration_since(last).unwrap_or(PRESSURE_INTERVAL) >= PRESSURE_INTERVAL
    });
    pressure_due.then_some(MaintenanceKind::Pressure)
}

fn maintenance_dir(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("soldr-daemon").join("maintenance")
}

fn full_marker_path(paths: &SoldrPaths) -> PathBuf {
    maintenance_dir(paths).join(FULL_MARKER)
}

fn full_attempt_marker_path(paths: &SoldrPaths) -> PathBuf {
    maintenance_dir(paths).join(FULL_ATTEMPT_MARKER)
}

fn read_last_full(paths: &SoldrPaths) -> Option<SystemTime> {
    let value = std::fs::read_to_string(full_marker_path(paths)).ok()?;
    let millis = value.trim().parse::<u64>().ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn read_last_full_attempt(paths: &SoldrPaths) -> Option<SystemTime> {
    let value = std::fs::read_to_string(full_attempt_marker_path(paths)).ok()?;
    let millis = value.trim().parse::<u64>().ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn record_last_full(paths: &SoldrPaths, now: SystemTime) -> std::io::Result<()> {
    let dir = maintenance_dir(paths);
    std::fs::create_dir_all(&dir)?;
    atomic_write(
        &full_marker_path(paths),
        format!("{}\n", unix_millis(now)).as_bytes(),
    )
}

fn record_last_full_attempt(paths: &SoldrPaths, now: SystemTime) -> std::io::Result<()> {
    let dir = maintenance_dir(paths);
    std::fs::create_dir_all(&dir)?;
    atomic_write(
        &full_attempt_marker_path(paths),
        format!("{}\n", unix_millis(now)).as_bytes(),
    )
}

fn write_status(paths: &SoldrPaths, status: &MaintenanceStatus) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(status).map_err(std::io::Error::other)?;
    let path = status_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &body)
}

fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, body)?;
    if let Err(error) = std::fs::rename(&temp, path) {
        if path.exists() {
            std::fs::remove_file(path)?;
            std::fs::rename(&temp, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

fn filesystem_space(path: &Path) -> (Option<u64>, Option<u64>) {
    let probe = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or(path);
    (
        fs2::total_space(probe).ok(),
        fs2::available_space(probe).ok(),
    )
}

fn unix_millis(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
