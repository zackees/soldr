//! Tokio-based soldr-daemon server. Accepts connections on a platform
//! socket / named pipe, decodes one frame per connection, dispatches
//! through the redb-backed target registry, and tracks lightweight
//! per-process state (request count, last activity, optional linked
//! zccache pid placeholder for Phase 3).

#[cfg(unix)]
use crate::cache_lib::daemon_sock_path;
use crate::cache_lib::target_registry::TargetRegistry;
use crate::cache_lib::{data_db_path, soldr_daemon_dir};
use crate::core::SoldrPaths;
use crate::daemon::db;
use crate::daemon::ipc::{read_frame_async, write_frame_async};
use crate::daemon::lifecycle::{append_lifecycle_event, is_live, remove_pid_file, write_pid_file};
use crate::daemon::protocol::{BuildRecord, Request, Response, StatusInfo, PROTOCOL_VERSION};
use crate::daemon::zccache_link;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);

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
    /// Linked zccache PID kept in memory for fast `Status` replies;
    /// also persisted to redb so a daemon restart can resume shutdown.
    linked_zccache_pid: std::sync::Mutex<Option<u32>>,
    start_instant: Instant,
    request_count: AtomicU64,
    last_activity_ms: AtomicU64,
    /// True when the idle watchdog drove the shutdown. Lets the main
    /// task tag the lifecycle event as `died-idle` instead of
    /// `died-shutdown`.
    exit_via_idle: AtomicBool,
    shutdown: Notify,
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
        StatusInfo {
            version: PROTOCOL_VERSION,
            pid: std::process::id(),
            uptime_secs: self.start_instant.elapsed().as_secs(),
            request_count: self.request_count.load(Ordering::Relaxed),
            linked_zccache_pid: *self
                .linked_zccache_pid
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        }
    }
}

/// Synchronous entry point used by both the `soldr-daemon` bin target
/// and `soldr daemon start --foreground`. Builds a tokio runtime and
/// blocks until the daemon exits.
pub fn run(opts: ServerOptions) -> Result<(), ServerError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
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
    // Resume any linked zccache PID persisted across daemon restarts.
    let resumed_pid = db::get_linked_zccache_pid(&db_path).ok().flatten();
    let start_instant = Instant::now();
    let state = Arc::new(State {
        db_path,
        linked_zccache_pid: std::sync::Mutex::new(resumed_pid),
        start_instant,
        request_count: AtomicU64::new(0),
        last_activity_ms: AtomicU64::new(0),
        exit_via_idle: AtomicBool::new(false),
        shutdown: Notify::new(),
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
    let req: Request = match read_frame_async(&mut stream).await {
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
            let _ = db::append_event(
                &state.db_path,
                &db::Event {
                    ts_ms: started_at_ms,
                    session_id: Some(session_id),
                    kind: db::EventKind::SessionStart,
                    crate_name: None,
                    duration_us: None,
                    target_dir: None,
                    exit_code: None,
                },
            );
        }
        Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        } => {
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
            let _ = db::append_event(
                &state.db_path,
                &db::Event {
                    ts_ms: ended_at_ms,
                    session_id: Some(session_id),
                    kind: db::EventKind::SessionEnd,
                    crate_name: None,
                    duration_us: None,
                    target_dir: None,
                    exit_code: Some(exit_code),
                },
            );
        }
        Request::RecordCompile {
            session_id,
            crate_name,
            target_dir,
            started_at_ms,
            duration_us,
        } => {
            let kind = if duration_us.is_some() {
                db::EventKind::CompileEnd
            } else {
                db::EventKind::CompileStart
            };
            let _ = db::append_event(
                &state.db_path,
                &db::Event {
                    ts_ms: started_at_ms,
                    session_id: Some(session_id),
                    kind,
                    crate_name: Some(crate_name),
                    duration_us,
                    target_dir: Some(target_dir),
                    exit_code: None,
                },
            );
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
        Request::LinkZccache { zccache_pid } => {
            // Persist to redb so a restart can still stop the linked
            // daemon, AND cache in-process for fast Status replies.
            let _ = db::set_linked_zccache_pid(&state.db_path, Some(zccache_pid));
            if let Ok(mut guard) = state.linked_zccache_pid.lock() {
                *guard = Some(zccache_pid);
            }
        }
    }
    Ok(())
}

async fn run_idle_watchdog(state: Arc<State>, idle_timeout: Duration) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if state.idle_for() >= idle_timeout {
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
