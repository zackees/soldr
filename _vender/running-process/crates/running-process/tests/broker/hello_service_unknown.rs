#![cfg(feature = "client")]

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, FrameKind, Hello, PayloadEncoding,
};
use running_process::broker::server::{
    ensure_service_definition_dir, BackendRegistry, HelloRequest, HelloRouter, PeerIdentity,
    ServiceDefinitionLoader,
};

fn request(service_name: &str) -> HelloRequest {
    let hello = Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: service_name.into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-service-unknown".into(),
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
            request_id: 17,
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
            assert_eq!(refused.retry_after_ms, 0);
            ErrorCode::try_from(refused.code).unwrap()
        }
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected service-unknown refusal, got negotiated {negotiated:?}")
        }
    }
}

#[test]
fn router_returns_service_unknown_when_service_definition_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let service_root = tmp.path().join("services");
    ensure_service_definition_dir(&service_root).unwrap();
    let loader = ServiceDefinitionLoader::new(service_root);
    let registry = BackendRegistry::new();
    let router = HelloRouter::new(&loader, &registry);

    let reply = router.handle_request(&request("zccache"));

    assert_eq!(refused_code(reply), ErrorCode::ErrorServiceUnknown);
}
