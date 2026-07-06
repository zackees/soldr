//! Synchronous, blocking client used by the wrapper hot path and by the
//! `soldr daemon status|stop` CLI surface. All daemon calls are best
//! effort: every error variant is mapped to a `ClientError` so the
//! caller can decide whether to fall back to direct redb writes.

use crate::cache_lib::target_registry::{current_unix_seconds, TargetRegistry};
use crate::cache_lib::{daemon_sock_path, data_db_path};
use crate::core::SoldrPaths;
use crate::daemon::db;
#[cfg(windows)]
use crate::daemon::ipc::{read_frame_async, write_frame_async};
#[cfg(unix)]
use crate::daemon::ipc::{read_frame_sync, write_frame_sync};
use crate::daemon::protocol::{BuildRecord, CompileRequest, Request, Response, StatusInfo};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 50 ms write timeout matches the plan: short enough that the wrapper
/// never blocks the hot path on a hung daemon.
const HOT_PATH_TIMEOUT: Duration = Duration::from_millis(50);

/// Slightly more generous timeout for request/response calls that need
/// to read a body back (status, shutdown). Still small enough that the
/// CLI returns quickly even if the daemon is unresponsive.
const REPLY_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Default compile-dispatch timeout — rustc may take minutes for a release
/// build of a large crate, so the default stays generous (30 minutes): a
/// stuck rustc cannot wedge the wrapper forever, but any legitimate compile
/// inside that bound runs to completion. Issue #977 Phase 5 / #980 L1.
const DEFAULT_REPLY_TIMEOUT_SECS: u64 = 30 * 60;

/// Env override for [`compile_reply_timeout`] (issue #1364). Lets an
/// operator fail fast on a wedged cache without waiting out the 30-minute
/// backstop, e.g. `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=30`. `0`, empty, or an
/// unparseable value falls back to [`DEFAULT_REPLY_TIMEOUT_SECS`].
const REPLY_TIMEOUT_ENV: &str = "SOLDR_COMPILE_REPLY_TIMEOUT_SECS";

/// Compile-dispatch timeout, resolved once from the environment.
fn compile_reply_timeout() -> Duration {
    static CACHED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| parse_reply_timeout(std::env::var(REPLY_TIMEOUT_ENV).ok().as_deref()))
}

/// Pure policy for [`compile_reply_timeout`] so the parse/fallback matrix
/// is unit-testable without mutating the process env.
fn parse_reply_timeout(value: Option<&str>) -> Duration {
    let secs = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(DEFAULT_REPLY_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

#[derive(Debug)]
pub enum ClientError {
    /// No daemon endpoint exists at the expected path (most common case
    /// on a fresh checkout — caller should fall back to direct redb).
    NotRunning,
    /// Endpoint exists but the connect / read / write failed.
    Io(std::io::Error),
    /// Daemon answered something we didn't ask for (or an Error variant).
    Protocol(String),
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning
            }
            _ => ClientError::Io(e),
        }
    }
}

/// Submit `req` to the daemon and drop the connection without reading
/// any reply. Used for `RecordTargetTouch`. The wrapper hot path calls
/// this and ignores the result on the failure side.
pub fn submit_fire_and_forget(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    #[cfg(windows)]
    {
        submit_fire_and_forget_windows(sock_path, req)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, HOT_PATH_TIMEOUT)?;
        write_frame_sync(&mut stream, req)?;
        Ok(())
    }
}

/// Submit `req`, wait for one `Response`, return it.
pub fn submit_request(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    #[cfg(windows)]
    {
        submit_request_windows(sock_path, req)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
        write_frame_sync(&mut stream, req)?;
        let resp: Response = read_frame_sync(&mut stream)?;
        Ok(resp)
    }
}

pub fn status(sock_path: &Path) -> Result<StatusInfo, ClientError> {
    match submit_request(sock_path, &Request::Status)? {
        Response::Status(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn shutdown(sock_path: &Path) -> Result<(), ClientError> {
    match submit_request(sock_path, &Request::Shutdown)? {
        Response::ShuttingDown => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Issue #1286 (F1): ask the daemon to checkpoint the embedded zccache
/// state (artifact index, depgraph snapshot, metadata cache) to disk
/// without shutting down. Used by `soldr save` / `soldr cache flush`
/// before archiving the cache tree.
pub fn flush_caches(sock_path: &Path) -> Result<(), ClientError> {
    match submit_request(sock_path, &Request::FlushCaches)? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_builds(
    sock_path: &Path,
    limit: u32,
    since_ms: Option<i64>,
) -> Result<Vec<BuildRecord>, ClientError> {
    match submit_request(sock_path, &Request::ListBuilds { limit, since_ms })? {
        Response::Builds(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn list_slow_builds(
    sock_path: &Path,
    threshold_ms: u64,
    limit: u32,
) -> Result<Vec<BuildRecord>, ClientError> {
    match submit_request(
        sock_path,
        &Request::ListSlowBuilds {
            threshold_ms,
            limit,
        },
    )? {
        Response::Builds(rows) => Ok(rows),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Convenience: build session lifecycle and per-compile fire-and-forget
/// helpers. Each one is best-effort — if the daemon isn't reachable,
/// the cargo build still completes; we just lose the timing data.
pub fn build_session_start(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) {
    let sock = default_sock_path(paths);
    let _ = submit_fire_and_forget(
        &sock,
        &Request::BuildSessionStart {
            session_id,
            repo_root: repo_root.display().to_string(),
            started_at_ms,
        },
    );
}

pub fn build_session_end(paths: &SoldrPaths, session_id: u64, exit_code: i32, ended_at_ms: i64) {
    let sock = default_sock_path(paths);
    let _ = submit_fire_and_forget(
        &sock,
        &Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        },
    );
}

pub fn record_compile(
    paths: &SoldrPaths,
    session_id: u64,
    crate_name: &str,
    target_dir: &Path,
    started_at_ms: i64,
    duration_us: Option<u64>,
) {
    let sock = default_sock_path(paths);
    let _ = submit_fire_and_forget(
        &sock,
        &Request::RecordCompile {
            session_id,
            crate_name: crate_name.to_string(),
            target_dir: target_dir.display().to_string(),
            started_at_ms,
            duration_us,
        },
    );
}

pub fn link_zccache(paths: &SoldrPaths, link: crate::daemon::protocol::ZccacheDaemonLink) {
    let sock = default_sock_path(paths);
    if submit_fire_and_forget(&sock, &Request::LinkZccache { link: link.clone() }).is_ok() {
        return;
    }
    let _ = db::set_linked_zccache(&data_db_path(paths), Some(&link));
}

/// PR 1 cook-index client surface (#576). PR 2 (`soldr cook`) and PR 3
/// (cargo-front-door pre-flight) consume these; PR 1 ships dormant so
/// the helpers here are unused outside integration tests.
///
/// The reply is a [`CookLookupOutcome`]:
/// * `Hit { sha256, path, size_bytes, origin_url_normalized }` — PR 3
///   verifies `sha256` against the bytes at `path` before extracting.
/// * `Miss { previous_origin_recipe_hashes }` — used as a drift
///   diagnostic when the pre-flight misses.
///
/// `Err(ClientError::NotRunning)` means the daemon endpoint is not
/// reachable — caller must NOT treat this as a hard error; the hot
/// path falls through to a normal cargo run.
#[allow(clippy::too_many_arguments)]
pub fn cook_lookup(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    origin_url_normalized: Option<String>,
) -> Result<CookLookupOutcome, ClientError> {
    cook_lookup_with_branch_lineage(
        sock_path,
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        origin_url_normalized,
        Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cook_lookup_with_branch_lineage(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    origin_url_normalized: Option<String>,
    branch_lineage: Vec<String>,
) -> Result<CookLookupOutcome, ClientError> {
    let req = Request::CookLookup {
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        origin_url_normalized,
        branch_lineage,
    };
    match submit_request(sock_path, &req)? {
        Response::CookHit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
        } => Ok(CookLookupOutcome::Hit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
        }),
        Response::CookMiss {
            previous_origin_recipe_hashes,
        } => Ok(CookLookupOutcome::Miss {
            previous_origin_recipe_hashes,
        }),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected cook_lookup response: {other:?}"
        ))),
    }
}

/// Strongly-typed reply for [`cook_lookup`] / [`cook_lookup_full`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookLookupOutcome {
    Hit {
        sha256: [u8; 32],
        path: String,
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        matched_recipe_hash: Option<[u8; 32]>,
        exact_recipe_match: bool,
        branch_name: Option<String>,
    },
    Miss {
        previous_origin_recipe_hashes: Vec<[u8; 32]>,
    },
}

/// Register a cook artifact with the daemon. PR 2's `soldr cook`
/// worker calls this after writing `<sha256>.tar.zst` to
/// `~/.soldr/cache/cook/`. Blocks for the daemon's `Ack` reply
/// because PR 2 wants to know whether the indexing succeeded before
/// emitting its `soldr cook: indexed` line.
#[allow(clippy::too_many_arguments)]
pub fn cook_record(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    sha256: [u8; 32],
    size_bytes: u64,
    origin_url_normalized: Option<String>,
    cook_cmd_summary: String,
) -> Result<(), ClientError> {
    cook_record_with_branch(
        sock_path,
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        sha256,
        size_bytes,
        origin_url_normalized,
        None,
        cook_cmd_summary,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cook_record_with_branch(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    sha256: [u8; 32],
    size_bytes: u64,
    origin_url_normalized: Option<String>,
    branch_name: Option<String>,
    cook_cmd_summary: String,
) -> Result<(), ClientError> {
    let req = Request::CookRecord {
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
    };
    match submit_request(sock_path, &req)? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected cook_record response: {other:?}"
        ))),
    }
}

/// Fire-and-forget bump of `last_used_unix_ms` for the entry whose
/// sha256 matches. Best-effort by design: a touch failure must never
/// affect callers (eviction will simply observe stale `last_used`).
pub fn cook_touch(sock_path: &Path, sha256: [u8; 32]) -> Result<(), ClientError> {
    submit_fire_and_forget(sock_path, &Request::CookTouch { sha256 })
}

/// Metadata returned by the streaming compile call after the last
/// chunk frame has been consumed (#983 Phase 5b). Carries everything
/// the wrapper needs to surface to cargo — the captured rustc output
/// has already been written through to the caller-provided sinks.
#[derive(Debug, Clone)]
pub struct CompileDoneInfo {
    pub exit_code: i32,
    #[allow(dead_code)] // forwarded for future telemetry; wrapper does not act on it today
    pub cached: bool,
    #[allow(dead_code)] // 1=Hit, 2=Miss, 3=Error — same shape as CompileResponseBody
    pub cache_outcome: i32,
    /// Daemon-side audit id for the compile. Empty in Phase 5b1 (the
    /// embedded zccache service does not yet surface it). Plumbed
    /// through so Phase 5b2 can fill it without another version bump.
    #[allow(dead_code)]
    pub compile_id: String,
}

/// Dispatch a single rustc compile to the daemon's embedded zccache
/// service and stream the captured rustc stdout/stderr to the caller's
/// sinks as they arrive (#983 Phase 5b — superseded the v6 single-frame
/// reply path).
///
/// The reply timeout is generous (30 minutes) because rustc itself may
/// take many minutes for a release build of a large crate. The function
/// reads frames in a loop, dispatching `CompileStdoutChunk` /
/// `CompileStderrChunk` to the matching writer and returning once it
/// sees the terminal `CompileDone` frame.
///
/// On any timeout / IO error the wrapper hard-errors (the legacy
/// `zccache.exe` fork path was removed in #980 L1's second pass).
pub fn compile_streaming<O, E>(
    sock_path: &Path,
    req: CompileRequest,
    mut stdout: O,
    mut stderr: E,
) -> Result<CompileDoneInfo, ClientError>
where
    O: Write,
    E: Write,
{
    #[cfg(windows)]
    {
        compile_streaming_windows(sock_path, req, &mut stdout, &mut stderr)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, compile_reply_timeout())?;
        write_frame_sync(&mut stream, &Request::Compile(req))?;
        loop {
            let frame: Response = read_frame_sync(&mut stream)?;
            match frame {
                Response::CompileStdoutChunk(bytes) => {
                    tracing::debug!(
                        target: "soldr::client::compile_stream",
                        bytes = bytes.len(),
                        "stdout chunk received",
                    );
                    stdout.write_all(&bytes).map_err(ClientError::Io)?;
                }
                Response::CompileStderrChunk(bytes) => {
                    tracing::debug!(
                        target: "soldr::client::compile_stream",
                        bytes = bytes.len(),
                        "stderr chunk received",
                    );
                    stderr.write_all(&bytes).map_err(ClientError::Io)?;
                }
                Response::CompileDone {
                    exit_code,
                    cached,
                    cache_outcome,
                    compile_id,
                } => {
                    tracing::debug!(
                        target: "soldr::client::compile_stream",
                        exit_code,
                        cached,
                        cache_outcome,
                        "compile done — streaming reply complete",
                    );
                    return Ok(CompileDoneInfo {
                        exit_code,
                        cached,
                        cache_outcome,
                        compile_id,
                    });
                }
                Response::Error(msg) => return Err(ClientError::Protocol(msg)),
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected compile stream frame: {other:?}"
                    )));
                }
            }
        }
    }
}

/// Same shape as [`submit_request`] but with an explicit reply timeout.
/// Extracted so [`compile`] can use a 30-minute budget without bloating
/// the call surface of the generic helper.
pub fn submit_request_with_timeout(
    sock_path: &Path,
    req: &Request,
    timeout: Duration,
) -> Result<Response, ClientError> {
    #[cfg(windows)]
    {
        submit_request_windows_with_timeout(sock_path, req, timeout)
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, timeout)?;
        write_frame_sync(&mut stream, req)?;
        let resp: Response = read_frame_sync(&mut stream)?;
        Ok(resp)
    }
}

/// Wrapper-side entry point. Tries the daemon first; on any failure,
/// upserts the row directly to the redb file. **Never** propagates
/// errors — a missing daemon must not break a build.
pub fn record_target_touch_or_fallback(paths: &SoldrPaths, target: &Path) {
    let unix_seconds = match current_unix_seconds() {
        Ok(s) => s,
        Err(_) => return,
    };

    let sock = daemon_sock_path(paths);
    let req = Request::RecordTargetTouch {
        path: target.display().to_string(),
        unix_seconds,
    };
    if submit_fire_and_forget(&sock, &req).is_ok() {
        return;
    }

    let db_path = data_db_path(paths);
    if let Ok(registry) = TargetRegistry::open(&db_path) {
        let _ = registry.upsert_with_time(target, unix_seconds);
    }
}

#[cfg(unix)]
fn connect(sock_path: &Path, timeout: Duration) -> Result<UnixOrPipe, ClientError> {
    let stream = std::os::unix::net::UnixStream::connect(sock_path)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_read_timeout(Some(timeout.max(Duration::from_millis(200))))?;
    Ok(UnixOrPipe(stream))
}

#[cfg(unix)]
pub struct UnixOrPipe(std::os::unix::net::UnixStream);

#[cfg(unix)]
impl std::io::Read for UnixOrPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(unix)]
impl std::io::Write for UnixOrPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(windows)]
fn windows_timeout_error(operation: &str, timeout: Duration) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{operation} timed out after {}ms", timeout.as_millis()),
    )
}

#[cfg(windows)]
fn windows_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
}

#[cfg(windows)]
fn run_windows_ipc<T, F>(operation: &'static str, timeout: Duration, f: F) -> Result<T, ClientError>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("soldr-daemon-client".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .map_err(ClientError::Io)?;

    match rx.recv_timeout(timeout) {
        Ok(result) => result.map_err(ClientError::from),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(ClientError::Io(windows_timeout_error(operation, timeout)))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(ClientError::Io(
            std::io::Error::other(format!("{operation} worker exited without a result")),
        )),
    }
}

#[cfg(windows)]
fn submit_fire_and_forget_windows(sock_path: &Path, req: &Request) -> Result<(), ClientError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC hot-path write", HOT_PATH_TIMEOUT, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = ClientOptions::new().open(sock_path)?;
            timeout(HOT_PATH_TIMEOUT, write_frame_async(&mut stream, &req))
                .await
                .map_err(|_| {
                    windows_timeout_error("daemon IPC hot-path write", HOT_PATH_TIMEOUT)
                })??;
            Ok::<(), std::io::Error>(())
        })
    })
}

#[cfg(windows)]
fn submit_request_windows(sock_path: &Path, req: &Request) -> Result<Response, ClientError> {
    submit_request_windows_with_timeout(sock_path, req, REPLY_TIMEOUT)
}

#[cfg(windows)]
fn submit_request_windows_with_timeout(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
) -> Result<Response, ClientError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC request", deadline, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = ClientOptions::new().open(sock_path)?;
            timeout(deadline, async {
                write_frame_async(&mut stream, &req).await?;
                read_frame_async(&mut stream).await
            })
            .await
            .map_err(|_| windows_timeout_error("daemon IPC request", deadline))?
        })
    })
}

/// Windows variant of [`compile_streaming`]. Tunnels the chunks back
/// from a tokio runtime thread to the calling thread via a single
/// std::sync::mpsc channel; the calling thread drains the channel and
/// forwards bytes to the caller's `stdout` / `stderr` sinks (which
/// usually are `std::io::stdout()` and `std::io::stderr()` from the
/// wrapper). Keeping the user writers off the tokio thread sidesteps
/// Windows's blocking-IO-on-stdout quirks and matches the sync shape
/// the Unix branch uses.
#[cfg(windows)]
fn compile_streaming_windows<O, E>(
    sock_path: &Path,
    req: CompileRequest,
    stdout: &mut O,
    stderr: &mut E,
) -> Result<CompileDoneInfo, ClientError>
where
    O: Write,
    E: Write,
{
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::timeout;

    /// Frames forwarded from the IPC worker thread to the calling
    /// thread. `Done` carries the terminal metadata; `Err` short-
    /// circuits on protocol/io failure.
    enum StreamMsg {
        Stdout(Vec<u8>),
        Stderr(Vec<u8>),
        Done(CompileDoneInfo),
        Err(ClientError),
    }

    let sock_path = sock_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<StreamMsg>();

    let worker = std::thread::Builder::new()
        .name("soldr-daemon-client-stream".into())
        .spawn(move || {
            let runtime = match windows_runtime() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                    return;
                }
            };
            runtime.block_on(async move {
                let mut stream = match ClientOptions::new().open(&sock_path) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(StreamMsg::Err(ClientError::from(e)));
                        return;
                    }
                };
                let request = Request::Compile(req);
                if let Err(e) = timeout(
                    compile_reply_timeout(),
                    write_frame_async(&mut stream, &request),
                )
                .await
                .map_err(|_| {
                    windows_timeout_error("daemon IPC compile write", compile_reply_timeout())
                })
                .and_then(|res| res)
                {
                    let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                    return;
                }
                loop {
                    let frame = match timeout(
                        compile_reply_timeout(),
                        read_frame_async::<_, Response>(&mut stream),
                    )
                    .await
                    {
                        Ok(Ok(f)) => f,
                        Ok(Err(e)) => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Io(e)));
                            return;
                        }
                        Err(_) => {
                            let _ =
                                tx.send(StreamMsg::Err(ClientError::Io(windows_timeout_error(
                                    "daemon IPC compile read",
                                    compile_reply_timeout(),
                                ))));
                            return;
                        }
                    };
                    match frame {
                        Response::CompileStdoutChunk(bytes) => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                bytes = bytes.len(),
                                "stdout chunk received",
                            );
                            if tx.send(StreamMsg::Stdout(bytes)).is_err() {
                                return;
                            }
                        }
                        Response::CompileStderrChunk(bytes) => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                bytes = bytes.len(),
                                "stderr chunk received",
                            );
                            if tx.send(StreamMsg::Stderr(bytes)).is_err() {
                                return;
                            }
                        }
                        Response::CompileDone {
                            exit_code,
                            cached,
                            cache_outcome,
                            compile_id,
                        } => {
                            tracing::debug!(
                                target: "soldr::client::compile_stream",
                                exit_code,
                                cached,
                                cache_outcome,
                                "compile done — streaming reply complete",
                            );
                            let _ = tx.send(StreamMsg::Done(CompileDoneInfo {
                                exit_code,
                                cached,
                                cache_outcome,
                                compile_id,
                            }));
                            return;
                        }
                        Response::Error(msg) => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(msg)));
                            return;
                        }
                        other => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(format!(
                                "unexpected compile stream frame: {other:?}"
                            ))));
                            return;
                        }
                    }
                }
            });
        })
        .map_err(ClientError::Io)?;

    let result = loop {
        match rx.recv() {
            Ok(StreamMsg::Stdout(bytes)) => {
                stdout.write_all(&bytes).map_err(ClientError::Io)?;
            }
            Ok(StreamMsg::Stderr(bytes)) => {
                stderr.write_all(&bytes).map_err(ClientError::Io)?;
            }
            Ok(StreamMsg::Done(info)) => break Ok(info),
            Ok(StreamMsg::Err(e)) => break Err(e),
            Err(_) => {
                break Err(ClientError::Io(std::io::Error::other(
                    "soldr-daemon-client-stream worker exited without a result",
                )))
            }
        }
    };
    // Best effort join — the worker has already pushed its final
    // message at this point, so this returns promptly.
    let _ = worker.join();
    result
}

/// Returns the well-known socket path the wrapper should use. Centralized
/// here so callers don't need to import `cache_lib` directly.
pub fn default_sock_path(paths: &SoldrPaths) -> PathBuf {
    #[cfg(unix)]
    {
        daemon_sock_path(paths)
    }
    #[cfg(windows)]
    {
        use crate::cache_lib::daemon_pipe_name;
        PathBuf::from(format!(r"\\.\pipe\{}", daemon_pipe_name(paths)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(reply_timeout_defaults_to_30_min, {
        // Unset / empty / non-numeric / zero all fall back to the generous
        // default so a legitimate slow release compile is never cut off.
        let default = Duration::from_secs(DEFAULT_REPLY_TIMEOUT_SECS);
        assert_eq!(parse_reply_timeout(None), default);
        assert_eq!(parse_reply_timeout(Some("")), default);
        assert_eq!(parse_reply_timeout(Some("nope")), default);
        assert_eq!(parse_reply_timeout(Some("0")), default);
    });

    crate::timed_test!(reply_timeout_env_override_fails_fast, {
        // #1364: an operator can opt into a short fail-fast budget.
        assert_eq!(parse_reply_timeout(Some("30")), Duration::from_secs(30));
        assert_eq!(parse_reply_timeout(Some("  5 ")), Duration::from_secs(5));
    });
}
