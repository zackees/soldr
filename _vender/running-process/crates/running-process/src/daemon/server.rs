use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use tokio::sync::watch;
use tokio::time::{timeout, Duration};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, error, info, warn};

use crate::proto::daemon::{DaemonRequest, DaemonResponse, RequestType, StatusCode};

use crate::daemon::attach_stream;
use crate::daemon::config::DaemonConfig;
use crate::daemon::emergency_reserve::EmergencyReserve;
use crate::daemon::handlers::{self, DaemonState};
use crate::daemon::pipe_attach_stream;
use crate::daemon::pipe_sessions::PipeSessionRegistry;
use crate::daemon::pty_sessions::PtySessionRegistry;
use crate::daemon::reaper;
use crate::daemon::registry::Registry;
use crate::daemon::runtime_gc;

// ---------------------------------------------------------------------------
// DaemonServer
// ---------------------------------------------------------------------------

pub struct DaemonServer {
    state: Arc<DaemonState>,
    shutdown_rx: watch::Receiver<bool>,
}

impl DaemonServer {
    /// Create a new daemon server.
    ///
    /// `socket_path` is the IPC endpoint.  `db_path` is the SQLite tracking
    /// database.  `scope`, `scope_hash`, and `scope_cwd` describe the
    /// project scope the daemon manages.
    ///
    /// The registry is opened (and crash-recovered) from `db_path`.
    pub fn new(
        socket_path: String,
        db_path: String,
        scope: String,
        scope_hash: String,
        scope_cwd: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let registry = Arc::new(Registry::open(std::path::Path::new(&db_path))?);
        let pty_sessions = Arc::new(PtySessionRegistry::new());
        let pipe_sessions = Arc::new(PipeSessionRegistry::new());
        // #390: pre-allocate the ENOSPC delete-to-recover reserve next to
        // the SQLite db. Never fails startup — degraded mode is logged.
        let reserve_dir = std::path::Path::new(&db_path)
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let emergency_reserve = Arc::new(EmergencyReserve::initialize_in(&reserve_dir));

        // runpm services share the tracking SQLite db and write per-service
        // logs under <data>/services/ (#222 Phase 2).
        let services_log_dir = reserve_dir.join("services");
        let services = Arc::new(crate::daemon::services::ServiceRegistry::open(
            std::path::Path::new(&db_path),
            services_log_dir,
        )?);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = Arc::new(DaemonState {
            start_time: std::time::Instant::now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            socket_path,
            db_path,
            scope,
            scope_hash,
            scope_cwd,
            shutdown_tx,
            active_connections: std::sync::atomic::AtomicU32::new(0),
            registry,
            pty_sessions,
            pipe_sessions,
            services,
            emergency_reserve,
        });
        Ok(Self { state, shutdown_rx })
    }

    /// Signal all accept loops and connection handlers to stop.
    pub fn shutdown(&self) {
        let _ = self.state.shutdown_tx.send(true);
    }

    /// Run the IPC server, blocking until shutdown is signalled.
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let socket_path = &self.state.socket_path;

        // Platform-specific: create parent directory for Unix socket files.
        #[cfg(unix)]
        {
            if let Some(parent) = std::path::Path::new(socket_path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Remove stale socket file if present.
            let _ = std::fs::remove_file(socket_path);
        }

        let name = self.create_socket_name()?;

        let listener = ListenerOptions::new().name(name).create_tokio()?;

        // On Unix, set socket file permissions to owner-only (0o600).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(socket_path, perms)?;
        }

        info!("daemon listening on {}", socket_path);

        // Spawn the background reaper task.
        let config = DaemonConfig::load();
        if !config.autostart.is_empty() {
            spawn_autostart_sessions(&self.state, &config.autostart);
        }
        let reaper_state = Arc::clone(&self.state);
        let reaper_handle = tokio::spawn(reaper::reaper_loop(
            reaper_state,
            config.reaper_interval_secs,
        ));
        let runtime_gc_state = Arc::clone(&self.state);
        let runtime_gc_handle = tokio::spawn(runtime_gc::runtime_gc_loop(
            runtime_gc_state,
            config.runtime_gc_interval_secs,
            config.runtime_gc_stale_after_secs,
        ));
        // runpm service supervisor: watches supervised children and applies
        // the restart policy on unexpected exit (#222 Phase 2).
        let supervisor_state = Arc::clone(&self.state);
        let supervisor_handle = tokio::spawn(crate::daemon::services::supervisor_loop(
            supervisor_state,
            1,
        ));

        let mut shutdown_rx = self.shutdown_rx.clone();

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok(stream) => {
                            let peer_shutdown = self.shutdown_rx.clone();
                            let peer_state = Arc::clone(&self.state);
                            tokio::spawn(async move {
                                let reserve = Arc::clone(&peer_state.emergency_reserve);
                                if let Err(e) = handle_connection(stream, peer_shutdown, peer_state).await {
                                    warn!("connection handler error: {e}");
                                    // #390: a full disk surfaces here as an io
                                    // write error; release the reserve so
                                    // shutdown bookkeeping can still write.
                                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                                        reserve.release_if_enospc(io_err, "connection handler error");
                                    } else {
                                        reserve.release_if_disk_full_message(
                                            &e.to_string(),
                                            "connection handler error",
                                        );
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                            self.state
                                .emergency_reserve
                                .release_if_enospc(&e, "listener accept error");
                            // #199: intentional — back-off against a
                            // pathological accept failure that would
                            // otherwise spin the daemon's CPU at 100%.
                            // 50ms is a conventional "rate limit my
                            // error log" delay.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("shutdown signal received, stopping listener");
                        break;
                    }
                }
            }
        }

        // #130 M8: reap any surviving daemon-owned sessions before exiting.
        // Without this step, PTY and pipe children would either die from
        // Windows Job-Object KILL_ON_JOB_CLOSE (acceptable) or survive as
        // orphans on POSIX (not acceptable). Doing the explicit terminate
        // here makes the cleanup deterministic on both platforms.
        reap_all_sessions(&self.state).await;

        // Wait for the reaper task to finish (it watches the same shutdown signal).
        let _ = reaper_handle.await;
        let _ = runtime_gc_handle.await;
        let _ = supervisor_handle.await;

        // Cleanup socket file on Unix.
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(socket_path);
        }

        Ok(())
    }

    /// Convert the socket path string into an `interprocess::local_socket::Name`.
    ///
    /// On Windows, named pipes live in a namespace, so we use `ToNsName` with
    /// `GenericNamespaced`.  On Unix, sockets are filesystem paths, so we use
    /// `ToFsName` with `GenericFilePath`.
    fn create_socket_name(
        &self,
    ) -> Result<interprocess::local_socket::Name<'_>, Box<dyn std::error::Error>> {
        #[cfg(unix)]
        {
            use interprocess::local_socket::ToFsName;
            Ok(self
                .state
                .socket_path
                .as_str()
                .to_fs_name::<GenericFilePath>()?)
        }

        #[cfg(windows)]
        {
            use interprocess::local_socket::ToNsName;
            Ok(self
                .state
                .socket_path
                .as_str()
                .to_ns_name::<GenericNamespaced>()?)
        }
    }
}

// ---------------------------------------------------------------------------
// Autostart sessions on daemon startup (#130 M7 B3)
// ---------------------------------------------------------------------------

/// Spawn every `AutostartSession` declared in the daemon's TOML config.
/// Failures are logged at warn level so a single bad entry does not
/// prevent the daemon from starting; the rest of the autostart list
/// still runs.
pub fn spawn_autostart_sessions(
    state: &DaemonState,
    entries: &[crate::daemon::config::AutostartSession],
) {
    for entry in entries {
        if entry.argv.is_empty() {
            warn!("autostart entry has empty argv; skipping");
            continue;
        }

        // Build env overlay. Layer on daemon env unless clear_env.
        let env = if entry.env.is_empty() && !entry.clear_env {
            None
        } else {
            let mut pairs: Vec<(String, String)> = if entry.clear_env {
                Vec::new()
            } else {
                std::env::vars().collect()
            };
            for (k, v) in &entry.env {
                if let Some((_, existing)) = pairs.iter_mut().find(|(ek, _)| ek == k) {
                    *existing = v.clone();
                } else {
                    pairs.push((k.clone(), v.clone()));
                }
            }
            Some(pairs)
        };

        let originator = if entry.originator.is_empty() {
            "autostart".to_string()
        } else {
            entry.originator.clone()
        };
        let command_display = entry.argv.join(" ");

        match entry.kind.as_str() {
            "pty" => {
                let rows = if entry.rows == 0 { 24 } else { entry.rows };
                let cols = if entry.cols == 0 { 80 } else { entry.cols };
                match state.pty_sessions.spawn(
                    entry.argv.clone(),
                    entry.cwd.clone(),
                    env,
                    rows,
                    cols,
                    originator,
                    command_display.clone(),
                ) {
                    Ok(s) => info!(
                        "autostart: spawned PTY session {} pid={} cmd={:?}",
                        s.id, s.pid, command_display
                    ),
                    Err(e) => warn!(
                        "autostart: failed to spawn PTY session cmd={:?}: {e}",
                        command_display
                    ),
                }
            }
            other => {
                if other != "pipe" {
                    warn!(
                        "autostart: unknown kind {other:?}, defaulting to pipe (cmd={:?})",
                        command_display
                    );
                }
                match state.pipe_sessions.spawn(
                    entry.argv.clone(),
                    entry.cwd.clone(),
                    env,
                    originator,
                    command_display.clone(),
                    entry.merge_stderr,
                ) {
                    Ok(s) => info!(
                        "autostart: spawned pipe session {} pid={} cmd={:?}",
                        s.id, s.pid, command_display
                    ),
                    Err(e) => warn!(
                        "autostart: failed to spawn pipe session cmd={:?}: {e}",
                        command_display
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session reap on daemon shutdown (#130 M8)
// ---------------------------------------------------------------------------

/// Terminate every PTY and pipe session the daemon currently owns. Called
/// once after the accept loop stops on shutdown so the daemon does not
/// leak orphaned child processes on POSIX. On Windows the Job Object
/// kill-on-close fires anyway as the daemon exits, but the explicit
/// terminate makes the cleanup observable in tests and consistent across
/// platforms.
async fn reap_all_sessions(state: &DaemonState) {
    let mut pids_to_wait = Vec::new();

    for session in state.pty_sessions.list() {
        if session.exit_state().is_some() {
            continue;
        }
        if let Err(e) = session.process.kill_tree_impl() {
            warn!(session_id = %session.id, error = %e, "kill_tree on shutdown failed");
        }
        if session.pid != 0 {
            pids_to_wait.push(session.pid);
        }
    }
    for session in state.pipe_sessions.list() {
        if session.exit_state().is_some() {
            continue;
        }
        if let Err(e) = session.process.kill() {
            warn!(session_id = %session.id, error = %e, "process.kill on shutdown failed");
        }
        if session.pid != 0 {
            pids_to_wait.push(session.pid);
        }
    }

    if pids_to_wait.is_empty() {
        return;
    }
    info!(
        "reaping {} sessions on shutdown ({} PIDs)",
        pids_to_wait.len(),
        pids_to_wait.len()
    );

    // #199: removed — this 150ms wait was a "tests benefit from it"
    // crutch, not a correctness requirement. Tests that need to
    // observe child-exit propagation after Shutdown should
    // themselves wait + retry rather than relying on the daemon
    // sleeping. Saves 150ms on every clean shutdown.
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// Idle timeout for waiting on the next frame from a client.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum frame size (4 MiB).
const MAX_FRAME_LENGTH: usize = 4 * 1024 * 1024;

async fn handle_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    mut shutdown_rx: watch::Receiver<bool>,
    state: Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Track this connection.
    state.active_connections.fetch_add(1, Ordering::Relaxed);

    let result = handle_connection_inner(stream, &mut shutdown_rx, &state).await;

    // Always decrement on exit, regardless of success/failure.
    state.active_connections.fetch_sub(1, Ordering::Relaxed);

    result
}

async fn handle_connection_inner(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    shutdown_rx: &mut watch::Receiver<bool>,
    state: &Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let codec = LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_type::<u32>()
        .max_frame_length(MAX_FRAME_LENGTH)
        .new_codec();

    let mut framed = Framed::new(stream, codec);

    loop {
        // Check for shutdown before blocking on read.
        if *shutdown_rx.borrow() {
            debug!("connection closing due to shutdown");
            break;
        }

        let frame: bytes::BytesMut = tokio::select! {
            result = timeout(IDLE_TIMEOUT, framed.next()) => {
                match result {
                    Ok(Some(Ok(bytes))) => bytes,
                    Ok(Some(Err(e))) => {
                        // Layer 1: frame decode error.
                        warn!("frame decode error: {e}");
                        let resp = error_response(0, StatusCode::InvalidArgument, format!("frame decode error: {e}"));
                        let _ = send_response(&mut framed, &resp).await;
                        break;
                    }
                    Ok(None) => {
                        // Client disconnected cleanly.
                        debug!("client disconnected");
                        break;
                    }
                    Err(_) => {
                        // Idle timeout.
                        debug!("connection idle timeout");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("connection closing due to shutdown");
                break;
            }
        };

        // Layer 2: protobuf decode.
        let request = match DaemonRequest::decode(frame.as_ref()) {
            Ok(req) => req,
            Err(e) => {
                warn!("protobuf decode error: {e}");
                let resp = error_response(
                    0,
                    StatusCode::InvalidArgument,
                    format!("protobuf decode error: {e}"),
                );
                let _ = send_response(&mut framed, &resp).await;
                continue;
            }
        };

        let request_id = request.id;

        // Intercept ATTACH_PTY_SESSION: the request switches the connection
        // into streaming mode for the rest of its lifetime, so we hand the
        // framed transport off to the streaming handler and return when it
        // finishes. The handler is responsible for sending the response.
        if RequestType::try_from(request.r#type) == Ok(RequestType::AttachPtySession) {
            let attach_req = request.attach_pty_session.clone().unwrap_or_default();
            let state_arc = Arc::clone(state);
            if let Err(e) =
                attach_stream::run_attach_stream(framed, request_id, attach_req, state_arc).await
            {
                warn!("attach stream ended with error: {e}");
            }
            return Ok(());
        }

        if RequestType::try_from(request.r#type) == Ok(RequestType::AttachPipeStream) {
            let attach_req = request.attach_pipe_stream.clone().unwrap_or_default();
            let state_arc = Arc::clone(state);
            if let Err(e) = pipe_attach_stream::run_pipe_attach_stream(
                framed, request_id, attach_req, state_arc,
            )
            .await
            {
                warn!("pipe attach stream ended with error: {e}");
            }
            return Ok(());
        }

        // Layer 4: catch panics around the dispatch.
        let response = match catch_unwind(AssertUnwindSafe(|| dispatch_request(&request, state))) {
            Ok(future) => future.await,
            Err(_) => {
                error!("panic in request handler for request_id={request_id}");
                error_response(
                    request_id,
                    StatusCode::Internal,
                    "internal server error: handler panicked".into(),
                )
            }
        };

        if let Err(e) = send_response(&mut framed, &response).await {
            warn!("failed to send response for request_id={request_id}: {e}");
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Layer 3: dispatch based on `RequestType`.
///
/// Request types dispatch to their concrete handlers.
fn dispatch_request(
    request: &DaemonRequest,
    state: &DaemonState,
) -> impl Future<Output = DaemonResponse> + Send + 'static {
    let request_id = request.id;
    let request_type = request.r#type;

    // Try to decode the request type enum.
    let response = match RequestType::try_from(request_type) {
        Ok(RequestType::Unspecified) => error_response(
            request_id,
            StatusCode::UnknownRequest,
            "unspecified request type".into(),
        ),
        Ok(RequestType::Ping) => handlers::handle_ping(request, state),
        Ok(RequestType::Status) => handlers::handle_status(request, state),
        Ok(RequestType::Shutdown) => handlers::handle_shutdown(request, state),
        Ok(RequestType::Register) => handlers::handle_register(request, state),
        Ok(RequestType::Unregister) => handlers::handle_unregister(request, state),
        Ok(RequestType::SpawnDaemon) => handlers::handle_spawn_daemon(request, state),
        Ok(RequestType::ListActive) => handlers::handle_list_active(request, state),
        Ok(RequestType::ListByOriginator) => handlers::handle_list_by_originator(request, state),
        Ok(RequestType::GetProcessTree) => handlers::handle_get_process_tree(request, state),
        Ok(RequestType::KillTree) => handlers::handle_kill_tree(request, state),
        Ok(RequestType::KillZombies) => handlers::handle_kill_zombies(request, state),
        Ok(RequestType::ServiceStart) => handlers::handle_service_start(request, state),
        Ok(RequestType::ServiceStop) => handlers::handle_service_stop(request, state),
        Ok(RequestType::ServiceRestart) => handlers::handle_service_restart(request, state),
        Ok(RequestType::ServiceDelete) => handlers::handle_service_delete(request, state),
        Ok(RequestType::ServiceList) => handlers::handle_service_list(request, state),
        Ok(RequestType::ServiceDescribe) => handlers::handle_service_describe(request, state),
        Ok(RequestType::ServiceLogs) => handlers::handle_service_logs(request, state),
        Ok(RequestType::ServiceFlush) => handlers::handle_service_flush(request, state),
        Ok(RequestType::ServiceSave) => handlers::handle_service_save(request, state),
        Ok(RequestType::ServiceResurrect) => handlers::handle_service_resurrect(request, state),
        Ok(RequestType::SpawnPtySession) => handlers::handle_spawn_pty_session(request, state),
        Ok(RequestType::AttachPtySession) => handlers::handle_attach_pty_session(request, state),
        Ok(RequestType::DetachPtySession) => handlers::handle_detach_pty_session(request, state),
        Ok(RequestType::ListPtySessions) => handlers::handle_list_pty_sessions(request, state),
        Ok(RequestType::TerminatePtySession) => {
            handlers::handle_terminate_pty_session(request, state)
        }
        Ok(RequestType::SpawnPipeSession) => handlers::handle_spawn_pipe_session(request, state),
        Ok(RequestType::AttachPipeStream) => handlers::handle_attach_pipe_stream(request, state),
        Ok(RequestType::DetachPipeStream) => handlers::handle_detach_pipe_stream(request, state),
        Ok(RequestType::ListPipeSessions) => handlers::handle_list_pipe_sessions(request, state),
        Ok(RequestType::TerminatePipeSession) => {
            handlers::handle_terminate_pipe_session(request, state)
        }
        Ok(RequestType::WritePipeStdin) => handlers::handle_write_pipe_stdin(request, state),
        Ok(RequestType::GetSessionBacklog) => handlers::handle_get_session_backlog(request, state),
        Ok(RequestType::PurgeExitedSessions) => {
            handlers::handle_purge_exited_sessions(request, state)
        }
        Ok(RequestType::BulkTerminateSessions) => {
            handlers::handle_bulk_terminate_sessions(request, state)
        }
        Ok(RequestType::ResizePtySession) => handlers::handle_resize_pty_session(request, state),
        Ok(RequestType::RegisterSessionTee) => {
            handlers::handle_register_session_tee(request, state)
        }
        Ok(RequestType::UnregisterSessionTee) => {
            handlers::handle_unregister_session_tee(request, state)
        }
        Ok(RequestType::GetSessionTeeStatus) => {
            handlers::handle_get_session_tee_status(request, state)
        }
        Ok(RequestType::RegisterSessionObserver) => {
            handlers::handle_register_session_observer(request, state)
        }
        Ok(RequestType::UnregisterSessionObserver) => {
            handlers::handle_unregister_session_observer(request, state)
        }
        Ok(RequestType::GetSessionObserverStatus) => {
            handlers::handle_get_session_observer_status(request, state)
        }
        Err(_) => error_response(
            request_id,
            StatusCode::UnknownRequest,
            format!("unknown request type: {request_type}"),
        ),
    };

    // Return a ready future so the signature is uniform for when real
    // async handlers are added later.
    std::future::ready(response)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode and send a `DaemonResponse` over the framed transport.
async fn send_response<T>(
    framed: &mut Framed<T, LengthDelimitedCodec>,
    response: &DaemonResponse,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let encoded = response.encode_to_vec();
    framed.send(Bytes::from(encoded)).await?;
    Ok(())
}

/// Build an error `DaemonResponse` with no payload.
fn error_response(request_id: u64, code: StatusCode, message: String) -> DaemonResponse {
    DaemonResponse {
        request_id,
        code: code.into(),
        message,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::watch;

    fn test_state() -> (DaemonState, tempfile::TempDir) {
        let (shutdown_tx, _rx) = watch::channel(false);
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let db_path = tmp_dir.path().join("test-server.db");
        let registry = Arc::new(Registry::open(&db_path).unwrap());
        let pty_sessions = Arc::new(crate::daemon::pty_sessions::PtySessionRegistry::new());
        let pipe_sessions = Arc::new(crate::daemon::pipe_sessions::PipeSessionRegistry::new());
        let services = Arc::new(
            crate::daemon::services::ServiceRegistry::open(
                &db_path,
                tmp_dir.path().join("services"),
            )
            .unwrap(),
        );
        let state = DaemonState {
            start_time: Instant::now(),
            version: "0.0.0-test".to_string(),
            socket_path: "/tmp/test.sock".to_string(),
            db_path: db_path.display().to_string(),
            scope: "global".to_string(),
            scope_hash: "0000000000000000".to_string(),
            scope_cwd: "/tmp".to_string(),
            shutdown_tx,
            active_connections: AtomicU32::new(0),
            registry,
            pty_sessions,
            pipe_sessions,
            services,
            emergency_reserve: Arc::new(EmergencyReserve::initialize_at(
                tmp_dir.path().join("emergency-reserve.bin"),
                4096,
            )),
        };
        (state, tmp_dir)
    }

    #[tokio::test]
    async fn dispatch_request_rejects_unspecified_request_type() {
        let (state, _tmp) = test_state();
        let request = DaemonRequest {
            id: 77,
            r#type: RequestType::Unspecified as i32,
            protocol_version: 1,
            client_name: "test".to_string(),
            ..Default::default()
        };

        let response = dispatch_request(&request, &state).await;

        assert_eq!(response.request_id, 77);
        assert_eq!(response.code, StatusCode::UnknownRequest as i32);
        assert_eq!(response.message, "unspecified request type");
    }

    /// PTY-session handlers (#130 milestone 2): missing-payload requests
    /// must reach the handler (not return UNKNOWN_REQUEST from dispatch)
    /// and report INVALID_ARGUMENT — the dispatch table is correctly
    /// routing the new request types. The full handler behaviour is
    /// exercised by `tests/pty_session_attach_test.rs`.
    #[tokio::test]
    async fn pty_session_handlers_route_via_dispatcher() {
        let (state, _tmp) = test_state();

        // Handlers that take a payload return INVALID_ARGUMENT when called
        // with no payload — that proves the dispatcher delivered the
        // request and the handler ran.
        let payload_required = [
            RequestType::SpawnPtySession,
            RequestType::DetachPtySession,
            RequestType::TerminatePtySession,
            RequestType::GetSessionBacklog,
            RequestType::RegisterSessionTee,
            RequestType::UnregisterSessionTee,
            RequestType::GetSessionTeeStatus,
            RequestType::RegisterSessionObserver,
            RequestType::UnregisterSessionObserver,
            RequestType::GetSessionObserverStatus,
        ];
        for (i, rt) in payload_required.iter().enumerate() {
            let request = DaemonRequest {
                id: 100 + i as u64,
                r#type: *rt as i32,
                protocol_version: 1,
                client_name: "test".to_string(),
                ..Default::default()
            };
            let response = dispatch_request(&request, &state).await;
            assert_eq!(response.request_id, 100 + i as u64);
            assert_eq!(
                response.code,
                StatusCode::InvalidArgument as i32,
                "{rt:?} should reach handler and report INVALID_ARGUMENT for missing payload; got code={} msg={:?}",
                response.code,
                response.message,
            );
        }

        // ListPtySessions has no required payload fields; a default
        // request returns OK with an empty list.
        let list_request = DaemonRequest {
            id: 200,
            r#type: RequestType::ListPtySessions as i32,
            protocol_version: 1,
            client_name: "test".to_string(),
            ..Default::default()
        };
        let list_response = dispatch_request(&list_request, &state).await;
        assert_eq!(list_response.code, StatusCode::Ok as i32);
        let payload = list_response
            .list_pty_sessions
            .expect("list response has payload");
        assert!(payload.sessions.is_empty());

        // AttachPtySession is intercepted by the streaming server before
        // it reaches `dispatch_request`. Calling the dispatcher directly
        // exercises the stub which returns INTERNAL to make accidental
        // direct dispatch loud.
        let attach_request = DaemonRequest {
            id: 300,
            r#type: RequestType::AttachPtySession as i32,
            protocol_version: 1,
            client_name: "test".to_string(),
            ..Default::default()
        };
        let attach_response = dispatch_request(&attach_request, &state).await;
        assert_eq!(attach_response.code, StatusCode::Internal as i32);
    }
}
