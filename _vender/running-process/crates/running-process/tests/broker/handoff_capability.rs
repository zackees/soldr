//! Negotiated handle-passing capability and pending token issuance (#354).
//!
//! Slice 1 wires the capability bit and token plumbing only: the client
//! still connects via `Negotiated.backend_pipe`, and the token is exposed
//! on `BackendConnection` for the future adoption slice.

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use interprocess::local_socket::prelude::*;
use prost::Message;
use running_process::broker::backend_lib::{
    accept_handed_off, parse_handoff_token, HandedOffPayload, HandoffRejectionReason,
};
use running_process::broker::capabilities::CAP_HANDLE_PASSING;
use running_process::broker::client::{
    connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, Frame, FrameKind, Hello, Negotiated,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    handle_hello_connection, HelloHandler, PeerIdentity, RegisteredBackend, HANDOFF_TOKEN_BYTES,
};

use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, cleanup_test_socket, unique_socket_name,
};

const BASE_SERVER_CAPABILITIES: u64 = 1 << 8;

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

fn handler(backend_endpoint: &str) -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: backend_endpoint.into(),
            server_capabilities: BASE_SERVER_CAPABILITIES,
        })
        .unwrap()
}

fn hello(client_capabilities: u64) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities,
        auth_token: Vec::new(),
        request_id: "req-handoff-cap".into(),
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
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn negotiate(handler: &HelloHandler, client_capabilities: u64) -> Negotiated {
    let request = hello(client_capabilities);
    let reply = handler.handle_frame(frame_for_hello(&request), peer());
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => negotiated,
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn hello_with_capability_negotiates_pending_token() {
    let handler = handler(r"\\.\pipe\rpb-v1-handoff-cap-backend");

    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);

    assert_eq!(negotiated.handle_passed_token.len(), HANDOFF_TOKEN_BYTES);
    assert_eq!(
        negotiated.server_capabilities & CAP_HANDLE_PASSING,
        CAP_HANDLE_PASSING
    );
    assert_eq!(
        negotiated.server_capabilities & BASE_SERVER_CAPABILITIES,
        BASE_SERVER_CAPABILITIES,
        "backend capability bits must be preserved"
    );
    assert_eq!(handler.handoff_token_store().pending_len(), 1);
}

#[test]
fn hello_without_capability_keeps_token_empty() {
    let handler = handler(r"\\.\pipe\rpb-v1-handoff-nocap-backend");

    let negotiated = negotiate(&handler, 0);

    assert!(negotiated.handle_passed_token.is_empty());
    assert_eq!(negotiated.server_capabilities & CAP_HANDLE_PASSING, 0);
    assert_eq!(negotiated.server_capabilities, BASE_SERVER_CAPABILITIES);
    assert_eq!(handler.handoff_token_store().pending_len(), 0);
}

#[test]
fn each_negotiation_issues_a_distinct_token() {
    let handler = handler(r"\\.\pipe\rpb-v1-handoff-distinct-backend");

    let first = negotiate(&handler, CAP_HANDLE_PASSING);
    let second = negotiate(&handler, CAP_HANDLE_PASSING);

    assert_ne!(first.handle_passed_token, second.handle_passed_token);
    assert_eq!(handler.handoff_token_store().pending_len(), 2);
}

#[test]
fn negotiated_token_is_consumed_exactly_once() {
    let handler = handler(r"\\.\pipe\rpb-v1-handoff-once-backend");
    let negotiated = negotiate(&handler, CAP_HANDLE_PASSING);
    let expected = parse_handoff_token(&negotiated.handle_passed_token).unwrap();
    let now = Instant::now();

    let accepted = accept_handed_off(
        &mut handler.handoff_token_store(),
        HandedOffPayload::new(expected, negotiated.handle_passed_token.clone(), "conn-1"),
        now,
    );
    assert!(accepted.is_accepted());

    let replayed = accept_handed_off(
        &mut handler.handoff_token_store(),
        HandedOffPayload::new(expected, negotiated.handle_passed_token.clone(), "conn-2"),
        now,
    );
    let rejected = replayed.into_result().unwrap_err();
    assert_eq!(rejected.reason, HandoffRejectionReason::TokenNotPending);
}

#[test]
fn connect_to_backend_exposes_token_and_keeps_reconnect_route() {
    let broker_endpoint = unique_socket_name("broker-handoff-cap");
    let backend_endpoint = unique_socket_name("backend-handoff-cap");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    let token = connection.handoff_token().expect("token must be exposed");
    assert_eq!(token.len(), HANDOFF_TOKEN_BYTES);
    let negotiated = connection.negotiated.as_ref().unwrap();
    assert_eq!(
        negotiated.server_capabilities & CAP_HANDLE_PASSING,
        CAP_HANDLE_PASSING
    );
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

fn spawn_accept_once(socket_name: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = socket_name.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&socket_name, &ready_tx)?;
        let _stream = listener.accept()?;
        cleanup_test_socket(&socket_name);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

fn spawn_broker_once(
    broker_endpoint: String,
    backend_endpoint: String,
) -> thread::JoinHandle<io::Result<()>> {
    let display_name = broker_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&broker_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "test-peer".into(),
        };
        handle_hello_connection(&mut stream, &handler(&backend_endpoint), peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&broker_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}
