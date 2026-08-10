//! Service-definition-backed Hello routing.
//!
//! `HelloHandler` owns deterministic validation and in-memory negotiation.
//! `HelloRouter` adds the broker-facing lookup layer: reload the service
//! definition for each request, resolve the trust-domain instance, query the
//! backend registry, and then delegate the final reply construction to
//! `HelloHandler`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, HelloReply, Refused,
    ServiceDefinition, PROTOCOL_VERSION,
};
use crate::broker::server::{
    check_version_allowed, BackendKey, BackendLaunchRequest, BackendLauncher, BackendRegistry,
    BrokerInstanceKey, HelloHandler, HelloHandlerError, HelloRequest, PeerIdentity,
    RegisteredBackend, ServiceDefinitionError, ServiceDefinitionLoader, SpawnBeginError,
    SpawnCoordinator, SpawnOutcome, TraceContext, VersionPolicyBlock,
};

/// Routes decoded Hello requests through service definitions and backend state.
#[derive(Clone, Copy)]
pub struct HelloRouter<'a> {
    service_definitions: &'a ServiceDefinitionLoader,
    backends: BackendRegistryView<'a>,
    spawn_coordinator: Option<&'a Mutex<SpawnCoordinator>>,
    backend_launcher: Option<&'a dyn BackendLauncher>,
}

#[derive(Clone, Copy)]
enum BackendRegistryView<'a> {
    Static(&'a BackendRegistry),
    Live(&'a Mutex<BackendRegistry>),
}

impl<'a> HelloRouter<'a> {
    /// Create a router over immutable broker state.
    pub fn new(
        service_definitions: &'a ServiceDefinitionLoader,
        backends: &'a BackendRegistry,
    ) -> Self {
        Self {
            service_definitions,
            backends: BackendRegistryView::Static(backends),
            spawn_coordinator: None,
            backend_launcher: None,
        }
    }

    /// Create a router over live broker state that prunes stale backend handles
    /// before each registry lookup.
    pub fn with_lifecycle_monitor(
        service_definitions: &'a ServiceDefinitionLoader,
        backends: &'a Mutex<BackendRegistry>,
    ) -> Self {
        Self {
            service_definitions,
            backends: BackendRegistryView::Live(backends),
            spawn_coordinator: None,
            backend_launcher: None,
        }
    }

    /// Attach spawn-budget coordination for backend registry misses.
    pub fn with_spawn_coordinator(
        mut self,
        spawn_coordinator: &'a Mutex<SpawnCoordinator>,
    ) -> Self {
        self.spawn_coordinator = Some(spawn_coordinator);
        self
    }

    /// Attach a launcher used to satisfy verified backend registry misses.
    pub fn with_backend_launcher(mut self, backend_launcher: &'a dyn BackendLauncher) -> Self {
        self.backend_launcher = Some(backend_launcher);
        self
    }

    /// Decode and route a framed Hello request.
    pub fn handle_frame(&self, frame: Frame, peer: PeerIdentity) -> HelloReply {
        match HelloRequest::decode(frame, peer) {
            Ok(request) => self.handle_request(&request),
            Err(refused) => refused_reply(refused),
        }
    }

    /// Route a decoded Hello request.
    pub fn handle_request(&self, request: &HelloRequest) -> HelloReply {
        match self.route_request(request) {
            Ok(registered) => match HelloHandler::new().with_backend(registered) {
                Ok(handler) => handler.handle_request(request),
                Err(err) => refused_reply(refused_from_handler_error(err)),
            },
            Err(refused) => refused_reply(refused),
        }
    }

    fn route_request(&self, request: &HelloRequest) -> Result<RegisteredBackend, Refused> {
        let service_definition = self
            .service_definitions
            .lookup_or_reload(&request.hello.service_name)
            .map_err(refused_from_service_definition_error)?;

        if let Err(block) =
            check_version_allowed(&request.hello.wanted_version, &service_definition)
        {
            return Err(refused_from_version_policy(block));
        }

        let instance =
            BrokerInstanceKey::from_service_definition(&service_definition).map_err(|err| {
                refused(
                    ErrorCode::ErrorInternal,
                    format!("service isolation could not be resolved: {err}"),
                    0,
                )
            })?;

        if let Some(registered) = self.registered_backend_for(
            &instance,
            &service_definition,
            &request.hello.wanted_version,
        ) {
            return Ok(registered);
        }

        let key = BackendKey::new(
            instance,
            request.hello.service_name.clone(),
            request.hello.wanted_version.clone(),
        );
        let trace_context = request.trace_context();
        self.launch_backend(&key, &service_definition, &trace_context)
    }

    fn registered_backend_for(
        &self,
        instance: &BrokerInstanceKey,
        service_definition: &ServiceDefinition,
        service_version: &str,
    ) -> Option<RegisteredBackend> {
        match self.backends {
            BackendRegistryView::Static(registry) => {
                registry.registered_backend_for(instance, service_definition, service_version)
            }
            BackendRegistryView::Live(registry) => {
                let mut registry = registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let _removed = registry.prune_stale();
                registry.registered_backend_for(instance, service_definition, service_version)
            }
        }
    }

    fn launch_backend(
        &self,
        key: &BackendKey,
        service_definition: &ServiceDefinition,
        trace_context: &TraceContext,
    ) -> Result<RegisteredBackend, Refused> {
        self.begin_spawn(key.clone())?;

        let Some(backend_launcher) = self.backend_launcher else {
            self.finish_spawn(key, SpawnOutcome::Failed);
            return Err(refused(
                ErrorCode::ErrorBackendSpawnFailed,
                "backend is not registered",
                1_000,
            ));
        };

        let request = BackendLaunchRequest {
            key,
            service_definition,
            trace_context,
        };
        match backend_launcher.launch(&request) {
            Ok(handle) => match self.register_launched_backend(key, service_definition, handle) {
                Ok(registered) => {
                    self.finish_spawn(key, SpawnOutcome::Success);
                    Ok(registered)
                }
                Err(refused) => {
                    self.finish_spawn(key, SpawnOutcome::Failed);
                    Err(refused)
                }
            },
            Err(err) => {
                self.finish_spawn(key, SpawnOutcome::Failed);
                Err(refused(
                    ErrorCode::ErrorBackendSpawnFailed,
                    format!("backend spawn failed: {err}"),
                    1_000,
                ))
            }
        }
    }

    fn begin_spawn(&self, key: BackendKey) -> Result<(), Refused> {
        let Some(spawn_coordinator) = self.spawn_coordinator else {
            return Ok(());
        };

        let now = Instant::now();
        let mut coordinator = spawn_coordinator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match coordinator.try_begin(key.clone(), now) {
            Ok(_) => Ok(()),
            Err(SpawnBeginError::AlreadyInProgress) => Err(refused(
                ErrorCode::ErrorRateLimited,
                "backend spawn already in progress",
                1_000,
            )),
            Err(SpawnBeginError::BudgetExhausted { retry_after, .. }) => Err(refused(
                ErrorCode::ErrorRateLimited,
                "backend spawn budget exhausted",
                duration_to_retry_ms(retry_after),
            )),
        }
    }

    fn finish_spawn(&self, key: &BackendKey, outcome: SpawnOutcome) {
        let Some(spawn_coordinator) = self.spawn_coordinator else {
            return;
        };

        let mut coordinator = spawn_coordinator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        coordinator.finish(key, outcome, Instant::now());
    }

    fn register_launched_backend(
        &self,
        key: &BackendKey,
        service_definition: &ServiceDefinition,
        handle: crate::broker::backend_handle::BackendHandle,
    ) -> Result<RegisteredBackend, Refused> {
        if handle.service_name != key.service_name || handle.service_version != key.service_version
        {
            return Err(refused(
                ErrorCode::ErrorInternal,
                "launched backend identity did not match request",
                0,
            ));
        }

        let registered = RegisteredBackend {
            service_definition: service_definition.clone(),
            daemon_version: handle.service_version.clone(),
            backend_pipe: handle.daemon_process.ipc_endpoint.path.clone(),
            server_capabilities: 0,
        };

        if let BackendRegistryView::Live(registry) = self.backends {
            let mut registry = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.insert(key.instance.clone(), handle);
        }

        Ok(registered)
    }
}

fn refused_from_service_definition_error(error: ServiceDefinitionError) -> Refused {
    match error {
        ServiceDefinitionError::InvalidName(_) => {
            refused(ErrorCode::ErrorPeerRejected, "invalid service_name", 0)
        }
        ServiceDefinitionError::Io(err) if err.kind() == std::io::ErrorKind::NotFound => refused(
            ErrorCode::ErrorServiceUnknown,
            "service definition was not found",
            0,
        ),
        other => refused(
            ErrorCode::ErrorServiceUnknown,
            format!("service definition could not be loaded: {other}"),
            0,
        ),
    }
}

fn refused_from_version_policy(block: VersionPolicyBlock) -> Refused {
    match block {
        VersionPolicyBlock::BelowMinVersion => refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is below min_version",
            30_000,
        ),
        VersionPolicyBlock::OutsideAllowList => refused(
            ErrorCode::ErrorVersionBlocked,
            "wanted_version is not in version_allow_list",
            30_000,
        ),
    }
}

fn refused_from_handler_error(error: HelloHandlerError) -> Refused {
    refused(
        ErrorCode::ErrorInternal,
        format!("registered backend could not be installed: {error}"),
        0,
    )
}

fn refused(code: ErrorCode, reason: impl Into<String>, retry_after_ms: u64) -> Refused {
    Refused {
        reason: reason.into(),
        daemon_min_protocol: PROTOCOL_VERSION,
        daemon_max_protocol: PROTOCOL_VERSION,
        code: code as i32,
        details: HashMap::new(),
        retry_after_ms,
    }
}

fn duration_to_retry_ms(duration: Duration) -> u64 {
    let millis = duration.as_millis().max(1);
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn refused_reply(refused: Refused) -> HelloReply {
    HelloReply {
        result: Some(HelloReplyResult::Refused(refused)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use prost::Message;

    use crate::broker::backend_handle::{BackendHandle, DaemonProcess};
    use crate::broker::protocol::{
        BrokerIsolation, Endpoint, FrameKind, Hello, PayloadEncoding, ServiceDefinition,
    };
    use crate::broker::server::{
        ensure_service_definition_dir, service_definition_path, PeerIdentity,
    };

    use super::*;

    fn service_definition() -> ServiceDefinition {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().to_path_buf();
        ServiceDefinition {
            service_name: "zccache".into(),
            binary_path: exe.to_string_lossy().into_owned(),
            isolation: BrokerIsolation::SharedBroker as i32,
            explicit_instance: String::new(),
            per_version_binary_dir: dir.to_string_lossy().into_owned(),
            min_version: "1.10.0".into(),
            version_allow_list: vec!["1.11.20".into()],
            labels: Default::default(),
        }
    }

    fn service_dir_with_definition(definition: &ServiceDefinition) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("services");
        ensure_service_definition_dir(&root).unwrap();
        fs::write(
            service_definition_path(&root, "zccache").unwrap(),
            definition.encode_to_vec(),
        )
        .unwrap();
        tmp
    }

    fn request() -> HelloRequest {
        let hello = Hello {
            client_min_protocol: 1,
            client_max_protocol: 1,
            service_name: "zccache".into(),
            wanted_version: "1.11.20".into(),
            client_version: "zccache-cli/1.11.20".into(),
            client_capabilities: 0,
            auth_token: Vec::new(),
            request_id: "req-live-prune".into(),
            connection_id: 0,
            peer_pid: 0,
            client_lib_name: "running-process".into(),
            client_lib_version: env!("CARGO_PKG_VERSION").into(),
            peer_attestation_nonce: Vec::new(),
            capability_token: Vec::new(),
            client_keepalive_secs: 60,
        };
        HelloRequest {
            frame: Frame {
                envelope_version: 1,
                kind: FrameKind::Request as i32,
                payload_protocol: 0,
                payload: hello.encode_to_vec(),
                request_id: 1,
                payload_encoding: PayloadEncoding::None as i32,
                deadline_unix_ms: 0,
                traceparent: String::new(),
                tracestate: String::new(),
            },
            hello,
            peer: PeerIdentity {
                pid: 0,
                uid_or_sid: "test-peer".into(),
            },
        }
    }

    fn stale_backend_handle() -> BackendHandle {
        let endpoint = Endpoint {
            namespace_id: "shared".into(),
            path: "rpb-v1-test-stale-backend".into(),
        };
        let mut daemon = DaemonProcess::current_process(endpoint, Some(30)).unwrap();
        daemon.pid = u32::MAX;
        BackendHandle {
            service_name: "zccache".into(),
            service_version: "1.11.20".into(),
            daemon_process: daemon,
            #[cfg(unix)]
            pid_handle: None,
            #[cfg(windows)]
            process_handle: None,
        }
    }

    #[test]
    fn live_registry_prunes_stale_backend_before_routing() {
        let definition = service_definition();
        let tmp = service_dir_with_definition(&definition);
        let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
        let mut registry = BackendRegistry::new();
        registry.insert(BrokerInstanceKey::Shared, stale_backend_handle());
        let registry = Mutex::new(registry);
        let router = HelloRouter::with_lifecycle_monitor(&loader, &registry);

        let reply = router.handle_request(&request());

        assert!(registry.lock().unwrap().is_empty());
        match reply.result.unwrap() {
            HelloReplyResult::Refused(refused) => {
                assert_eq!(
                    ErrorCode::try_from(refused.code).unwrap(),
                    ErrorCode::ErrorBackendSpawnFailed
                );
            }
            HelloReplyResult::Negotiated(negotiated) => {
                panic!("stale backend must not negotiate: {negotiated:?}")
            }
        }
    }
}
