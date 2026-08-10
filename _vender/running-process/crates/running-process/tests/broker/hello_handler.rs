#![cfg(feature = "client")]

use std::time::Duration;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    HelloHandler, HelloRequest, PeerIdentity, RegisteredBackend,
};

fn service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: "/opt/zccache/versions".into(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn handler() -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: r"\\.\pipe\rpb-v1-test-backend".into(),
            server_capabilities: 0x01,
        })
        .unwrap()
}

fn limited_handler(max_per_window: u32) -> HelloHandler {
    HelloHandler::new()
        .with_rate_limit(max_per_window, Duration::from_secs(60))
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: r"\\.\pipe\rpb-v1-test-backend".into(),
            server_capabilities: 0x01,
        })
        .unwrap()
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
        request_id: "req-1".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: "test-peer".into(),
    }
}

fn frame_for_hello(hello: &Hello) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello.encode_to_vec(),
        request_id: 7,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: "vendor=value".into(),
    }
}

fn refused_code(reply: running_process::broker::protocol::HelloReply) -> ErrorCode {
    match reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => ErrorCode::try_from(refused.code).unwrap(),
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refused, got negotiated {negotiated:?}")
        }
    }
}

fn assert_negotiated(reply: running_process::broker::protocol::HelloReply) {
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(_) => {}
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn hello_negotiates_registered_backend() {
    let request = hello();
    let reply = handler().handle_frame(frame_for_hello(&request), peer());
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.negotiated_protocol, 1);
            assert_eq!(negotiated.daemon_version, "1.11.20");
            assert_eq!(negotiated.backend_pipe, r"\\.\pipe\rpb-v1-test-backend");
            assert_eq!(negotiated.keepalive_interval_secs, 60);
            assert_eq!(negotiated.connection_id, 1);
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn hello_request_decode_preserves_frame_context() {
    let request = hello();
    let frame = frame_for_hello(&request);
    let decoded = HelloRequest::decode(frame, peer()).unwrap();

    assert_eq!(decoded.frame.request_id, 7);
    assert_eq!(
        decoded.frame.traceparent,
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    assert_eq!(decoded.frame.tracestate, "vendor=value");
    assert_eq!(decoded.hello.service_name, "zccache");
    assert_eq!(decoded.peer.pid, std::process::id());
}

#[test]
fn hello_rejects_invalid_service_name() {
    let mut request = hello();
    request.service_name = "Zccache".into();
    assert_eq!(
        refused_code(handler().handle_frame(frame_for_hello(&request), peer())),
        ErrorCode::ErrorPeerRejected
    );
}

#[test]
fn hello_rejects_unknown_service() {
    let mut request = hello();
    request.service_name = "clud".into();
    assert_eq!(
        refused_code(handler().handle_frame(frame_for_hello(&request), peer())),
        ErrorCode::ErrorServiceUnknown
    );
}

#[test]
fn hello_rejects_version_below_floor() {
    let mut request = hello();
    request.wanted_version = "1.9.9".into();
    assert_eq!(
        refused_code(handler().handle_frame(frame_for_hello(&request), peer())),
        ErrorCode::ErrorVersionBlocked
    );
}

#[test]
fn hello_rejects_version_outside_allow_list() {
    let mut request = hello();
    request.wanted_version = "1.12.0".into();
    assert_eq!(
        refused_code(handler().handle_frame(frame_for_hello(&request), peer())),
        ErrorCode::ErrorVersionBlocked
    );
}

#[test]
fn hello_rejects_protocol_skew() {
    let mut request = hello();
    request.client_min_protocol = 2;
    request.client_max_protocol = 2;
    assert_eq!(
        refused_code(handler().handle_frame(frame_for_hello(&request), peer())),
        ErrorCode::ErrorVersionUnsupported
    );
}

#[test]
fn hello_rejects_peer_pid_mismatch() {
    let request = hello();
    let mut wrong_peer = peer();
    wrong_peer.pid = wrong_peer.pid.saturating_add(1);

    assert_eq!(
        refused_code(handler().handle_frame(frame_for_hello(&request), wrong_peer)),
        ErrorCode::ErrorPeerRejected
    );
}

#[test]
fn hello_accepts_claimed_pid_when_kernel_peer_pid_unavailable() {
    // macOS LOCAL_PEERCRED carries no pid, so PeerIdentity.pid is 0 there.
    // A nonzero client-claimed pid must not be refused against that 0.
    let request = hello();
    let kernel_blind_peer = PeerIdentity {
        pid: 0,
        uid_or_sid: "test-peer".into(),
    };

    assert_negotiated(handler().handle_frame(frame_for_hello(&request), kernel_blind_peer));
}

#[test]
fn hello_rate_limit_refuses_after_peer_budget() {
    let handler = limited_handler(2);
    let request = hello();
    let verified_peer = peer();

    assert_negotiated(handler.handle_frame(frame_for_hello(&request), verified_peer.clone()));
    assert_negotiated(handler.handle_frame(frame_for_hello(&request), verified_peer.clone()));
    assert_eq!(
        refused_code(handler.handle_frame(frame_for_hello(&request), verified_peer)),
        ErrorCode::ErrorRateLimited
    );
}

#[test]
fn hello_rate_limit_is_per_peer_pid() {
    let handler = limited_handler(1);
    let request = hello();
    let peer_a = peer();
    let mut peer_b = peer();
    peer_b.pid = peer_b.pid.saturating_add(1);
    let mut request_b = request.clone();
    request_b.peer_pid = peer_b.pid;

    assert_negotiated(handler.handle_frame(frame_for_hello(&request), peer_a.clone()));
    assert_negotiated(handler.handle_frame(frame_for_hello(&request_b), peer_b));
    assert_eq!(
        refused_code(handler.handle_frame(frame_for_hello(&request), peer_a)),
        ErrorCode::ErrorRateLimited
    );
}

#[test]
fn hello_rate_limit_skips_unknown_verified_pid() {
    let handler = limited_handler(1);
    let mut request = hello();
    request.peer_pid = 0;
    let unknown_peer = PeerIdentity {
        pid: 0,
        uid_or_sid: "test-peer".into(),
    };

    assert_negotiated(handler.handle_frame(frame_for_hello(&request), unknown_peer.clone()));
    assert_negotiated(handler.handle_frame(frame_for_hello(&request), unknown_peer));
}

#[test]
fn hello_rejects_malformed_frame_payload() {
    let mut frame = frame_for_hello(&hello());
    frame.payload = vec![0xFF, 0xFF, 0xFF];

    assert_eq!(
        refused_code(handler().handle_frame(frame, peer())),
        ErrorCode::ErrorPeerRejected
    );
}
