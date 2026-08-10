#![cfg(feature = "client")]

use std::fs;
use std::path::Path;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_path, BackendRegistry, HelloRequest,
    HelloRouter, PeerIdentity, ServiceDefinitionLoader,
};

fn absolute_paths() -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    (
        exe.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    )
}

fn service_definition() -> ServiceDefinition {
    let (binary_path, per_version_binary_dir) = absolute_paths();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path,
        isolation: BrokerIsolation::SharedBroker as i32,
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

fn request(wanted_version: &str) -> HelloRequest {
    let hello = Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: wanted_version.into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-version-blocked".into(),
        connection_id: 0,
        peer_pid: 0,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
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
            request_id: 23,
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

fn refused_code(reply: running_process::broker::protocol::HelloReply) -> ErrorCode {
    match reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => {
            assert_eq!(refused.retry_after_ms, 30_000);
            ErrorCode::try_from(refused.code).unwrap()
        }
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected version-blocked refusal, got negotiated {negotiated:?}")
        }
    }
}

#[test]
fn router_blocks_wanted_version_below_min_version() {
    let definition = service_definition();
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("1.9.9"));

    assert_eq!(refused_code(reply), ErrorCode::ErrorVersionBlocked);
}

#[test]
fn router_blocks_wanted_version_outside_allow_list() {
    let definition = service_definition();
    let tmp = service_dir_with_definition(&definition);
    let loader = ServiceDefinitionLoader::new(tmp.path().join("services"));
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("1.12.0"));

    assert_eq!(refused_code(reply), ErrorCode::ErrorVersionBlocked);
}
