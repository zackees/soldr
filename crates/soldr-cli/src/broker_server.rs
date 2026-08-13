//! Soldr's single stable broker listener (#2476).
//!
//! Admin, readiness, Hello negotiation, route progress, and SESSION bytes all
//! share one accepted connection. After negotiation the broker attempts the
//! optimized handoff path; the portable baseline keeps that same connection
//! and transparently proxies it to the selected daemon endpoint.

use interprocess::local_socket::traits::tokio::Listener as _;
use prost::Message;
use running_process::broker::protocol::{
    hello_reply, AdminRequest, AdminVerb, ErrorCode, Frame, FrameKind, Hello, HelloReply,
    PayloadEncoding, Refused, ADMIN_PAYLOAD_PROTOCOL, CONTROL_PAYLOAD_PROTOCOL, ENVELOPE_VERSION,
    MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
use running_process::broker::server::{
    AdminSnapshot, BackendRegistry, BrokerInstanceKey, CombinedServiceDefinitionLoader,
    FdPressureGuard, HelloResponder, HelloRouter, PeerCredentialPolicy, ServeHandoffContext,
    SpawnCoordinator,
};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const BROKER_INSTANCE_ID_ENV: &str = "SOLDR_INTERNAL_BROKER_INSTANCE_ID";

pub(crate) fn broker_image_instance_id() -> io::Result<String> {
    let executable = std::env::current_exe()?;
    let cache = crate::daemon::service_definition::broker_owned_paths()
        .cache
        .join("broker-image-hash");
    let digest = crate::daemon::image_hash::cached_blake3_hex(&cache, &executable)?;
    Ok(format_broker_instance_id(
        env!("CARGO_PKG_VERSION"),
        &digest,
    ))
}

fn broker_server_instance_id() -> io::Result<String> {
    if let Some(instance_id) = std::env::var(BROKER_INSTANCE_ID_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Ok(instance_id);
    }
    broker_image_instance_id()
}

fn format_broker_instance_id(version: &str, image_digest: &str) -> String {
    format!("soldr-{version}-{image_digest}")
}

/// Protobuf lane for route-start progress events (`RP` in ASCII).
pub(crate) const ROUTE_PROGRESS_PAYLOAD_PROTOCOL: u32 = 0x5250;
/// Soldr-private admin tunnel (`SC` in ASCII). The request selects an existing
/// verified daemon route; after a broker restart it may re-adopt a persisted
/// claim, but it never launches a missing backend.
pub(crate) const DAEMON_CONTROL_PAYLOAD_PROTOCOL: u32 = 0x5343;
#[cfg(target_os = "linux")]
const BROKER_LISTEN_BACKLOG: i32 = 1024;
const DEFAULT_FIRST_RESPONSE_MS: u64 = 2_000;
const DEFAULT_PROGRESS_SILENCE_MS: u64 = 5_000;
const DEFAULT_ROUTE_CEILING_MS: u64 = 120_000;
const DEFAULT_BUSY_BUDGET_MS: u64 = 1_000;

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RouteProgress {
    #[prost(string, tag = "1")]
    pub stage: String,
    #[prost(uint32, tag = "2")]
    pub attempt: u32,
    #[prost(uint64, tag = "3")]
    pub elapsed_ms: u64,
    #[prost(string, tag = "4")]
    pub latest_result: String,
    #[prost(uint64, tag = "5")]
    pub retry_after_ms: u64,
}

/// Encoded in `Hello.peer_attestation_nonce`. The field was reserved for a
/// challenge-response payload, so carrying a protobuf attestation is additive
/// and leaves the frozen Hello field numbering intact.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ClientHostAttestation {
    #[prost(string, tag = "1")]
    pub machine_id: String,
    #[prost(string, tag = "2")]
    pub boot_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct DaemonControlTunnelRequest {
    #[prost(string, tag = "1")]
    pub service_name: String,
    #[prost(string, tag = "2")]
    pub machine_id: String,
    #[prost(string, tag = "3")]
    pub boot_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct DaemonControlTunnelReply {
    #[prost(bool, tag = "1")]
    pub accepted: bool,
    #[prost(string, tag = "2")]
    pub error_detail: String,
    #[prost(bool, tag = "3")]
    pub not_running: bool,
}

pub(crate) fn client_host_attestation() -> Vec<u8> {
    let identity = running_process::broker::host_identity::current();
    ClientHostAttestation {
        machine_id: identity.machine_id,
        boot_id: identity.boot_id,
    }
    .encode_to_vec()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BrokerDeadlines {
    pub(crate) busy_budget: Duration,
    pub(crate) first_response: Duration,
    pub(crate) progress_silence: Duration,
    pub(crate) route_ceiling: Duration,
}

impl BrokerDeadlines {
    pub(crate) fn from_env() -> Self {
        Self {
            busy_budget: env_duration("SOLDR_BROKER_BUSY_BUDGET_MS", DEFAULT_BUSY_BUDGET_MS),
            first_response: env_duration(
                "SOLDR_BROKER_FIRST_RESPONSE_MS",
                DEFAULT_FIRST_RESPONSE_MS,
            ),
            progress_silence: env_duration(
                "SOLDR_BROKER_PROGRESS_SILENCE_MS",
                DEFAULT_PROGRESS_SILENCE_MS,
            ),
            route_ceiling: env_duration("SOLDR_ROUTE_ACQUIRE_CEILING_MS", DEFAULT_ROUTE_CEILING_MS),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct DoctorBrokerDeadline {
    pub(crate) name: &'static str,
    pub(crate) env_var: &'static str,
    pub(crate) default_ms: u64,
    pub(crate) effective_ms: u64,
    pub(crate) source: &'static str,
}

pub(crate) fn doctor_deadlines() -> Vec<DoctorBrokerDeadline> {
    let effective = BrokerDeadlines::from_env();
    [
        (
            "broker busy retry",
            "SOLDR_BROKER_BUSY_BUDGET_MS",
            DEFAULT_BUSY_BUDGET_MS,
            effective.busy_budget,
        ),
        (
            "broker first response",
            "SOLDR_BROKER_FIRST_RESPONSE_MS",
            DEFAULT_FIRST_RESPONSE_MS,
            effective.first_response,
        ),
        (
            "broker progress silence",
            "SOLDR_BROKER_PROGRESS_SILENCE_MS",
            DEFAULT_PROGRESS_SILENCE_MS,
            effective.progress_silence,
        ),
        (
            "broker route ceiling",
            "SOLDR_ROUTE_ACQUIRE_CEILING_MS",
            DEFAULT_ROUTE_CEILING_MS,
            effective.route_ceiling,
        ),
    ]
    .into_iter()
    .map(
        |(name, env_var, default_ms, duration)| DoctorBrokerDeadline {
            name,
            env_var,
            default_ms,
            effective_ms: duration.as_millis() as u64,
            source: match std::env::var(env_var) {
                Ok(value) if value.trim().parse::<u64>().is_ok_and(|value| value > 0) => "override",
                Ok(_) => "default (override ignored: expected positive milliseconds)",
                Err(_) => "default",
            },
        },
    )
    .collect()
}

pub(crate) fn print_doctor_deadlines() {
    println!("\nbroker route deadlines:");
    for row in doctor_deadlines() {
        println!(
            "  {:<24} {:>7} ms  [{} via {}]",
            row.name, row.effective_ms, row.source, row.env_var
        );
    }
}

fn env_duration(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default_ms),
    )
}

fn route_progress_heartbeat_interval(progress_silence: Duration) -> Duration {
    (progress_silence / 3)
        .max(Duration::from_millis(1))
        .min(Duration::from_secs(1))
}

struct BrokerState {
    instance_id: String,
    loader: CombinedServiceDefinitionLoader,
    registry: Mutex<BackendRegistry>,
    spawn_coordinator: Mutex<SpawnCoordinator>,
    launcher: crate::broker_launcher::SoldrBackendLauncher,
    started_at: Instant,
    connections_open: AtomicU64,
    fd_guard: FdPressureGuard,
}

impl BrokerState {
    fn route(
        &self,
        frame: Frame,
        peer: running_process::broker::server::PeerIdentity,
    ) -> HelloReply {
        let router = HelloRouter::with_lifecycle_monitor(&self.loader, &self.registry)
            .with_spawn_coordinator(&self.spawn_coordinator)
            .with_backend_launcher(&self.launcher);
        router.handle_frame(frame, peer)
    }

    fn snapshot(&self) -> AdminSnapshot {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        AdminSnapshot::from_registry(
            &self.instance_id,
            self.started_at.elapsed(),
            true,
            self.connections_open.load(Ordering::Relaxed),
            &registry,
            &[],
        )
        .with_fd_pressure_demoted(self.fd_guard.is_demoted())
    }

    fn instance_for_route(
        &self,
        service_name: &str,
        service_version: &str,
        backend_pipe: &str,
    ) -> Option<BrokerInstanceKey> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = registry.iter().find_map(|(key, handle)| {
            (key.service_name == service_name
                && key.service_version == service_version
                && handle.daemon_process.ipc_endpoint.path == backend_pipe)
                .then(|| key.instance.clone())
        });
        instance
    }

    fn private_control_endpoint_for_service(&self, service_name: &str) -> Option<String> {
        let debug = std::env::var_os("SOLDR_BROKER_DEBUG").is_some();
        let started = Instant::now();
        {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.prune_stale();
            let endpoint = registry.iter().find_map(|(key, handle)| {
                (key.service_name == service_name).then(|| {
                    private_control_endpoint_from_session(&handle.daemon_process.ipc_endpoint.path)
                })
            });
            if let Some(endpoint) = endpoint {
                if debug {
                    eprintln!(
                        "soldr broker: control route {service_name} found in registry endpoint={endpoint}"
                    );
                }
                return Some(endpoint);
            }
        }

        if debug {
            eprintln!("soldr broker: re-adopting control route {service_name}");
        }

        let adopted = match self
            .launcher
            .adopt_existing_control_route(&self.loader, service_name)
        {
            Ok(adopted) => adopted,
            Err(error) => {
                if std::env::var_os("SOLDR_BROKER_DEBUG").is_some() {
                    eprintln!(
                        "soldr broker: control route {service_name} could not be re-adopted: {error}"
                    );
                }
                None
            }
        }?;
        let endpoint =
            private_control_endpoint_from_session(&adopted.1.daemon_process.ipc_endpoint.path);
        if debug {
            eprintln!(
                "soldr broker: control route {service_name} re-adopted endpoint={endpoint} elapsed={:?}",
                started.elapsed()
            );
        }
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(adopted.0, adopted.1);
        Some(endpoint)
    }
}

fn private_control_endpoint_from_session(session_endpoint: &str) -> String {
    crate::daemon::session_endpoint::private_control_endpoint_from_session(session_endpoint)
}

struct OpenConnection(Arc<BrokerState>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.connections_open.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn serve(endpoint: &crate::broker_identity::ResolvedBrokerEndpoint) -> io::Result<()> {
    let endpoint = endpoint.clone();
    std::thread::Builder::new()
        .name("soldr-broker-main".into())
        .spawn(move || serve_on_runtime_thread(&endpoint))
        .map_err(|error| {
            io::Error::other(format!("could not spawn broker runtime thread: {error}"))
        })?
        .join()
        .map_err(|_| io::Error::other("broker runtime thread panicked"))?
}

fn serve_on_runtime_thread(
    endpoint: &crate::broker_identity::ResolvedBrokerEndpoint,
) -> io::Result<()> {
    endpoint
        .create_owner_only_directories()
        .map_err(io::Error::other)?;
    if let Some(diagnostic) = endpoint.fallback_diagnostic() {
        eprintln!("soldr broker: {diagnostic}");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("soldr-broker")
        .build()?;
    let peer_policy = PeerCredentialPolicy::current_user()
        .ok_or_else(|| io::Error::other("current-user broker peer policy is unavailable"))?;
    let state = Arc::new(BrokerState {
        instance_id: broker_server_instance_id()?,
        loader: CombinedServiceDefinitionLoader::new(
            running_process::broker::server::service_definition_dir(),
        ),
        registry: Mutex::new(BackendRegistry::new()),
        spawn_coordinator: Mutex::new(SpawnCoordinator::new()),
        launcher: crate::broker_launcher::SoldrBackendLauncher::new(),
        started_at: Instant::now(),
        connections_open: AtomicU64::new(0),
        fd_guard: FdPressureGuard::default(),
    });
    let runtime_context = runtime.enter();
    let listener = bind_listener(endpoint)?;
    drop(runtime_context);
    println!(
        "soldr broker: stable endpoint bound at {} (admin+hello+session)",
        endpoint.bind_endpoint
    );
    runtime.block_on(serve_loop(
        listener,
        state,
        Arc::new(peer_policy),
        endpoint.bind_endpoint.clone(),
    ))
}

async fn serve_loop(
    listener: interprocess::local_socket::tokio::Listener,
    state: Arc<BrokerState>,
    peer_policy: Arc<PeerCredentialPolicy>,
    endpoint: String,
) -> io::Result<()> {
    #[cfg(windows)]
    let _ = endpoint;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            accepted = listener.accept() => {
                let stream = accepted?;
                let peer = match running_process::broker::server::connection::peer_identity_from_tokio_stream(&stream) {
                    Ok(peer) => peer,
                    Err(error) => {
                        eprintln!("soldr broker: rejected peer without credentials: {error}");
                        continue;
                    }
                };
                if !peer_policy.allows(&peer) {
                    eprintln!("soldr broker: rejected foreign peer pid={}", peer.pid);
                    continue;
                }
                state.connections_open.fetch_add(1, Ordering::Relaxed);
                let state = Arc::clone(&state);
                let shutdown = Arc::clone(&shutdown);
                connections.spawn(async move {
                    let _open = OpenConnection(Arc::clone(&state));
                    if let Err(error) = handle_connection(stream, peer, state, shutdown).await {
                        eprintln!("soldr broker: connection ended: {error}");
                    }
                });
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    eprintln!("soldr broker: connection task failed: {error}");
                }
            }
        }
    }

    // Stop admission, then let already-negotiated same-connection proxies
    // drain. A bounded stop command may still terminate this process if a
    // client never closes.
    drop(listener);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&endpoint);
    while let Some(joined) = connections.join_next().await {
        if let Err(error) = joined {
            eprintln!("soldr broker: draining connection task failed: {error}");
        }
    }
    Ok(())
}

async fn handle_connection(
    mut stream: interprocess::local_socket::tokio::Stream,
    peer: running_process::broker::server::PeerIdentity,
    state: Arc<BrokerState>,
    shutdown: Arc<tokio::sync::Notify>,
) -> io::Result<()> {
    let deadlines = BrokerDeadlines::from_env();
    let body = tokio::time::timeout(deadlines.first_response, read_frame_async(&mut stream))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "broker first frame deadline exceeded",
            )
        })??;
    let request = Frame::decode(body.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    if request.payload_protocol == DAEMON_CONTROL_PAYLOAD_PROTOCOL {
        return handle_daemon_control_tunnel(stream, request, state).await;
    }

    if request.payload_protocol == ADMIN_PAYLOAD_PROTOCOL {
        let admin = AdminRequest::decode(request.payload.as_slice())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let response =
            running_process::broker::server::admin::handle_admin_frame(request, &state.snapshot())
                .map_err(io::Error::other)?;
        write_frame_async(&mut stream, &response).await?;
        if AdminVerb::try_from(admin.verb) == Ok(AdminVerb::Shutdown) {
            shutdown.notify_waiters();
        }
        return Ok(());
    }

    if request.payload_protocol != CONTROL_PAYLOAD_PROTOCOL {
        let reply = refused(
            ErrorCode::ErrorPeerRejected,
            "the first broker frame must be Hello or admin",
            0,
        );
        write_hello_reply(&mut stream, &request, &reply).await?;
        return Ok(());
    }
    if let Some(reply) = validate_client_host(&request) {
        write_hello_reply(&mut stream, &request, &reply).await?;
        return Ok(());
    }
    let hello = Hello::decode(request.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    let route_started = Instant::now();
    let route_ceiling = tokio::time::Instant::now() + deadlines.route_ceiling;
    state.launcher.note_route_deadline(
        &hello.service_name,
        std::time::Instant::now() + deadlines.route_ceiling,
    );
    let mut observed_progress = state.launcher.subscribe_progress();
    let mut latest_route_result = "broker accepted the route request".to_string();
    let heartbeat_interval = route_progress_heartbeat_interval(deadlines.progress_silence);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_interval,
        heartbeat_interval,
    );
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    write_progress(
        &mut stream,
        &request,
        RouteProgress {
            stage: "route-request".into(),
            attempt: 1,
            elapsed_ms: 0,
            latest_result: latest_route_result.clone(),
            retry_after_ms: 0,
        },
    )
    .await?;
    let mut attempt = 0_u32;
    let reply = 'acquire: loop {
        attempt = attempt.saturating_add(1);
        let route_state = Arc::clone(&state);
        let route_frame = request.clone();
        let route_peer = peer.clone();
        let mut route =
            tokio::task::spawn_blocking(move || route_state.route(route_frame, route_peer));
        let reply = loop {
            tokio::select! {
                joined = &mut route => {
                    break joined.map_err(|error| io::Error::other(format!("route worker failed: {error}")))?;
                }
                progress = observed_progress.recv() => {
                    match progress {
                        Ok(progress) if progress.service_name == hello.service_name => {
                            latest_route_result = progress.latest_result.clone();
                            write_progress(&mut stream, &request, RouteProgress {
                                stage: progress.stage.into(),
                                attempt,
                                elapsed_ms: route_started.elapsed().as_millis() as u64,
                                latest_result: progress.latest_result,
                                retry_after_ms: 0,
                            }).await?;
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err(io::Error::other("broker launcher progress channel closed"));
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    write_progress(&mut stream, &request, RouteProgress {
                        stage: "route-wait".into(),
                        attempt,
                        elapsed_ms: route_started.elapsed().as_millis() as u64,
                        latest_result: latest_route_result.clone(),
                        retry_after_ms: 0,
                    }).await?;
                }
                _ = tokio::time::sleep_until(route_ceiling) => {
                    route.abort();
                    break 'acquire refused(
                        ErrorCode::ErrorBackendSpawnFailed,
                        format!(
                            "route acquisition exceeded its hard ceiling after {} attempts; latest result: {}",
                            attempt, latest_route_result
                        ),
                        0,
                    );
                }
            }
        };
        let Some((latest_result, retry_after_ms)) = retryable_route_refusal(&reply) else {
            break reply;
        };
        latest_route_result = latest_result.clone();
        let delay = route_retry_delay(retry_after_ms);
        write_progress(
            &mut stream,
            &request,
            RouteProgress {
                stage: "single-flight-wait".into(),
                attempt,
                elapsed_ms: route_started.elapsed().as_millis() as u64,
                latest_result,
                retry_after_ms: delay.as_millis() as u64,
            },
        )
        .await?;
        let retry_deadline = tokio::time::Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(retry_deadline) => break,
                _ = heartbeat.tick() => {
                    write_progress(&mut stream, &request, RouteProgress {
                        stage: "single-flight-wait".into(),
                        attempt,
                        elapsed_ms: route_started.elapsed().as_millis() as u64,
                        latest_result: latest_route_result.clone(),
                        retry_after_ms: retry_deadline
                            .saturating_duration_since(tokio::time::Instant::now())
                            .as_millis() as u64,
                    }).await?;
                }
                _ = tokio::time::sleep_until(route_ceiling) => {
                    break 'acquire refused(
                        ErrorCode::ErrorBackendSpawnFailed,
                        format!(
                            "route acquisition exceeded its hard ceiling after {} attempts; latest result: {}",
                            attempt, latest_route_result
                        ),
                        0,
                    );
                }
            }
        }
    };
    write_hello_reply(&mut stream, &request, &reply).await?;
    let Some(hello_reply::Result::Negotiated(negotiated)) = reply.result.as_ref() else {
        return Ok(());
    };
    if negotiated.backend_pipe.is_empty() {
        return Err(io::Error::other("negotiated route has no daemon endpoint"));
    }

    if try_direct_handoff(&mut stream, &state, &hello.service_name, negotiated, &reply).await? {
        return Ok(());
    }

    write_handoff_fallback(&mut stream, &request, negotiated).await?;

    // Portable fallback: no reconnect. The exact accepted stream that carried
    // Hello now carries SessionStart and the complete compile exchange.
    running_process::broker::session_relay::relay_session(stream, &negotiated.backend_pipe).await
}

async fn handle_daemon_control_tunnel(
    mut stream: interprocess::local_socket::tokio::Stream,
    request_frame: Frame,
    state: Arc<BrokerState>,
) -> io::Result<()> {
    let request = DaemonControlTunnelRequest::decode(request_frame.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let local = running_process::broker::host_identity::current();
    let requested_service = request.service_name.clone();
    let existing_endpoint =
        if request.machine_id == local.machine_id && request.boot_id == local.boot_id {
            tokio::task::spawn_blocking(move || {
                state.private_control_endpoint_for_service(&requested_service)
            })
            .await
            .map_err(|error| io::Error::other(format!("control adoption worker failed: {error}")))?
        } else {
            None
        };
    let (reply, endpoint) = if request.machine_id != local.machine_id
        || request.boot_id != local.boot_id
    {
        (
            DaemonControlTunnelReply {
                accepted: false,
                error_detail: "shared Soldr home points at a broker from another machine or boot; use a machine-local home".into(),
                not_running: false,
            },
            None,
        )
    } else if let Some(endpoint) = existing_endpoint {
        (
            DaemonControlTunnelReply {
                accepted: true,
                error_detail: String::new(),
                not_running: false,
            },
            Some(endpoint),
        )
    } else {
        (
            DaemonControlTunnelReply {
                accepted: false,
                error_detail: format!(
                    "daemon route {} is not running in the broker registry",
                    request.service_name
                ),
                not_running: true,
            },
            None,
        )
    };
    write_frame_async(
        &mut stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: DAEMON_CONTROL_PAYLOAD_PROTOCOL,
            payload: reply.encode_to_vec(),
            request_id: request_frame.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request_frame.traceparent,
            tracestate: request_frame.tracestate,
        },
    )
    .await?;
    let Some(endpoint) = endpoint else {
        return Ok(());
    };
    running_process::broker::session_relay::relay_session(stream, &endpoint).await
}

fn retryable_route_refusal(reply: &HelloReply) -> Option<(String, u64)> {
    let Some(hello_reply::Result::Refused(refused)) = reply.result.as_ref() else {
        return None;
    };
    (ErrorCode::try_from(refused.code) == Ok(ErrorCode::ErrorRateLimited)
        && refused.retry_after_ms > 0)
        .then(|| (refused.reason.clone(), refused.retry_after_ms))
}

fn route_retry_delay(retry_after_ms: u64) -> Duration {
    let ceiling = retry_after_ms.clamp(5, 1_000);
    let mut random = [0_u8; 8];
    let value = if getrandom::fill(&mut random).is_ok() {
        u64::from_le_bytes(random)
    } else {
        0
    };
    Duration::from_millis(5 + value % (ceiling - 4))
}

async fn write_handoff_fallback(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Frame,
    negotiated: &running_process::broker::protocol::Negotiated,
) -> io::Result<()> {
    use running_process::broker::capabilities::CAP_HANDLE_PASSING;

    if negotiated.server_capabilities & CAP_HANDLE_PASSING == 0
        || negotiated.handle_passed_token.is_empty()
    {
        return Ok(());
    }
    let ack = running_process::broker::protocol::HandoffAck {
        token: negotiated.handle_passed_token.clone(),
        accepted: false,
        error_detail: "broker retained the accepted connection for proxy fallback".into(),
        correlation_id: negotiated.connection_id,
    };
    write_frame_async(
        stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Event as i32,
            payload_protocol: running_process::broker::protocol::HANDOFF_PAYLOAD_PROTOCOL,
            payload: ack.encode_to_vec(),
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        },
    )
    .await
}

async fn try_direct_handoff(
    stream: &mut interprocess::local_socket::tokio::Stream,
    state: &Arc<BrokerState>,
    service_name: &str,
    negotiated: &running_process::broker::protocol::Negotiated,
    reply: &HelloReply,
) -> io::Result<bool> {
    if !direct_handoff_eligible(
        negotiated.server_capabilities,
        &negotiated.handle_passed_token,
    ) {
        return Ok(false);
    }

    #[cfg(debug_assertions)]
    if std::env::var_os("SOLDR_TEST_BROKER_DISABLE_HANDOFF").is_some() {
        return Ok(false);
    }
    let Some(instance) = state.instance_for_route(
        service_name,
        &negotiated.daemon_version,
        &negotiated.backend_pipe,
    ) else {
        return Ok(false);
    };
    let Ok(mut handoff_client) = duplicate_handoff_stream(stream) else {
        return Ok(false);
    };
    let state = Arc::clone(state);
    let service_name = service_name.to_string();
    let service_version = negotiated.daemon_version.clone();
    let handoff_endpoint =
        crate::daemon::session_endpoint::handoff_endpoint_path(&negotiated.backend_pipe);
    let reply = reply.clone();
    let transferred = tokio::task::spawn_blocking(move || {
        let context = ServeHandoffContext {
            handoff_endpoint: &handoff_endpoint,
            service_name: &service_name,
            service_version: &service_version,
            instance: &instance,
            registry: &state.registry,
        };
        running_process::broker::server::try_transfer_negotiated_handoff(
            &context,
            &mut handoff_client,
            &reply,
        )
    })
    .await
    .map_err(|error| io::Error::other(format!("broker handoff worker failed: {error}")))?;
    if !transferred {
        return Ok(false);
    }
    let ack = running_process::broker::protocol::HandoffAck {
        token: negotiated.handle_passed_token.clone(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: negotiated.connection_id,
    };
    write_handoff_ready_async(stream, &ack).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("backend accepted broker handoff but the ready event could not be delivered: {error}"),
        )
    })?;
    Ok(true)
}

fn direct_handoff_eligible(server_capabilities: u64, token: &[u8]) -> bool {
    use running_process::broker::capabilities::CAP_HANDLE_PASSING;

    // A duplicated Windows named-pipe handle can be converted and ACKed by
    // the daemon, yet the adopted SESSION receives ERROR_BROKEN_PIPE as soon
    // as the broker drops its original accepted handle. Keep Windows on the
    // same-connection relay, which preserves the protocol and ownership
    // contract without exposing a client-visible false-positive handoff.
    !cfg!(windows) && server_capabilities & CAP_HANDLE_PASSING != 0 && !token.is_empty()
}

async fn write_handoff_ready_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    ack: &running_process::broker::protocol::HandoffAck,
) -> io::Result<()> {
    let frame = running_process::broker::server::handoff_ready_frame(ack);
    write_frame_async(stream, &frame).await
}

#[cfg(unix)]
fn duplicate_handoff_stream(
    stream: &interprocess::local_socket::tokio::Stream,
) -> io::Result<interprocess::local_socket::Stream> {
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};

    let interprocess::local_socket::tokio::Stream::UdSocket(stream) = stream;
    // F_DUPFD_CLOEXEC prevents an accepted client connection from leaking
    // through any daemon/tool process the broker launches concurrently.
    let duplicated = unsafe { libc::fcntl(stream.as_fd().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(duplicated) };
    let stream = interprocess::os::unix::uds_local_socket::Stream::from(owned);
    Ok(stream.into())
}

#[cfg(windows)]
fn duplicate_handoff_stream(
    stream: &interprocess::local_socket::tokio::Stream,
) -> io::Result<interprocess::local_socket::Stream> {
    use std::os::windows::io::{AsHandle as _, AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let interprocess::local_socket::tokio::Stream::NamedPipe(stream) = stream;
    let process = unsafe { GetCurrentProcess() };
    let mut duplicated: HANDLE = std::ptr::null_mut();
    let result = unsafe {
        DuplicateHandle(
            process,
            stream.as_handle().as_raw_handle() as HANDLE,
            process,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(duplicated.cast()) };
    let stream = interprocess::os::windows::named_pipe::local_socket::Stream::try_from(owned)
        .map_err(|error| error.to_io_error())?;
    Ok(stream.into())
}

fn validate_client_host(request: &Frame) -> Option<HelloReply> {
    let hello = match Hello::decode(request.payload.as_slice()) {
        Ok(hello) => hello,
        Err(_) => {
            return Some(refused(
                ErrorCode::ErrorPeerRejected,
                "malformed broker Hello",
                0,
            ));
        }
    };
    if hello.peer_attestation_nonce.is_empty() {
        return Some(refused(
            ErrorCode::ErrorPeerRejected,
            "broker Hello omitted machine/boot attestation for the shared home",
            0,
        ));
    }
    let client = match ClientHostAttestation::decode(hello.peer_attestation_nonce.as_slice()) {
        Ok(attestation) => attestation,
        Err(_) => {
            return Some(refused(
                ErrorCode::ErrorPeerRejected,
                "soldr Hello carried a malformed machine/boot attestation",
                0,
            ));
        }
    };
    let local = running_process::broker::host_identity::current();
    if client.machine_id != local.machine_id || client.boot_id != local.boot_id {
        let mut reply = refused(
            ErrorCode::ErrorPeerRejected,
            "shared Soldr home points at a broker from another machine or boot; use a machine-local home",
            0,
        );
        if let Some(hello_reply::Result::Refused(ref mut detail)) = reply.result {
            detail
                .details
                .insert("client_machine_id".into(), client.machine_id);
            detail
                .details
                .insert("broker_machine_id".into(), local.machine_id);
            detail
                .details
                .insert("client_boot_id".into(), client.boot_id);
            detail
                .details
                .insert("broker_boot_id".into(), local.boot_id);
        }
        return Some(reply);
    }
    None
}

fn refused(code: ErrorCode, reason: impl Into<String>, retry_after_ms: u64) -> HelloReply {
    HelloReply {
        result: Some(hello_reply::Result::Refused(Refused {
            reason: reason.into(),
            daemon_min_protocol: PROTOCOL_VERSION,
            daemon_max_protocol: PROTOCOL_VERSION,
            code: code as i32,
            details: Default::default(),
            retry_after_ms,
        })),
    }
}

async fn write_progress(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Frame,
    progress: RouteProgress,
) -> io::Result<()> {
    write_frame_async(
        stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Event as i32,
            payload_protocol: ROUTE_PROGRESS_PAYLOAD_PROTOCOL,
            payload: progress.encode_to_vec(),
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        },
    )
    .await
}

async fn write_hello_reply(
    stream: &mut interprocess::local_socket::tokio::Stream,
    request: &Frame,
    reply: &HelloReply,
) -> io::Result<()> {
    write_frame_async(
        stream,
        &Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
            payload: reply.encode_to_vec(),
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        },
    )
    .await
}

async fn read_frame_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
) -> io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).await?;
    if header[0] != ENVELOPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported broker framing version",
        ));
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker frame exceeds the maximum size",
        ));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame_async(
    stream: &mut interprocess::local_socket::tokio::Stream,
    frame: &Frame,
) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let body = frame.encode_to_vec();
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "broker frame exceeds u32"))?;
    let mut header = [0_u8; 5];
    header[0] = ENVELOPE_VERSION;
    header[1..].copy_from_slice(&len.to_le_bytes());
    stream.write_all(&header).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

fn bind_listener(
    endpoint: &crate::broker_identity::ResolvedBrokerEndpoint,
) -> io::Result<interprocess::local_socket::tokio::Listener> {
    #[cfg(unix)]
    let _guard = UnixBindGuard::acquire(&endpoint.bind_endpoint)?;
    #[cfg(unix)]
    let listener = create_listener(&endpoint.bind_endpoint).or_else(|error| {
        if running_process::broker::server::singleton_bind::is_already_bound_error(&error)
            && running_process::broker::server::singleton_bind::unix_socket_path_is_stale(
                &endpoint.bind_endpoint,
            )
        {
            std::fs::remove_file(&endpoint.bind_endpoint)?;
            return create_listener(&endpoint.bind_endpoint);
        }
        Err(error)
    })?;
    #[cfg(windows)]
    let listener = create_listener(&endpoint.bind_endpoint)?;
    Ok(listener)
}

fn create_listener(endpoint: &str) -> io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::ListenerOptions;
    let name = running_process::broker::server::singleton_bind::wrap_socket_name(endpoint)
        .map_err(io::Error::other)?;
    let options = ListenerOptions::new().name(name);
    // interprocess can atomically apply a Unix socket mode on Linux, but its
    // fchmod-before-bind implementation is unsupported on macOS. The broker
    // process is single-purpose at this point; macOS creates under its normal
    // umask and we tighten the finished socket below before accepting peers.
    #[cfg(all(unix, not(target_os = "macos")))]
    let options = {
        use interprocess::os::unix::local_socket::ListenerOptionsExt as _;
        options.mode(0o600)
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let listener = options
            .create_tokio_as::<interprocess::os::unix::uds_local_socket::tokio::Listener>()?;
        if unsafe { libc::listen(listener.as_fd().as_raw_fd(), BROKER_LISTEN_BACKLOG) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(listener.into())
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let listener = options
            .create_tokio_as::<interprocess::os::unix::uds_local_socket::tokio::Listener>()?;
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(listener.into())
    }
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;

        // Named-pipe local sockets use FILE_FLAG_FIRST_PIPE_INSTANCE and
        // PIPE_REJECT_REMOTE_CLIENTS in interprocess. The protected DACL grants
        // generic-all only to the object owner (the current user); peer SID
        // validation above is the second layer on every accepted connection.
        let sddl = widestring::U16CString::from_str("D:P(A;;GA;;;OW)").map_err(io::Error::other)?;
        let descriptor = SecurityDescriptor::deserialize(&sddl).map_err(io::Error::other)?;
        options.security_descriptor(descriptor).create_tokio()
    }
}

#[cfg(unix)]
struct UnixBindGuard(std::fs::File);

#[cfg(unix)]
impl UnixBindGuard {
    fn acquire(endpoint: &str) -> io::Result<Self> {
        use fs2::FileExt as _;
        use std::os::unix::fs::PermissionsExt as _;
        let endpoint_path = std::path::Path::new(endpoint);
        let lock_path = endpoint_path
            .parent()
            .ok_or_else(|| io::Error::other("broker endpoint has no parent"))?
            .join("bind.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for UnixBindGuard {
    fn drop(&mut self) {
        use fs2::FileExt as _;
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(
        direct_handoff_eligibility_requires_platform_capability_and_token,
        {
            use running_process::broker::capabilities::CAP_HANDLE_PASSING;

            assert_eq!(
                direct_handoff_eligible(CAP_HANDLE_PASSING, &[1]),
                !cfg!(windows)
            );
            assert!(!direct_handoff_eligible(0, &[1]));
            assert!(!direct_handoff_eligible(CAP_HANDLE_PASSING, &[]));
        }
    );

    #[cfg(windows)]
    crate::timed_test!(windows_handoff_ready_uses_async_original_pipe, {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                use interprocess::local_socket::traits::tokio::Stream as _;

                let endpoint = format!(
                    "soldr-handoff-ready-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("clock")
                        .as_nanos()
                );
                let listener = create_listener(&endpoint).expect("named-pipe listener");
                let client_name =
                    running_process::broker::server::singleton_bind::wrap_socket_name(&endpoint)
                        .expect("named-pipe name");

                let server = async {
                    let mut stream = listener.accept().await.expect("accept client");
                    let duplicated = duplicate_handoff_stream(&stream)
                        .expect("duplicate overlapped named-pipe handle for handoff");
                    drop(duplicated);

                    let ack = running_process::broker::protocol::HandoffAck {
                        token: vec![1, 2, 3, 4],
                        accepted: true,
                        error_detail: String::new(),
                        correlation_id: 42,
                    };
                    write_handoff_ready_async(&mut stream, &ack)
                        .await
                        .expect("write ready event asynchronously on original pipe");
                };
                let client = async {
                    let mut stream =
                        interprocess::local_socket::tokio::Stream::connect(client_name)
                            .await
                            .expect("connect client");
                    let body = read_frame_async(&mut stream)
                        .await
                        .expect("read ready event");
                    let frame = Frame::decode(body.as_slice()).expect("decode ready frame");
                    let ack = running_process::broker::protocol::HandoffAck::decode(
                        frame.payload.as_slice(),
                    )
                    .expect("decode handoff acknowledgement");
                    assert!(ack.accepted);
                    assert_eq!(ack.correlation_id, 42);
                    assert_eq!(ack.token, vec![1, 2, 3, 4]);
                };

                tokio::join!(server, client);
            });
    });

    crate::timed_test!(
        broker_instance_identity_includes_the_complete_image_digest,
        {
            let first = format_broker_instance_id("0.9.0", &"a".repeat(64));
            let second = format_broker_instance_id("0.9.0", &"b".repeat(64));
            assert_ne!(first, second, "same-version images must not alias");
            assert!(first.ends_with(&"a".repeat(64)));
        }
    );

    crate::timed_test!(route_heartbeat_stays_inside_the_client_silence_budget, {
        assert_eq!(
            route_progress_heartbeat_interval(Duration::from_secs(5)),
            Duration::from_secs(1)
        );
        assert_eq!(
            route_progress_heartbeat_interval(Duration::from_millis(30)),
            Duration::from_millis(10)
        );
        assert!(
            route_progress_heartbeat_interval(Duration::from_millis(30))
                < Duration::from_millis(30)
        );
    });

    crate::timed_test!(progress_and_attestation_are_protobuf_roundtrips, {
        let progress = RouteProgress {
            stage: "spawn".into(),
            attempt: 3,
            elapsed_ms: 42,
            latest_result: "waiting".into(),
            retry_after_ms: 7,
        };
        assert_eq!(
            RouteProgress::decode(progress.encode_to_vec().as_slice()).unwrap(),
            progress
        );
        let bytes = client_host_attestation();
        let attestation = ClientHostAttestation::decode(bytes.as_slice()).unwrap();
        assert!(!attestation.machine_id.is_empty());
        assert!(!attestation.boot_id.is_empty());
    });

    crate::timed_test!(
        deadline_env_values_are_positive_and_have_contract_defaults,
        {
            let deadlines = BrokerDeadlines::from_env();
            assert!(!deadlines.first_response.is_zero());
            assert!(!deadlines.progress_silence.is_zero());
            assert!(!deadlines.route_ceiling.is_zero());
        }
    );

    crate::timed_test!(mismatched_machine_attestation_is_refused_as_shared_home, {
        let hello = Hello {
            client_lib_name: "soldr".into(),
            peer_attestation_nonce: ClientHostAttestation {
                machine_id: "another-machine".into(),
                boot_id: "another-boot".into(),
            }
            .encode_to_vec(),
            ..Default::default()
        };
        let request = Frame::request(CONTROL_PAYLOAD_PROTOCOL, hello.encode_to_vec());
        let reply = validate_client_host(&request).expect("foreign machine must be refused");
        let Some(hello_reply::Result::Refused(refused)) = reply.result else {
            panic!("expected refusal");
        };
        assert_eq!(
            ErrorCode::try_from(refused.code),
            Ok(ErrorCode::ErrorPeerRejected)
        );
        assert!(refused.reason.contains("shared Soldr home"));
        assert!(refused.details.contains_key("client_machine_id"));
        assert!(refused.details.contains_key("broker_machine_id"));
    });

    #[cfg(unix)]
    crate::timed_test!(stale_socket_n_way_bind_has_exactly_one_winner, {
        use std::sync::{mpsc, Barrier};

        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("soldr-broker.sock");
        let stale = std::os::unix::net::UnixListener::bind(&socket).expect("seed stale socket");
        drop(stale);
        let endpoint = crate::broker_identity::ResolvedBrokerEndpoint {
            executable_path: temp.path().join("soldr-broker"),
            logical_socket_path: socket.display().to_string(),
            bind_endpoint: socket.display().to_string(),
            windows_pipe_leaf: None,
            oversized_windows_pipe_leaf: None,
            fallback: None,
            lease_database_path: temp.path().join("lease.sqlite3"),
        };
        let contenders = 16;
        let release = Arc::new(Barrier::new(contenders + 1));
        let (send, receive) = mpsc::channel();
        let threads: Vec<_> = (0..contenders)
            .map(|_| {
                let endpoint = endpoint.clone();
                let release = Arc::clone(&release);
                let send = send.clone();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime");
                    let _context = runtime.enter();
                    let listener = bind_listener(&endpoint);
                    send.send(listener.is_ok()).expect("send result");
                    release.wait();
                    drop(listener);
                })
            })
            .collect();
        drop(send);
        let results: Vec<_> = receive.iter().take(contenders).collect();
        assert_eq!(results.iter().filter(|won| **won).count(), 1, "{results:?}");
        release.wait();
        for thread in threads {
            thread.join().expect("bind contender");
        }
    });

    #[cfg(target_os = "macos")]
    crate::timed_test!(macos_listener_restricts_socket_permissions_after_bind, {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let socket = temp.path().join("soldr-broker.sock");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _context = runtime.enter();
        let listener = create_listener(socket.to_str().expect("UTF-8 socket path"))
            .expect("macOS broker listener");
        let mode = std::fs::metadata(&socket)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
    });
}
