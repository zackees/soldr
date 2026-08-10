use std::time::Duration;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{HelloHandler, PeerIdentity, RegisteredBackend};

#[test]
fn local_peer_hello_flood_is_rate_limited_without_blocking_other_peers() {
    let handler = HelloHandler::new()
        .with_rate_limit(2, Duration::from_secs(60))
        .with_backend(backend())
        .unwrap();
    let noisy_peer = peer(4242);

    assert_negotiated(&handler.handle_frame(frame_for_hello("req-1"), noisy_peer.clone()));
    assert_negotiated(&handler.handle_frame(frame_for_hello("req-2"), noisy_peer.clone()));

    let rate_limited_reply = handler.handle_frame(frame_for_hello("req-3"), noisy_peer);
    let refused = refused(&rate_limited_reply);
    assert_eq!(refused.code, ErrorCode::ErrorRateLimited as i32);
    assert!(
        refused.retry_after_ms > 0,
        "rate-limited peer should get a bounded retry hint"
    );

    assert_negotiated(&handler.handle_frame(frame_for_hello("req-4"), peer(4343)));
}

fn backend() -> RegisteredBackend {
    RegisteredBackend {
        service_definition: service_definition(),
        daemon_version: "1.11.20".into(),
        backend_pipe: "rpb-v1-test-backend".into(),
        server_capabilities: 0x01,
    }
}

fn service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: platform_absolute_path("zccache"),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: platform_absolute_path("zccache-versions"),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn frame_for_hello(request_id: &str) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello(request_id).encode_to_vec(),
        request_id: 241,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn hello(request_id: &str) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: request_id.into(),
        connection_id: 0,
        peer_pid: 0,
        client_lib_name: "running-process".into(),
        client_lib_version: env!("CARGO_PKG_VERSION").into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn peer(pid: u32) -> PeerIdentity {
    PeerIdentity {
        pid,
        uid_or_sid: account_id("1000").into(),
    }
}

fn assert_negotiated(reply: &running_process::broker::protocol::HelloReply) {
    match reply.result.as_ref() {
        Some(HelloReplyResult::Negotiated(_)) => {}
        other => panic!("expected negotiated reply, got {other:?}"),
    }
}

fn refused(
    reply: &running_process::broker::protocol::HelloReply,
) -> &running_process::broker::protocol::Refused {
    match reply.result.as_ref() {
        Some(HelloReplyResult::Refused(refused)) => refused,
        other => panic!("expected refused reply, got {other:?}"),
    }
}

fn account_id(local_id: &'static str) -> &'static str {
    if cfg!(windows) {
        match local_id {
            "1000" => "S-1-5-21-1000",
            _ => "S-1-5-21-9999",
        }
    } else {
        match local_id {
            "1000" => "uid:1000",
            _ => "uid:9999",
        }
    }
}

fn platform_absolute_path(leaf: &str) -> String {
    if cfg!(windows) {
        format!(r"C:\running-process-test\{leaf}.exe")
    } else {
        format!("/opt/running-process-test/{leaf}")
    }
}
