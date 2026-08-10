#![cfg(feature = "client")]

use std::io::Cursor;

use prost::Message;
use running_process::broker::protocol::{
    write_frame, BrokerIsolation, Frame, FrameKind, Hello, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    handle_hello_connection_with_peer_policy, HelloHandler, PeerCredentialPolicy, PeerIdentity,
    RegisteredBackend,
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
            backend_pipe: r"\\.\pipe\rpb-v1-peer-creds-test-backend".into(),
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
        request_id: "req-peer-creds".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn peer_with_owner(uid_or_sid: &str) -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: uid_or_sid.into(),
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

fn encode_framed_frame(frame: &Frame) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &frame.encode_to_vec()).unwrap();
    bytes
}

#[test]
fn peer_creds_drop_silently_refuses_foreign_owner_before_hello_read() {
    let request = encode_framed_frame(&frame_for_hello(&hello()));
    let request_len = request.len();
    let mut stream = Cursor::new(request);
    let policy = PeerCredentialPolicy::owner_only("owner-1");

    let reply = handle_hello_connection_with_peer_policy(
        &mut stream,
        &handler(),
        peer_with_owner("owner-2"),
        &policy,
    )
    .unwrap();

    assert!(reply.is_none());
    assert_eq!(stream.position(), 0);
    assert_eq!(stream.get_ref().len(), request_len);
}
