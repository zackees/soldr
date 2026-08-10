#![cfg(feature = "client")]

use std::fs;
use std::path::Path;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_path, BackendRegistry, BrokerInstanceKey,
    HelloRequest, HelloRouter, PeerIdentity, ServiceDefinitionLoader,
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

fn write_definition_for(root: &Path, definition: &ServiceDefinition) {
    let path = service_definition_path(root, &definition.service_name).unwrap();
    fs::write(path, definition.encode_to_vec()).unwrap();
}

fn service_loader_for(
    definition: &ServiceDefinition,
) -> (tempfile::TempDir, ServiceDefinitionLoader) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    write_definition_for(&root, definition);
    let loader = ServiceDefinitionLoader::new(root);
    (tmp, loader)
}

fn hello() -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-instance-isolation".into(),
        connection_id: 0,
        peer_pid: 0,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn request() -> HelloRequest {
    let hello = hello();
    HelloRequest {
        frame: Frame {
            envelope_version: 1,
            kind: FrameKind::Request as i32,
            payload_protocol: 0,
            payload: hello.encode_to_vec(),
            request_id: 7,
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

fn registry_with_shared_backend() -> (BackendRegistry, String) {
    let daemon = current_daemon();
    let expected_pipe = daemon.ipc_endpoint.path.clone();
    let handle = verified_backend_from_daemon("zccache", "1.11.20", &daemon);
    let mut registry = BackendRegistry::new();
    registry.insert(BrokerInstanceKey::Shared, handle);
    (registry, expected_pipe)
}

#[test]
fn private_broker_route_does_not_see_shared_broker_backend() {
    let (_shared_tmp, shared_loader) =
        service_loader_for(&service_definition(BrokerIsolation::SharedBroker));
    let (registry, expected_shared_pipe) = registry_with_shared_backend();
    let shared_router = HelloRouter::new(&shared_loader, &registry);

    let shared_reply = shared_router.handle_request(&request());

    match shared_reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, expected_shared_pipe);
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected shared refusal: {refused:?}"),
    }

    let (_private_tmp, private_loader) =
        service_loader_for(&service_definition(BrokerIsolation::PrivateBroker));
    let private_router = HelloRouter::new(&private_loader, &registry);

    let private_reply = private_router.handle_request(&request());

    match private_reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => {
            assert_eq!(
                ErrorCode::try_from(refused.code).unwrap(),
                ErrorCode::ErrorBackendSpawnFailed
            );
        }
        HelloReplyResult::Negotiated(negotiated) => {
            assert_ne!(negotiated.backend_pipe, expected_shared_pipe);
        }
    }
}
