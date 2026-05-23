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
use crate::daemon::ipc::{read_frame_async, write_frame_async};
use crate::daemon::lifecycle::{append_lifecycle_event, is_live, remove_pid_file, write_pid_file};
use crate::daemon::protocol::{Request, Response, StatusInfo, PROTOCOL_VERSION};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    registry: TargetRegistry,
    start_instant: Instant,
    request_count: AtomicU64,
    last_activity_ms: AtomicU64,
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
            linked_zccache_pid: None,
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

    let registry = TargetRegistry::open(&data_db_path(&paths))?;
    let start_instant = Instant::now();
    let state = Arc::new(State {
        registry,
        start_instant,
        request_count: AtomicU64::new(0),
        last_activity_ms: AtomicU64::new(0),
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

    // Best-effort: remove the endpoint file so a stale socket doesn't
    // confuse the next start. On Windows the named pipe is destroyed
    // when the last handle is closed by the runtime drop below — no
    // cleanup needed there.
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(daemon_sock_path(&paths));
    }
    remove_pid_file(&paths);
    append_lifecycle_event(&paths, "died-shutdown");
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
            // Fire-and-forget: best-effort write, no reply.
            let _ = state
                .registry
                .upsert_with_time(Path::new(&path), unix_seconds);
        }
        Request::Status => {
            let info = state.status();
            let _ = write_frame_async(&mut stream, &Response::Status(info)).await;
        }
        Request::Shutdown => {
            let _ = write_frame_async(&mut stream, &Response::ShuttingDown).await;
            state.shutdown.notify_waiters();
        }
    }
    Ok(())
}

async fn run_idle_watchdog(state: Arc<State>, idle_timeout: Duration) {
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if state.idle_for() >= idle_timeout {
            // Caller (run_async) re-uses the same notification path
            // that signal handlers + Shutdown requests use.
            state.shutdown.notify_waiters();
            // Stamp an idle exit before the main task tears down so the
            // lifecycle log captures it distinctly from a Shutdown RPC.
            // We can't borrow SoldrPaths here without re-resolving;
            // skip the event-tagging refinement for Phase 1.
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
