//! Synchronous, blocking client used by the wrapper hot path and by the
//! `soldr daemon status|stop` CLI surface. All daemon calls are best
//! effort: every error variant is mapped to a `ClientError` so the
//! caller can decide whether to fall back to direct redb writes.

use crate::cache_lib::target_registry::{current_unix_seconds, TargetRegistry};
use crate::cache_lib::{daemon_sock_path, data_db_path};
use crate::core::SoldrPaths;
#[cfg(windows)]
use crate::daemon::ipc::{
    read_frame_async, read_frame_async_for_version, write_frame_async,
    write_frame_async_for_version,
};
#[cfg(unix)]
use crate::daemon::ipc::{
    read_frame_sync, read_frame_sync_for_version, write_frame_sync, write_frame_sync_for_version,
};
use crate::daemon::protocol::{
    BuildRecord, CacheFlushInfo, CompileRequest, CompileStatsInfo, Request, Response, ShutdownAck,
    StatusInfo,
};
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
/// The embedded flush has seven individually bounded phases (pending writes,
/// index writer, and up to five persistence saves), so its IPC budget must be
/// longer than the generic status/shutdown request timeout.
const CACHE_FLUSH_REPLY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Last protocol spoken by pre-generation shutdown acknowledgements. This
/// fallback exists only to retire an older local daemon safely during rollout.
const LEGACY_SHUTDOWN_PROTOCOL_VERSION: u32 = 17;

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

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;
#[cfg(windows)]
const PIPE_BUSY_RETRY_LIMIT: u32 = 8;

/// How many times a client re-dials after the daemon replies
/// `Response::Backpressure`. Shared by both transports since soldr#1853 —
/// compile admission is no longer Windows-only, so the AF_UNIX path needs
/// the same bounded back-off.
const BACKPRESSURE_RETRY_LIMIT: u32 = 8;

#[cfg(windows)]
struct WindowsPipeOpen {
    stream: tokio::net::windows::named_pipe::NamedPipeClient,
    busy_retries: u32,
}

#[cfg(windows)]
fn busy_pipe_retry_delay(attempt: u32) -> Duration {
    let base_ms = (2_u64.saturating_mul(1_u64 << attempt.min(5))).min(64);
    // A tiny deterministic jitter avoids synchronized cargo workers while
    // keeping this retry policy testable and dependency-free.
    let jitter_ms = (u64::from(attempt) * 17 + u64::from(std::process::id())) % 4;
    Duration::from_millis(base_ms + jitter_ms)
}

#[cfg(windows)]
async fn open_windows_pipe_with_retry(path: &Path) -> std::io::Result<WindowsPipeOpen> {
    use tokio::net::windows::named_pipe::ClientOptions;
    for attempt in 0..PIPE_BUSY_RETRY_LIMIT {
        match ClientOptions::new().open(path) {
            Ok(stream) => {
                return Ok(WindowsPipeOpen {
                    stream,
                    busy_retries: attempt,
                })
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && attempt + 1 < PIPE_BUSY_RETRY_LIMIT =>
            {
                // A busy pipe is listener-pool backpressure, not evidence that the
                // daemon died. Keep retrying inside this client call so callers do
                // not start recovery or bypass the cache.
                tokio::time::sleep(busy_pipe_retry_delay(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop either returned a client or its last IO error")
}

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
    /// The daemon answered, but speaks a protocol this binary cannot parse
    /// (#1853).
    ///
    /// Distinct from [`Self::Protocol`] because retrying cannot help: a
    /// version-skewed daemon can never serve this client, no matter how many
    /// attempts remain. The caller must displace the daemon or fall back to a
    /// direct compile rather than burning its retry budget.
    VersionMismatch(String),
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        #[cfg(windows)]
        if e.raw_os_error() == Some(ERROR_PIPE_BUSY) {
            // The retry cap only bounds a single client call; it does not
            // change daemon liveness. Keep this out of the caller's generic
            // I/O-recovery branch, which is allowed to spawn a missing daemon.
            return ClientError::Protocol(
                "Windows named pipe remained busy after retry budget".into(),
            );
        }
        // #1853: both the version-mismatch branch and the pre-handshake
        // reject record surface as InvalidData from the frame reader. Classify
        // them here, in the shared conversion, so Unix and Windows transports
        // both get it from one place.
        if e.kind() == std::io::ErrorKind::InvalidData {
            let message = e.to_string();
            if message.contains("protocol version mismatch")
                || message.contains("daemon rejected connection")
            {
                return ClientError::VersionMismatch(message);
            }
        }
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

fn submit_request_for_version(
    sock_path: &Path,
    req: &Request,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    #[cfg(windows)]
    {
        submit_request_windows_with_timeout_and_version(
            sock_path,
            req,
            REPLY_TIMEOUT,
            protocol_version,
        )
    }
    #[cfg(unix)]
    {
        let mut stream = connect(sock_path, REPLY_TIMEOUT)?;
        write_frame_sync_for_version(&mut stream, req, protocol_version)?;
        let resp: Response = read_frame_sync_for_version(&mut stream, protocol_version)?;
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

/// soldr#1368: read the embedded zccache service's cumulative compile
/// counters. `soldr session start` captures a baseline and `soldr
/// session end` diffs against it to report per-session hit/miss stats —
/// replacing the removed managed `zccache session-end` subprocess.
pub fn compile_stats(sock_path: &Path) -> Result<CompileStatsInfo, ClientError> {
    match submit_request(sock_path, &Request::CompileStats)? {
        Response::CompileStats(info) => Ok(info),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

pub fn shutdown(sock_path: &Path) -> Result<ShutdownAck, ClientError> {
    let current_error = match submit_request(sock_path, &Request::Shutdown) {
        Ok(Response::ShuttingDown(ack)) => return Ok(ack),
        Ok(Response::Error(msg)) => return Err(ClientError::Protocol(msg)),
        Ok(other) => {
            return Err(ClientError::Protocol(format!(
                "unexpected response: {other:?}"
            )))
        }
        Err(ClientError::NotRunning) => return Err(ClientError::NotRunning),
        Err(error) => error,
    };

    // v18 rollout bridge: v17's empty shutdown Ack cannot identify its
    // responder. Read v17 status immediately before requesting shutdown, then
    // use that PID only for waiting. Callers never signal this legacy identity.
    let legacy_status = match submit_request_for_version(
        sock_path,
        &Request::Status,
        LEGACY_SHUTDOWN_PROTOCOL_VERSION,
    ) {
        Ok(Response::Status(status)) => Some(status),
        _ => None,
    };
    match submit_request_for_version(
        sock_path,
        &Request::Shutdown,
        LEGACY_SHUTDOWN_PROTOCOL_VERSION,
    ) {
        Ok(Response::ShuttingDown(ack)) if ack.pid != 0 => Ok(ack),
        Ok(Response::ShuttingDown(_)) => legacy_status
            .map(|status| ShutdownAck {
                pid: status.pid,
                generation: status.generation,
            })
            .ok_or_else(|| {
                ClientError::Protocol(
                    "legacy daemon acknowledged shutdown without a responder identity".into(),
                )
            }),
        _ => Err(current_error),
    }
}

/// Issue #1286 (F1): ask the daemon to checkpoint the embedded zccache
/// state (artifact index, depgraph snapshot, metadata cache) to disk
/// without shutting down. Used by `soldr save` / `soldr cache flush`
/// before archiving the cache tree.
pub fn flush_caches(sock_path: &Path) -> Result<CacheFlushInfo, ClientError> {
    match submit_request_with_timeout(sock_path, &Request::FlushCaches, CACHE_FLUSH_REPLY_TIMEOUT)?
    {
        Response::CacheFlushed(info) => Ok(info),
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

/// Acknowledged build-session lifecycle helper. The daemon only returns Ok
/// after the start record and SessionStart event are persisted.
pub fn build_session_start(
    paths: &SoldrPaths,
    session_id: u64,
    repo_root: &Path,
    started_at_ms: i64,
) -> Result<(), ClientError> {
    let sock = default_sock_path(paths);
    match submit_request(
        &sock,
        &Request::BuildSessionStart {
            session_id,
            repo_root: repo_root.display().to_string(),
            started_at_ms,
        },
    )? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected build_session_start response: {other:?}"
        ))),
    }
}

/// Acknowledged finalization (soldr#1536): blocks until the daemon has
/// rolled the session up from its in-memory aggregate, persisted the
/// finalized BuildRecord, and flushed every staged session event to
/// redb. `Ok(())` therefore means the wrapper can trust the persisted
/// aggregate instead of re-scanning the event table; any error routes
/// the caller to the direct-redb fallback exactly like before.
pub fn build_session_end(
    paths: &SoldrPaths,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
) -> Result<(), ClientError> {
    let sock = default_sock_path(paths);
    match submit_request(
        &sock,
        &Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        },
    )? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected build_session_end response: {other:?}"
        ))),
    }
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
        // Honour `Response::Backpressure` (soldr#1853). Compile admission
        // used to be `#[cfg(windows)]`-only, so this transport accepted every
        // connection unconditionally and shed load by resetting sockets —
        // surfacing as ECONNRESET, a burned 30 s budget, and a red build.
        // Applying admission on Unix needs a client that backs off instead of
        // treating the reply as a protocol violation, which is exactly what
        // `open_compile_pipe_with_backpressure` already does for named pipes.
        //
        // Synchronous by necessity: this path is sync end to end, so the wait
        // is a `thread::sleep` and each retry re-dials. The first frame is
        // carried into the loop below rather than re-read, since reading it
        // here to test for backpressure consumes it.
        let (mut stream, first_frame) = {
            let mut admitted = None;
            let mut last_retry_after_ms = 0u32;
            for attempt in 0..BACKPRESSURE_RETRY_LIMIT {
                let mut stream = connect(sock_path, compile_reply_timeout())?;
                write_frame_sync(&mut stream, &Request::Compile(req.clone()))?;
                let frame: Response = read_frame_sync(&mut stream)?;
                match frame {
                    Response::Backpressure { retry_after_ms } => {
                        last_retry_after_ms = retry_after_ms;
                        if attempt + 1 == BACKPRESSURE_RETRY_LIMIT {
                            break;
                        }
                        // Per-process jitter so wrappers released together do
                        // not re-dial in lockstep; mirrors the Windows spread.
                        let jitter_ms =
                            (u64::from(attempt) * 11 + u64::from(std::process::id())) % 4;
                        std::thread::sleep(Duration::from_millis(
                            u64::from(retry_after_ms) + jitter_ms,
                        ));
                    }
                    other => {
                        admitted = Some((stream, other));
                        break;
                    }
                }
            }
            match admitted {
                Some(pair) => pair,
                // Deliberately `Io`, not `Protocol`: a daemon too busy to
                // admit us is *unavailable*, and `Protocol` is classified as
                // "the daemon answered, so do not degrade"
                // (`compile_dispatch::client_error_indicates_daemon_unavailable`).
                // Reporting this as Io lets the caller fall back to a direct
                // uncached rustc instead of failing the build.
                None => {
                    return Err(ClientError::Io(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!(
                            "daemon IPC admission stayed backpressured across                              {BACKPRESSURE_RETRY_LIMIT} attempts                              ({last_retry_after_ms}ms apart)"
                        ),
                    )))
                }
            }
        };
        let mut pending = Some(first_frame);
        loop {
            let frame: Response = match pending.take() {
                Some(frame) => frame,
                None => read_frame_sync(&mut stream)?,
            };
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

    let sock = default_sock_path(paths);
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
async fn open_compile_pipe_with_backpressure(
    sock_path: &Path,
    req: &CompileRequest,
) -> std::io::Result<(tokio::net::windows::named_pipe::NamedPipeClient, Response)> {
    for attempt in 0..BACKPRESSURE_RETRY_LIMIT {
        let opened = open_windows_pipe_with_retry(sock_path).await?;
        let mut stream = opened.stream;
        let mut compile_req = req.clone();
        compile_req.ipc_busy_retries = compile_req
            .ipc_busy_retries
            .saturating_add(opened.busy_retries);
        let first = tokio::time::timeout(compile_reply_timeout(), async {
            write_frame_async(&mut stream, &Request::Compile(compile_req)).await?;
            read_frame_async(&mut stream).await
        })
        .await
        .map_err(|_| {
            windows_timeout_error("daemon IPC compile admission", compile_reply_timeout())
        })??;
        match first {
            Response::Backpressure { retry_after_ms } if attempt + 1 < BACKPRESSURE_RETRY_LIMIT => {
                let jitter_ms = (u64::from(attempt) * 11 + u64::from(std::process::id())) % 4;
                tokio::time::sleep(Duration::from_millis(u64::from(retry_after_ms) + jitter_ms))
                    .await;
            }
            response => return Ok((stream, response)),
        }
    }
    unreachable!("backpressure loop returns on the final response")
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
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC hot-path write", HOT_PATH_TIMEOUT, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = open_windows_pipe_with_retry(&sock_path).await?.stream;
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
    submit_request_windows_with_timeout_and_version(
        sock_path,
        req,
        deadline,
        crate::daemon::protocol::PROTOCOL_VERSION,
    )
}

#[cfg(windows)]
fn submit_request_windows_with_timeout_and_version(
    sock_path: &Path,
    req: &Request,
    deadline: Duration,
    protocol_version: u32,
) -> Result<Response, ClientError> {
    use tokio::time::timeout;

    let sock_path = sock_path.to_path_buf();
    let req = req.clone();
    run_windows_ipc("daemon IPC request", deadline, move || {
        let runtime = windows_runtime()?;
        runtime.block_on(async move {
            let mut stream = open_windows_pipe_with_retry(&sock_path).await?.stream;
            timeout(deadline, async {
                write_frame_async_for_version(&mut stream, &req, protocol_version).await?;
                read_frame_async_for_version(&mut stream, protocol_version).await
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
                let (mut stream, first_frame) = match open_compile_pipe_with_backpressure(
                    &sock_path, &req,
                )
                .await
                {
                    Ok(connection) => connection,
                    Err(e) => {
                        let _ = tx.send(StreamMsg::Err(ClientError::from(e)));
                        return;
                    }
                };
                let mut first_frame = Some(first_frame);
                loop {
                    let frame = match first_frame.take() {
                        Some(frame) => frame,
                        None => match timeout(
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
                                let _ = tx.send(StreamMsg::Err(ClientError::Io(
                                    windows_timeout_error(
                                        "daemon IPC compile read",
                                        compile_reply_timeout(),
                                    ),
                                )));
                                return;
                            }
                        },
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
                        Response::Backpressure { retry_after_ms } => {
                            let _ = tx.send(StreamMsg::Err(ClientError::Protocol(format!(
                                "daemon IPC admission remained backpressured after retry ({retry_after_ms}ms)"
                            ))));
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
