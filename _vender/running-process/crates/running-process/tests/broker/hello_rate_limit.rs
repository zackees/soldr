#![cfg(feature = "client")]

use std::time::Duration;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello,
    HelloReply, PayloadEncoding, Refused, ServiceDefinition,
};
use running_process::broker::server::{HelloHandler, PeerIdentity, RegisteredBackend};

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

fn rate_limited_handler(max_per_window: u32) -> HelloHandler {
    HelloHandler::new()
        .with_rate_limit(max_per_window, Duration::from_secs(60))
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: r"\\.\pipe\rpb-v1-rate-limit-test-backend".into(),
            server_capabilities: 0x01,
        })
        .unwrap()
}

fn hello(peer_pid: u32) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-rate-limit".into(),
        connection_id: 0,
        peer_pid,
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn peer(pid: u32) -> PeerIdentity {
    PeerIdentity {
        pid,
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
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn assert_negotiated(reply: HelloReply) {
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(_) => {}
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

fn refused(reply: HelloReply) -> Refused {
    match reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => refused,
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refusal, got negotiated {negotiated:?}")
        }
    }
}

fn other_pid(pid: u32) -> u32 {
    if pid == u32::MAX {
        pid - 1
    } else {
        pid + 1
    }
}

#[test]
fn hello_rate_limit_refuses_same_verified_peer_pid_after_budget() {
    let handler = rate_limited_handler(2);
    let verified_peer = peer(std::process::id());
    let request = hello(verified_peer.pid);

    assert_negotiated(handler.handle_frame(frame_for_hello(&request), verified_peer.clone()));
    assert_negotiated(handler.handle_frame(frame_for_hello(&request), verified_peer.clone()));

    let refusal = refused(handler.handle_frame(frame_for_hello(&request), verified_peer));
    assert_eq!(
        ErrorCode::try_from(refusal.code),
        Ok(ErrorCode::ErrorRateLimited)
    );
    assert!(refusal.retry_after_ms > 0);
}

#[test]
fn hello_rate_limit_tracks_verified_peer_pids_independently() {
    let handler = rate_limited_handler(1);
    let peer_a = peer(std::process::id());
    let peer_b = peer(other_pid(peer_a.pid));
    let request_a = hello(peer_a.pid);
    let request_b = hello(peer_b.pid);

    assert_negotiated(handler.handle_frame(frame_for_hello(&request_a), peer_a.clone()));
    assert_negotiated(handler.handle_frame(frame_for_hello(&request_b), peer_b));

    let refusal = refused(handler.handle_frame(frame_for_hello(&request_a), peer_a));
    assert_eq!(
        ErrorCode::try_from(refusal.code),
        Ok(ErrorCode::ErrorRateLimited)
    );
}
