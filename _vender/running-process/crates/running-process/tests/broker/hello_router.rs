#![cfg(feature = "client")]

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use prost::Message;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, write_frame, BrokerIsolation, Endpoint,
    ErrorCode, Frame, FrameKind, Hello, HelloReply, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    ensure_service_definition_dir, handle_hello_connection_with, service_definition_path,
    BackendLaunchError, BackendLaunchRequest, BackendLauncher, BackendRegistry, BrokerInstanceKey,
    HelloRequest, HelloRouter, PeerIdentity, ServiceDefinitionLoader, SpawnBudgetConfig,
    SpawnCoordinator, TraceContext,
};

use crate::backend_handle_common::{current_daemon, verified_backend_from_daemon};

fn absolute_paths() -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    (
        exe.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    )
}

fn service_definition(isolation: BrokerIsolation) -> ServiceDefinition {
    let (binary_path, per_version_binary_dir) = absolute_paths();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path,
        isolation: isolation as i32,
        explicit_instance: String::new(),
        per_version_binary_dir,
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn write_definition_for(root: &Path, service_name: &str, definition: &ServiceDefinition) {
    let path = service_definition_path(root, service_name).unwrap();
    fs::write(path, definition.encode_to_vec()).unwrap();
}

fn service_dir_with_definition(definition: &ServiceDefinition) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    write_definition_for(&root, "zccache", definition);
    tmp
}

fn hello(service_name: &str, wanted_version: &str) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: service_name.into(),
        wanted_version: wanted_version.into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-router".into(),
        connection_id: 0,
        peer_pid: 0,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn request(service_name: &str, wanted_version: &str) -> HelloRequest {
    HelloRequest {
        frame: Frame {
            envelope_version: 1,
            kind: FrameKind::Request as i32,
            payload_protocol: 0,
            payload: hello(service_name, wanted_version).encode_to_vec(),
            request_id: 7,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        },
        hello: hello(service_name, wanted_version),
        peer: PeerIdentity {
            pid: 0,
            uid_or_sid: "test-peer".into(),
        },
    }
}

fn registry_with_backend(instance: BrokerInstanceKey) -> (BackendRegistry, String) {
    let daemon = current_daemon();
    let expected_pipe = daemon.ipc_endpoint.path.clone();
    let handle = verified_backend_from_daemon("zccache", "1.11.20", &daemon);
    let mut registry = BackendRegistry::new();
    registry.insert(instance, handle);
    (registry, expected_pipe)
}

fn current_backend_for(
    service_name: &str,
    service_version: &str,
    instance: &BrokerInstanceKey,
    endpoint_path: &str,
) -> BackendHandle {
    let endpoint = Endpoint {
        namespace_id: instance.id(),
        path: endpoint_path.into(),
    };
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).unwrap();
    verified_backend_from_daemon(service_name, service_version, &daemon)
}

struct CurrentProcessLauncher {
    endpoint_path: String,
    calls: Mutex<usize>,
    trace_contexts: Mutex<Vec<TraceContext>>,
}

impl CurrentProcessLauncher {
    fn new(endpoint_path: impl Into<String>) -> Self {
        Self {
            endpoint_path: endpoint_path.into(),
            calls: Mutex::new(0),
            trace_contexts: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }

    fn trace_contexts(&self) -> Vec<TraceContext> {
        self.trace_contexts.lock().unwrap().clone()
    }
}

impl BackendLauncher for CurrentProcessLauncher {
    fn launch(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        *self.calls.lock().unwrap() += 1;
        self.trace_contexts
            .lock()
            .unwrap()
            .push(request.trace_context.clone());
        Ok(current_backend_for(
            &request.key.service_name,
            &request.key.service_version,
            &request.key.instance,
            &self.endpoint_path,
        ))
    }
}

struct FailingLauncher {
    calls: Mutex<usize>,
}

impl FailingLauncher {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl BackendLauncher for FailingLauncher {
    fn launch(
        &self,
        _request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        *self.calls.lock().unwrap() += 1;
        Err(BackendLaunchError::Launcher("test launcher failed".into()))
    }
}

fn reply_code(reply: &running_process::broker::protocol::HelloReply) -> ErrorCode {
    match reply.result.as_ref().unwrap() {
        HelloReplyResult::Refused(refused) => ErrorCode::try_from(refused.code).unwrap(),
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refusal, got negotiated {negotiated:?}")
        }
    }
}

fn encode_framed_frame(frame: &Frame) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &frame.encode_to_vec()).unwrap();
    bytes
}

fn decode_response_frame(bytes: &[u8]) -> (Frame, HelloReply) {
    let mut cursor = Cursor::new(bytes);
    let response_bytes = read_frame(&mut cursor).unwrap();
    let frame = Frame::decode(response_bytes.as_slice()).unwrap();
    let reply = HelloReply::decode(frame.payload.as_slice()).unwrap();
    (frame, reply)
}

#[test]
fn router_negotiates_registry_backend_for_loaded_service_definition() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let (registry, expected_pipe) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, expected_pipe);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn framed_connection_can_route_through_service_definition_router() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let (registry, expected_pipe) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);
    let mut request = request("zccache", "1.11.20");
    request.frame.request_id = 41;
    request.frame.traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into();
    let peer = request.peer.clone();
    let request_bytes = encode_framed_frame(&request.frame);
    let request_len = request_bytes.len();
    let mut stream = Cursor::new(request_bytes);

    let reply = handle_hello_connection_with(&mut stream, &router, peer).unwrap();

    let response_bytes = &stream.get_ref()[request_len..];
    let (response_frame, decoded_reply) = decode_response_frame(response_bytes);
    assert_eq!(reply, decoded_reply);
    assert_eq!(response_frame.request_id, 41);
    assert_eq!(
        response_frame.traceparent,
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    match decoded_reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, expected_pipe);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn router_rereads_service_definition_on_each_request() {
    let mut definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let service_root = tmp.path().join("services");
    let loader = ServiceDefinitionLoader::new(&service_root);
    let (registry, _) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);

    assert!(matches!(
        router.handle_request(&request("zccache", "1.11.20")).result,
        Some(HelloReplyResult::Negotiated(_))
    ));

    definition.min_version = "1.12.0".into();
    write_definition_for(&service_root, "zccache", &definition);

    let reply = router.handle_request(&request("zccache", "1.11.20"));
    assert_eq!(reply_code(&reply), ErrorCode::ErrorVersionBlocked);
}

#[test]
fn router_reports_missing_service_definition_as_service_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let service_root = tmp.path().join("services");
    ensure_service_definition_dir(&service_root).unwrap();
    let loader = ServiceDefinitionLoader::new(service_root);
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&reply), ErrorCode::ErrorServiceUnknown);
}

#[test]
fn router_reports_registry_miss_as_spawn_failed_placeholder() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&reply), ErrorCode::ErrorBackendSpawnFailed);
}

#[test]
fn router_spawns_and_registers_backend_on_live_registry_miss() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = Mutex::new(BackendRegistry::new());
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let endpoint_path = format!("rpb-v1-test-spawn-success-{}", std::process::id());
    let launcher = CurrentProcessLauncher::new(&endpoint_path);
    let router = HelloRouter::with_lifecycle_monitor(&loader, &registry)
        .with_spawn_coordinator(&spawn_coordinator)
        .with_backend_launcher(&launcher);
    let mut first_request = request("zccache", "1.11.20");
    first_request.frame.request_id = 88;
    first_request.frame.traceparent =
        "00-11111111111111111111111111111111-2222222222222222-01".into();
    first_request.frame.tracestate = "vendor=value".into();

    let first = router.handle_request(&first_request);

    match first.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, endpoint_path);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
    assert_eq!(launcher.calls(), 1);
    assert_eq!(
        launcher.trace_contexts(),
        vec![TraceContext {
            request_id: 88,
            traceparent: "00-11111111111111111111111111111111-2222222222222222-01".into(),
            tracestate: "vendor=value".into(),
        }]
    );
    assert!(registry
        .lock()
        .unwrap()
        .get(&BrokerInstanceKey::Shared, "zccache", "1.11.20")
        .is_some());

    let second = router.handle_request(&request("zccache", "1.11.20"));

    assert!(matches!(
        second.result,
        Some(HelloReplyResult::Negotiated(_))
    ));
    assert_eq!(launcher.calls(), 1);
}

#[test]
fn router_launch_failure_consumes_spawn_budget() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = Mutex::new(BackendRegistry::new());
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let launcher = FailingLauncher::new();
    let router = HelloRouter::with_lifecycle_monitor(&loader, &registry)
        .with_spawn_coordinator(&spawn_coordinator)
        .with_backend_launcher(&launcher);

    let first = router.handle_request(&request("zccache", "1.11.20"));
    let second = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&first), ErrorCode::ErrorBackendSpawnFailed);
    assert_eq!(reply_code(&second), ErrorCode::ErrorRateLimited);
    assert_eq!(launcher.calls(), 1);
    assert!(registry.lock().unwrap().is_empty());
}

#[test]
fn router_consumes_spawn_budget_on_registry_miss() {
    let definition = service_definition(BrokerIsolation::SharedBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let router = HelloRouter::new(&loader, &registry).with_spawn_coordinator(&spawn_coordinator);

    let first = router.handle_request(&request("zccache", "1.11.20"));
    let second = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&first), ErrorCode::ErrorBackendSpawnFailed);
    assert_eq!(reply_code(&second), ErrorCode::ErrorRateLimited);
}

#[test]
fn router_spawn_budget_is_per_service_version() {
    let mut definition = service_definition(BrokerIsolation::SharedBroker);
    definition.version_allow_list = vec!["1.11.20".into(), "1.11.21".into()];
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let router = HelloRouter::new(&loader, &registry).with_spawn_coordinator(&spawn_coordinator);

    let first = router.handle_request(&request("zccache", "1.11.20"));
    let second_version = router.handle_request(&request("zccache", "1.11.21"));

    assert_eq!(reply_code(&first), ErrorCode::ErrorBackendSpawnFailed);
    assert_eq!(
        reply_code(&second_version),
        ErrorCode::ErrorBackendSpawnFailed
    );
}

#[test]
fn router_spawn_budget_allows_distinct_version_flood_once() {
    let versions: Vec<String> = (20..36).map(|patch| format!("1.11.{patch}")).collect();
    let mut definition = service_definition(BrokerIsolation::SharedBroker);
    definition.version_allow_list = versions.clone();
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let spawn_coordinator = Mutex::new(SpawnCoordinator::with_config(SpawnBudgetConfig::new(
        1,
        Duration::from_secs(10),
    )));
    let router = HelloRouter::new(&loader, &registry).with_spawn_coordinator(&spawn_coordinator);

    for version in &versions {
        let first = router.handle_request(&request("zccache", version));
        assert_eq!(
            reply_code(&first),
            ErrorCode::ErrorBackendSpawnFailed,
            "first distinct-version request should be admitted for {version}"
        );
    }

    for version in &versions {
        let second = router.handle_request(&request("zccache", version));
        assert_eq!(
            reply_code(&second),
            ErrorCode::ErrorRateLimited,
            "second request should exhaust the per-version spawn budget for {version}"
        );
    }
}

#[test]
fn router_respects_service_definition_instance_isolation() {
    let definition = service_definition(BrokerIsolation::PrivateBroker);
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let (registry, _) = registry_with_backend(BrokerInstanceKey::Shared);
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache", "1.11.20"));

    assert_eq!(reply_code(&reply), ErrorCode::ErrorBackendSpawnFailed);
}
