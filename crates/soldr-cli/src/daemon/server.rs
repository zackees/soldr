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
    BuildRecord, CookStats, Request, Response, StatusInfo, ZccacheDaemonLink,
    COMPILE_BACKEND_EMBEDDED, PROTOCOL_VERSION,
};
use crate::daemon::zccache_link;
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
    /// Linked zccache runtime kept in memory for fast `Status` replies;
    /// also persisted to redb so a daemon restart can resume shutdown.
    linked_zccache: std::sync::Mutex<Option<ZccacheDaemonLink>>,
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
    /// per-compile redb event writes. Hot path (`Request::RecordCompile`)
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
            linked_zccache: self
                .linked_zccache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
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

/// Start the embedded zccache compile service at daemon boot. Issue
/// #977 / #980 L1 — embedded is mandatory; on failure the daemon
/// itself refuses to start so callers see a hard error instead of a
/// silent degrade. No env-var gating, no feature gating, no fallback.
async fn start_compile_service(
    paths: &SoldrPaths,
) -> Result<Arc<SoldrZccacheService>, ServerError> {
    match SoldrZccacheService::start(paths).await {
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
pub fn run(opts: ServerOptions) -> Result<(), ServerError> {
    // L6 (soldr#980): cap the daemon Tokio runtime to a small fixed
    // worker count so it doesn't oversubscribe the CPU during cargo
    // builds. The daemon handles IPC + bookkeeping; rustc is the CPU
    // bottleneck. Off-CPU profiling showed `tokio-rt-worker` context
    // switches were ~3x higher cold vs bare and `sched_yield` events
    // tripled — capping at 2-4 workers cuts the unused-worker noise.
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let workers = available.saturating_div(2).clamp(2, 4);
    tracing::info!("soldr-daemon Tokio runtime: {workers} workers (host parallelism: {available})");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    runtime.block_on(run_async(opts))
}

async fn run_async(opts: ServerOptions) -> Result<(), ServerError> {
    let paths = SoldrPaths::new()?;
    std::fs::create_dir_all(soldr_daemon_dir(&paths))?;

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
    // Resume any linked zccache identity persisted across daemon restarts.
    let resumed_link = db::get_linked_zccache(&db_path).ok().flatten();
    let start_instant = Instant::now();
    let idle_timeout_secs = u32::try_from(opts.idle_timeout.as_secs()).ok();
    let daemon_identity = current_daemon_process(&paths, idle_timeout_secs)
        .map_err(|err| ServerError::Io(std::io::Error::other(err.to_string())))?;

    // Issue #977 / #980 L1 — start the embedded zccache compile
    // service here so the daemon's tokio runtime owns its background
    // tasks. tokio-console sees the union of soldr + zccache work
    // from a single attach. Embedded is mandatory; if start fails,
    // the daemon refuses to come up.
    let compile_service = start_compile_service(&paths).await?;

    // L4 (issue soldr#980): start the background event-flusher BEFORE we
    // accept any IPC traffic so the very first `RecordCompile` lands on
    // a live channel. The drain task lives in the daemon's tokio runtime
    // and exits cleanly via `shutdown()` below.
    let event_batcher = EventBatcher::start(db_path.clone());

    let state = Arc::new(State {
        db_path,
        paths: paths.clone(),
        daemon_identity,
        linked_zccache: std::sync::Mutex::new(resumed_link),
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

    // Phase 3: if the daemon's session linked a zccache daemon PID,
    // stop it before our own final exit. Runs on both explicit shutdown
    // and idle-timeout paths.
    let paths_for_stop = paths.clone();
    tokio::task::spawn_blocking(move || zccache_link::stop_linked_zccache(&paths_for_stop))
        .await
        .ok();

    // Best-effort: remove the endpoint file so a stale socket doesn't
    // confuse the next start. On Windows the named pipe is destroyed
    // when the last handle is closed by the runtime drop below — no
    // cleanup needed there.
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(daemon_sock_path(&paths));
    }
    remove_pid_file(&paths);
    let event = if state.exit_via_idle.load(Ordering::Relaxed) {
        "died-idle"
    } else {
        "died-shutdown"
    };
    append_lifecycle_event(&paths, event);
    Ok(())
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
            let record = BuildRecord {
                session_id,
                repo_root,
                started_at_ms,
                ended_at_ms: None,
                exit_code: None,
                total_wall_ms: None,
                crate_count: 0,
                slowest_crate_us: None,
                slowest_crate_name: None,
            };
            let _ = db::upsert_build(&state.db_path, &record);
            // L4 (issue soldr#980): route the SessionStart event through
            // the batcher so we don't compete with the build's first
            // RecordCompile burst for the redb writer.
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
            // L4 (issue soldr#980): drain any per-compile events still
            // staged in the batcher BEFORE aggregating the session.
            // `aggregate_session` reads from redb, so events buffered in
            // memory would be invisible without this explicit flush.
            state.event_batcher.flush().await;
            // Finalize the BuildRecord: aggregate counts/slowest from
            // events with this session_id, persist back.
            let (count, slowest_us, slowest_name) =
                db::aggregate_session(&state.db_path, session_id).unwrap_or((0u32, None, None));
            if let Ok(Some(mut record)) = db::get_build(&state.db_path, session_id) {
                record.ended_at_ms = Some(ended_at_ms);
                record.exit_code = Some(exit_code);
                record.total_wall_ms = Some((ended_at_ms - record.started_at_ms).max(0) as u64);
                record.crate_count = count;
                record.slowest_crate_us = slowest_us;
                record.slowest_crate_name = slowest_name;
                let _ = db::upsert_build(&state.db_path, &record);
            }
            // SessionEnd ALSO rides the batcher and then gets flushed
            // again so the journal is consistent at the end of the
            // session (and a follow-up status snapshot sees the
            // terminator row).
            state
                .event_batcher
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
            state.event_batcher.flush().await;
        }
        Request::RecordCompile {
            session_id,
            crate_name,
            target_dir,
            started_at_ms,
            duration_us,
        } => {
            // L4 (issue soldr#980): the hot path. Pre-L4 this opened
            // redb and did two write txns (next_event_id + event row)
            // per compile, which fsync'd twice per call — ~3.4 s of
            // pure-sync wait across the 171 misses observed in the
            // cold profile. The batcher coalesces these into one fsync
            // per 64 rows / 100 ms.
            let kind = if duration_us.is_some() {
                db::EventKind::CompileEnd
            } else {
                db::EventKind::CompileStart
            };
            state
                .event_batcher
                .record(db::Event {
                    ts_ms: started_at_ms,
                    session_id: Some(session_id),
                    kind,
                    crate_name: Some(crate_name),
                    duration_us,
                    target_dir: Some(target_dir),
                    exit_code: None,
                })
                .await;
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
        Request::LinkZccache { link } => {
            // Persist to redb so a restart can still stop the linked
            // daemon, AND cache in-process for fast Status replies.
            // Surface persistence errors on stderr — silently dropping
            // the disk write is exactly what caused the #608 regression
            // (`stop_linked_zccache` reads from disk on shutdown, not
            // from this in-memory mutex).
            if let Err(err) = db::set_linked_zccache(&state.db_path, Some(&link)) {
                eprintln!("soldr-daemon: failed to persist linked zccache: {err}");
            }
            if let Ok(mut guard) = state.linked_zccache.lock() {
                *guard = Some(link);
            }
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
            let reply = dispatch_compile(&state, req).await;
            let _ = write_frame_async(&mut stream, &reply).await;
        }
    }
    Ok(())
}

/// Daemon-side compile dispatcher (issue #977 / #980 L1). Forwards to
/// `SoldrZccacheService::compile` and packages the result.
async fn dispatch_compile(
    state: &Arc<State>,
    req: crate::daemon::protocol::CompileRequest,
) -> Response {
    match state.compile_service.compile(req).await {
        Ok(body) => Response::Compile(body),
        Err(err) => Response::Error(format!("embedded zccache compile failed: {err}")),
    }
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
