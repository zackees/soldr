//! Synchronous IPC client for the running-process daemon.
//!
//! Connects to the daemon over a local socket (Unix domain socket on
//! Linux/macOS, named pipe on Windows) and exchanges length-prefixed protobuf
//! messages.

use crate::client::paths;
use crate::proto::daemon::{
    BulkTerminateSessionsRequest, BulkTerminateSessionsResponse, DaemonRequest, DaemonResponse,
    GetProcessTreeRequest, GetSessionBacklogRequest, GetSessionBacklogResponse, KeyValue,
    KillTreeRequest, KillZombiesRequest, ListActiveRequest, ListByOriginatorRequest, PingRequest,
    PipeStreamKind, PurgeExitedSessionsRequest, PurgeExitedSessionsResponse, RequestType,
    ResizePtySessionRequest, ServiceConfig, ServiceDeleteRequest, ServiceDescribeRequest,
    ServiceFlushRequest, ServiceListRequest, ServiceLogsRequest, ServiceRestartRequest,
    ServiceResurrectRequest, ServiceSaveRequest, ServiceStartRequest, ServiceStopRequest,
    ShutdownRequest, SpawnDaemonRequest as ProtoSpawnDaemonRequest, StatusCode, StatusRequest,
};
use interprocess::local_socket::Stream;
use interprocess::TryClone;
use prost::Message;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by [`DaemonClient`] operations.
#[derive(Debug)]
pub enum ClientError {
    /// Failed to connect to the daemon socket.
    Connect(std::io::Error),
    /// I/O error during send or receive.
    Io(std::io::Error),
    /// Failed to decode a protobuf response.
    Decode(prost::DecodeError),
    /// The daemon returned an application-level error response.
    Server {
        /// Application-level status code returned by the daemon.
        code: StatusCode,
        /// Human-readable daemon error message.
        message: String,
    },
    /// The daemon is not running and could not be started.
    DaemonNotRunning,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Connect(e) => write!(f, "failed to connect to daemon: {e}"),
            ClientError::Io(e) => write!(f, "daemon I/O error: {e}"),
            ClientError::Decode(e) => write!(f, "failed to decode daemon response: {e}"),
            ClientError::Server { code, message } => {
                write!(f, "daemon returned {:?}: {}", code, message)
            }
            ClientError::DaemonNotRunning => write!(f, "daemon is not running"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Connect(e) | ClientError::Io(e) => Some(e),
            ClientError::Decode(e) => Some(e),
            ClientError::Server { .. } | ClientError::DaemonNotRunning => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn API
// ---------------------------------------------------------------------------

/// Request to spawn a detached daemonized shell command under daemon control.
#[derive(Debug, Clone)]
pub struct SpawnCommandRequest {
    /// Shell command line to execute.
    pub command: String,
    /// Working directory for the spawned command.
    pub cwd: Option<PathBuf>,
    /// Environment key/value pairs sent with the request.
    pub env: Vec<(String, String)>,
    /// Caller-provided originator used for tracking and filtering.
    pub originator: Option<String>,
    /// When `true`, the daemon clears the inherited env before applying
    /// [`Self::env`], so the subprocess sees ONLY the supplied map.
    /// Mirrors Python's `subprocess.Popen(env=…)` replace semantic.
    /// Default `false` keeps the historic "layer on top of inherited"
    /// behaviour.
    pub clear_inherited_env: bool,
}

impl SpawnCommandRequest {
    fn default_originator() -> String {
        let caller = std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "running-process-client".to_string());
        format!("{caller}:{}", std::process::id())
    }

    /// Build a shell-command request using the caller's current working
    /// directory and environment.
    pub fn shell(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: std::env::current_dir().ok(),
            env: std::env::vars().collect(),
            originator: Some(Self::default_originator()),
            clear_inherited_env: false,
        }
    }

    /// Override the working directory used for the spawned command.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Replace the environment block sent to the daemon (layered on top
    /// of the daemon's inherited env, unless [`Self::with_env_replace`]
    /// is used instead).
    pub fn with_envs<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self
    }

    /// Set the env block AND tell the daemon to clear the inherited
    /// env first — the subprocess will see ONLY the supplied map.
    ///
    /// Mirrors Python's `subprocess.Popen(env=…)` semantic:
    ///
    /// ```python
    /// subprocess.Popen(["..."], env=None)        # inherits
    /// subprocess.Popen(["..."], env={"K": "V"})  # replaces
    /// ```
    ///
    /// On Windows you typically still want to include `SystemRoot` in
    /// the supplied map so `cmd.exe` can load its DLLs.
    pub fn with_env_replace<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self.clear_inherited_env = true;
        self
    }

    /// Add or replace a single environment variable while keeping the rest
    /// of the existing environment block intact.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        let value = value.into();
        if let Some((_, existing)) = self
            .env
            .iter_mut()
            .find(|(existing_key, _)| *existing_key == key)
        {
            *existing = value;
        } else {
            self.env.push((key, value));
        }
        self
    }

    /// Set the originator value stored in the daemon registry and injected
    /// into the spawned child environment.
    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = Some(originator.into());
        self
    }
}

/// Information about a daemonized process spawned by the service.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnedDaemon {
    /// Operating-system process identifier of the spawned daemon.
    pub pid: u32,
    /// Daemon-side creation timestamp in Unix seconds.
    pub created_at: f64,
    /// Shell command registered for the spawned daemon.
    pub command: String,
    /// Working directory reported for the spawned daemon.
    pub cwd: Option<String>,
    /// Originator recorded for the spawned daemon.
    pub originator: Option<String>,
    /// Containment mechanism used by the daemon for this process.
    pub containment: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Synchronous IPC client that communicates with the daemon over a local socket.
///
/// Messages are framed with a 4-byte big-endian length prefix followed by
/// a protobuf-encoded payload.
pub struct DaemonClient {
    // Raw nonblocking streams (not BufReader/BufWriter): `send_request`
    // drives deadline-bounded reads/writes via `deadline_io` so a stalled
    // or crashed-mid-reply daemon can't wedge the caller (issue #590, B1).
    reader: Stream,
    writer: Stream,
    next_id: AtomicU64,
}

impl DaemonClient {
    /// Connect to a running daemon identified by an optional scope hash.
    ///
    /// The socket path is computed by [`paths::socket_path`] and the name type
    /// dispatch matches the server via [`paths::make_socket_name`].
    pub fn connect(scope_hash: Option<&str>) -> Result<Self, ClientError> {
        let path = paths::socket_path(scope_hash);
        Self::connect_to(&path)
    }

    /// Connect to a daemon listening at an explicit socket path.
    ///
    /// Use this when you already know the socket path (e.g. in integration
    /// tests that start a server on a unique path).
    pub fn connect_to(socket_path: &str) -> Result<Self, ClientError> {
        // Validate the name up front so a bad path keeps its own error,
        // then connect with a bounded timeout (issue #590, cluster B) so a
        // bound-but-never-accepting daemon socket can't wedge the caller.
        paths::make_socket_name(socket_path).map_err(ClientError::Connect)?;
        let stream = crate::client::deadline_io::connect_with_timeout(socket_path)
            .map_err(ClientError::Connect)?;
        let stream_clone = stream.try_clone().map_err(ClientError::Connect)?;
        // Nonblocking on both handles so `send_request`'s deadline-bounded
        // reads/writes work. On Unix `try_clone` shares the file
        // description (so O_NONBLOCK carries), but on Windows each handle's
        // mode is independent — set both explicitly.
        use interprocess::local_socket::traits::Stream as _;
        stream.set_nonblocking(true).map_err(ClientError::Connect)?;
        stream_clone
            .set_nonblocking(true)
            .map_err(ClientError::Connect)?;

        Ok(Self {
            reader: stream,
            writer: stream_clone,
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a request and wait for the corresponding response.
    ///
    /// The request is length-prefixed (4-byte big-endian u32) then protobuf-encoded.
    /// The response uses the same framing.
    pub fn send_request(&mut self, request: DaemonRequest) -> Result<DaemonResponse, ClientError> {
        use crate::client::deadline_io::{
            read_frame_with_deadline, rpc_read_deadline, write_all_with_deadline,
        };

        // Frame: 4-byte big-endian length prefix + protobuf payload.
        let payload = request.encode_to_vec();
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        framed.extend_from_slice(&payload);

        // Bound the whole round-trip (issue #590, cluster B1): a daemon that
        // accepts then stalls or crashes mid-reply must not wedge the
        // Python-facing caller. `read_frame_with_deadline` also applies the
        // `MAX_FRAME_BYTES` cap before allocating the response buffer.
        let deadline = rpc_read_deadline();
        write_all_with_deadline(&mut self.writer, &framed, deadline).map_err(ClientError::Io)?;
        let resp_buf =
            read_frame_with_deadline(&mut self.reader, deadline).map_err(ClientError::Io)?;

        DaemonResponse::decode(&resp_buf[..]).map_err(ClientError::Decode)
    }

    // -----------------------------------------------------------------------
    // Convenience helpers
    // -----------------------------------------------------------------------

    /// Allocate the next request ID.
    pub(crate) fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn ensure_ok(&self, response: &DaemonResponse) -> Result<(), ClientError> {
        if response.code == StatusCode::Ok as i32 {
            return Ok(());
        }

        let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
        Err(ClientError::Server {
            code,
            message: response.message.clone(),
        })
    }

    /// Ping the daemon to check liveness.
    pub fn ping(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::Ping.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            ping: Some(PingRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Ask the daemon to shut down.
    pub fn shutdown(
        &mut self,
        graceful: bool,
        timeout_seconds: f64,
    ) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::Shutdown.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            shutdown: Some(ShutdownRequest {
                graceful,
                timeout_seconds,
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Query daemon status.
    pub fn status(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::Status.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            status: Some(StatusRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// List all active tracked processes.
    pub fn list_active(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ListActive.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            list_active: Some(ListActiveRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// List tracked processes filtered by originator tool name.
    pub fn list_by_originator(&mut self, tool: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ListByOriginator.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            list_by_originator: Some(ListByOriginatorRequest {
                tool: tool.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Kill zombie processes tracked by the daemon.
    pub fn kill_zombies(&mut self, dry_run: bool) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::KillZombies.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            kill_zombies: Some(KillZombiesRequest { dry_run }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Kill a process tree rooted at `pid`.
    pub fn kill_tree(
        &mut self,
        pid: u32,
        timeout_seconds: f64,
    ) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::KillTree.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            kill_tree: Some(KillTreeRequest {
                pid,
                timeout_seconds,
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Get the process tree display for a given PID.
    pub fn get_process_tree(&mut self, pid: u32) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::GetProcessTree.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            get_process_tree: Some(GetProcessTreeRequest { pid }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Ask the daemon to spawn and track a detached shell command.
    pub fn spawn_command(
        &mut self,
        request: &SpawnCommandRequest,
    ) -> Result<SpawnedDaemon, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::SpawnDaemon.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            spawn_daemon: Some(ProtoSpawnDaemonRequest {
                command: request.command.clone(),
                cwd: request
                    .cwd
                    .as_ref()
                    .map(|cwd| cwd.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                env: request
                    .env
                    .iter()
                    .map(|(k, v)| KeyValue {
                        key: k.clone(),
                        value: v.clone(),
                    })
                    .collect(),
                originator: request.originator.clone().unwrap_or_default(),
                clear_inherited_env: request.clear_inherited_env,
            }),
            ..Default::default()
        };

        let response = self.send_request(daemon_request)?;
        self.ensure_ok(&response)?;

        let payload = response.spawn_daemon.ok_or_else(|| ClientError::Server {
            code: StatusCode::Internal,
            message: "spawn response missing payload".to_string(),
        })?;

        Ok(SpawnedDaemon {
            pid: payload.pid,
            created_at: payload.created_at,
            command: payload.command,
            cwd: if payload.cwd.is_empty() {
                None
            } else {
                Some(payload.cwd)
            },
            originator: if payload.originator.is_empty() {
                None
            } else {
                Some(payload.originator)
            },
            containment: payload.containment,
        })
    }

    // --- service supervision (runpm) — Phase 1 ---

    /// Start a supervised service from a [`ServiceConfig`].
    pub fn service_start(&mut self, config: ServiceConfig) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceStart.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_start: Some(ServiceStartRequest {
                config: Some(config),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Stop a supervised service identified by name, id, or `"all"`.
    pub fn service_stop(&mut self, target: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceStop.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_stop: Some(ServiceStopRequest {
                target: target.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Restart a supervised service identified by name, id, or `"all"`.
    pub fn service_restart(&mut self, target: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceRestart.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_restart: Some(ServiceRestartRequest {
                target: target.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Delete a supervised service from the registry.
    pub fn service_delete(&mut self, target: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceDelete.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_delete: Some(ServiceDeleteRequest {
                target: target.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// List all supervised services known to the daemon.
    pub fn service_list(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceList.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_list: Some(ServiceListRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Describe a single supervised service in detail.
    pub fn service_describe(&mut self, target: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceDescribe.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_describe: Some(ServiceDescribeRequest {
                target: target.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Fetch buffered log output for a supervised service.
    pub fn service_logs(
        &mut self,
        target: &str,
        lines: u32,
        follow: bool,
    ) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceLogs.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_logs: Some(ServiceLogsRequest {
                target: target.to_string(),
                lines,
                follow,
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Flush buffered logs for a supervised service.
    pub fn service_flush(&mut self, target: &str) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceFlush.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_flush: Some(ServiceFlushRequest {
                target: target.to_string(),
            }),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Persist the current set of supervised services to a snapshot.
    pub fn service_save(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceSave.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_save: Some(ServiceSaveRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Restore supervised services from the most recent snapshot.
    pub fn service_resurrect(&mut self) -> Result<DaemonResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ServiceResurrect.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            service_resurrect: Some(ServiceResurrectRequest {}),
            ..Default::default()
        };
        self.send_request(request)
    }

    /// Resize a PTY session without going through an attach
    /// (#130 M5 follow-up). The new size persists for the lifetime of
    /// the session; subsequent attaches can override it via their own
    /// rows/cols fields.
    pub fn resize_pty_session(
        &mut self,
        session_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ResizePtySession.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            resize_pty_session: Some(ResizePtySessionRequest {
                session_id: session_id.into(),
                rows: rows as u32,
                cols: cols as u32,
            }),
            ..Default::default()
        };
        let response = self.send_request(request)?;
        if response.code != StatusCode::Ok as i32 {
            let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
            return Err(ClientError::Server {
                code,
                message: response.message,
            });
        }
        Ok(())
    }

    /// Purge exited sessions from both daemon-side registries (#130 M9
    /// H4). Returns counts of PTY and pipe sessions reaped.
    pub fn purge_exited_sessions(
        &mut self,
        originator: &str,
    ) -> Result<PurgeExitedSessionsResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::PurgeExitedSessions.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            purge_exited_sessions: Some(PurgeExitedSessionsRequest {
                originator: originator.into(),
            }),
            ..Default::default()
        };
        let response = self.send_request(request)?;
        if response.code != StatusCode::Ok as i32 {
            let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
            return Err(ClientError::Server {
                code,
                message: response.message,
            });
        }
        response
            .purge_exited_sessions
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "purge_exited_sessions response missing payload".into(),
            })
    }

    /// Schedule termination of every session older than the threshold
    /// (#130 M9 H4). `older_than_secs=0` terminates everything in scope.
    pub fn bulk_terminate_sessions(
        &mut self,
        older_than_secs: u64,
        originator: &str,
        grace_ms: u32,
    ) -> Result<BulkTerminateSessionsResponse, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::BulkTerminateSessions.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            bulk_terminate_sessions: Some(BulkTerminateSessionsRequest {
                older_than_secs,
                originator: originator.into(),
                grace_ms,
            }),
            ..Default::default()
        };
        let response = self.send_request(request)?;
        if response.code != StatusCode::Ok as i32 {
            let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
            return Err(ClientError::Server {
                code,
                message: response.message,
            });
        }
        response
            .bulk_terminate_sessions
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "bulk_terminate_sessions response missing payload".into(),
            })
    }

    /// Snapshot a PTY or pipe session's output backlog without consuming
    /// it. For pipe sessions, `pipe_stream` selects between stdout and
    /// stderr (default stdout). For PTY sessions `pipe_stream` is ignored.
    /// Returns `None` when the session is not found.
    pub fn get_session_backlog(
        &mut self,
        session_id: &str,
        pipe_stream: PipeStreamKind,
    ) -> Result<Option<GetSessionBacklogResponse>, ClientError> {
        let request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::GetSessionBacklog.into(),
            protocol_version: 1,
            client_name: String::from("running-process-client"),
            get_session_backlog: Some(GetSessionBacklogRequest {
                session_id: session_id.into(),
                pipe_stream: pipe_stream as i32,
            }),
            ..Default::default()
        };
        let response = self.send_request(request)?;
        if response.code == StatusCode::NotFound as i32 {
            return Ok(None);
        }
        if response.code != StatusCode::Ok as i32 {
            let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
            return Err(ClientError::Server {
                code,
                message: response.message,
            });
        }
        Ok(response.get_session_backlog)
    }
}

// ---------------------------------------------------------------------------
// Auto-start logic
// ---------------------------------------------------------------------------

/// Connect to the daemon, starting it first if it is not running.
///
/// 1. Attempt to connect.
/// 2. On failure, spawn `running-process-daemon start` as a detached process.
/// 3. Retry with exponential back-off: 50 ms, 100 ms, 200 ms, 400 ms.
/// 4. Return an error if the daemon cannot be reached after all retries.
pub fn connect_or_start(scope_hash: Option<&str>) -> Result<DaemonClient, ClientError> {
    // Fast path: daemon already running.
    if let Ok(client) = DaemonClient::connect(scope_hash) {
        return Ok(client);
    }

    // Spawn the daemon as a detached background process.
    spawn_daemon()?;

    // Retry with exponential back-off.
    //
    // #199: intentional — the daemon binds its socket asynchronously
    // after `spawn_daemon()` returns. There's no event the OS can
    // signal us with when the socket is ready, so we poll. Exponential
    // back-off (50→100→200→400ms) is the standard pattern; total
    // wait caps at 750ms.
    let delays_ms: [u64; 4] = [50, 100, 200, 400];
    for delay in delays_ms {
        std::thread::sleep(std::time::Duration::from_millis(delay));
        if let Ok(client) = DaemonClient::connect(scope_hash) {
            return Ok(client);
        }
    }

    Err(ClientError::DaemonNotRunning)
}

/// Launch a detached shell command through the running-process daemon.
///
/// The daemon owns process tracking after launch, so this helper returns as
/// soon as the child has been spawned and registered.
pub fn launch_detached(command: &str) -> Result<SpawnedDaemon, ClientError> {
    let mut client = connect_or_start(None)?;
    client.spawn_command(&SpawnCommandRequest::shell(command))
}

/// Convenience helper that connects to the daemon and asks it to daemonize
/// the provided shell command under the caller's current cwd/environment.
///
/// Prefer [`launch_detached`] in new code; this name is kept for existing
/// callers.
pub fn daemonize_command(command: &str) -> Result<SpawnedDaemon, ClientError> {
    launch_detached(command)
}

/// Spawn the daemon binary as a detached background process.
fn spawn_daemon() -> Result<(), ClientError> {
    let exe = daemon_exe_path();
    let mut command = std::process::Command::new(&exe);
    command.arg("start");
    crate::spawn_daemon(&mut command).map_err(ClientError::Io)?;
    Ok(())
}

/// Determine the path to the daemon executable.
///
/// Looks next to the current executable first, then falls back to expecting
/// it on `$PATH`.
fn daemon_exe_path() -> String {
    if let Ok(mut path) = std::env::current_exe() {
        path.pop(); // remove current binary name
        let candidate = path.join(if cfg!(windows) {
            "running-process-daemon.exe"
        } else {
            "running-process-daemon"
        });
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    // Fallback: assume it is on PATH.
    String::from("running-process-daemon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_detached_has_public_sync_signature() {
        let _api: fn(&str) -> Result<SpawnedDaemon, ClientError> = launch_detached;
    }

    #[test]
    fn spawn_command_request_builder_sets_detached_launch_context() {
        let request = SpawnCommandRequest::shell("echo hello")
            .with_cwd("work")
            .with_envs([("A", "1")])
            .with_env("B", "2")
            .with_originator("tool:123");

        assert_eq!(request.command, "echo hello");
        assert_eq!(request.cwd.as_deref(), Some(std::path::Path::new("work")));
        assert_eq!(
            request.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ]
        );
        assert_eq!(request.originator.as_deref(), Some("tool:123"));
    }
}
