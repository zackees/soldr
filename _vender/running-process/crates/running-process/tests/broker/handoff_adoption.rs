//! Client-side adoption of handed-off backend connections (#354, slice 7).
//!
//! With `ConnectBackendRequest::adopt_handed_off_connection` set, the client
//! waits (deadline-bounded) for the broker's handoff-ready relay — an EVENT
//! frame under the `0xD0FF` handoff payload protocol carrying the backend's
//! `HandoffAck` — on the same connection that carried Hello. A valid
//! accepted relay with a matching token echo means the broker handed the
//! client's connection to the backend, so the client keeps the socket it
//! already has (`BackendConnectionRoute::HandlePassed`). Every failure mode
//! (timeout, refusal, malformed relay, token mismatch, missing negotiation)
//! silently downgrades to today's `backend_pipe` reconnect.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use prost::Message;
use running_process::broker::capabilities::CAP_HANDLE_PASSING;
use running_process::broker::client::{
    connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, write_frame, BrokerIsolation, Frame, FrameKind,
    HandoffAck, HelloReply, Negotiated, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::handoff::handoff_ready_frame;
use running_process::broker::server::{
    handle_hello_connection, HelloHandler, PeerIdentity, RegisteredBackend,
};

use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, cleanup_test_socket, unique_socket_name,
};

const READY_TIMEOUT: Duration = Duration::from_millis(300);
const CLIENT_PROBE: u8 = 0xC3;
const BACKEND_REPLY: u8 = 0x5A;

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
            server_capabilities: 0,
        })
        .unwrap()
}

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: "test-peer".into(),
    }
}

fn adopting_request<'a>(broker_endpoint: &'a str) -> ConnectBackendRequest<'a> {
    let mut request = ConnectBackendRequest::new(broker_endpoint, "zccache", "1.11.20", "1.11.20");
    request.adopt_handed_off_connection = true;
    request.handoff_ready_timeout = READY_TIMEOUT;
    request
}

/// What the test broker does on the Hello connection after replying.
enum BrokerScenario {
    /// Relay an accepted ACK echoing the issued token, then serve one
    /// probe/reply byte exchange on the SAME socket (backend traffic).
    ReadyThenServe,
    /// Relay a refused ACK, then close.
    Refused,
    /// Relay an accepted ACK with a wrong token echo, then close.
    TokenMismatch,
    /// Relay a frame that is not a valid handoff relay, then close.
    Malformed,
    /// Send nothing and hold the connection open until signaled.
    SilentHold(mpsc::Receiver<()>),
}

fn negotiated_token(reply: &HelloReply) -> Vec<u8> {
    match reply.result.as_ref().unwrap() {
        HelloReplyResult::Negotiated(negotiated) => negotiated.handle_passed_token.clone(),
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

fn spawn_broker(
    broker_endpoint: String,
    backend_endpoint: String,
    scenario: BrokerScenario,
) -> thread::JoinHandle<io::Result<()>> {
    let display_name = broker_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&broker_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let reply = handle_hello_connection(&mut stream, &handler(&backend_endpoint), peer())
            .map_err(|err| io::Error::other(err.to_string()))?;
        let token = negotiated_token(&reply);
        assert!(!token.is_empty(), "handler must have issued a token");
        match scenario {
            BrokerScenario::ReadyThenServe => {
                write_ready(&mut stream, accepted_ack(token))?;
                let mut probe = [0_u8; 1];
                stream.read_exact(&mut probe)?;
                assert_eq!(probe, [CLIENT_PROBE]);
                stream.write_all(&[BACKEND_REPLY])?;
            }
            BrokerScenario::Refused => {
                let ack = HandoffAck {
                    accepted: false,
                    error_detail: "backend refused the handoff".into(),
                    ..accepted_ack(token)
                };
                write_ready(&mut stream, ack)?;
            }
            BrokerScenario::TokenMismatch => {
                let mut wrong = token;
                wrong[0] ^= 0xFF;
                write_ready(&mut stream, accepted_ack(wrong))?;
            }
            BrokerScenario::Malformed => {
                // Wrong payload protocol and kind: a control-plane response
                // frame where the client expects the handoff EVENT relay.
                let frame = Frame {
                    envelope_version: 1,
                    kind: FrameKind::Response as i32,
                    payload_protocol: 0,
                    payload: vec![0xFF; 8],
                    request_id: 9,
                    payload_encoding: PayloadEncoding::None as i32,
                    deadline_unix_ms: 0,
                    traceparent: String::new(),
                    tracestate: String::new(),
                };
                write_frame(&mut stream, &frame.encode_to_vec())
                    .map_err(|err| io::Error::other(err.to_string()))?;
            }
            BrokerScenario::SilentHold(release) => {
                // Keep the connection open without writing anything so the
                // client's bounded relay wait must expire.
                let _ = release.recv_timeout(Duration::from_secs(30));
            }
        }
        cleanup_test_socket(&broker_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

fn accepted_ack(token: Vec<u8>) -> HandoffAck {
    HandoffAck {
        token,
        accepted: true,
        error_detail: String::new(),
        correlation_id: 7,
    }
}

fn write_ready<S: Write>(stream: &mut S, ack: HandoffAck) -> io::Result<()> {
    let frame = handoff_ready_frame(&ack);
    write_frame(stream, &frame.encode_to_vec())
        .map(|_| ())
        .map_err(|err| io::Error::other(err.to_string()))
}

#[test]
fn confirmed_handoff_adopts_existing_connection() {
    let broker_endpoint = unique_socket_name("broker-adopt-ok");
    // No listener is ever bound on the backend endpoint: if the client
    // wrongly fell back to reconnect, connect_to_backend would error.
    let backend_endpoint = unique_socket_name("backend-adopt-ok");
    let broker = spawn_broker(
        broker_endpoint.clone(),
        backend_endpoint.clone(),
        BrokerScenario::ReadyThenServe,
    );

    let mut connection = connect_to_backend(adopting_request(&broker_endpoint)).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::HandlePassed);
    assert_eq!(
        connection.endpoint, backend_endpoint,
        "adopted connections must still report backend_pipe for hello-skip caching"
    );
    assert!(connection.handoff_token().is_some());

    // Prove the SAME socket now serves backend traffic.
    connection.stream.write_all(&[CLIENT_PROBE]).unwrap();
    let mut reply = [0_u8; 1];
    connection.stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [BACKEND_REPLY]);

    drop(connection.stream);
    broker.join().unwrap().unwrap();
}

#[test]
fn relay_timeout_downgrades_to_backend_pipe() {
    let broker_endpoint = unique_socket_name("broker-adopt-timeout");
    let backend_endpoint = unique_socket_name("backend-adopt-timeout");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let (release_tx, release_rx) = mpsc::channel();
    let broker = spawn_broker(
        broker_endpoint.clone(),
        backend_endpoint.clone(),
        BrokerScenario::SilentHold(release_rx),
    );

    let connection = connect_to_backend(adopting_request(&broker_endpoint)).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_endpoint);
    release_tx.send(()).unwrap();
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[test]
fn refused_relay_downgrades_to_backend_pipe() {
    let broker_endpoint = unique_socket_name("broker-adopt-refused");
    let backend_endpoint = unique_socket_name("backend-adopt-refused");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker(
        broker_endpoint.clone(),
        backend_endpoint.clone(),
        BrokerScenario::Refused,
    );

    let connection = connect_to_backend(adopting_request(&broker_endpoint)).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_endpoint);
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[test]
fn token_echo_mismatch_downgrades_to_backend_pipe() {
    let broker_endpoint = unique_socket_name("broker-adopt-badtoken");
    let backend_endpoint = unique_socket_name("backend-adopt-badtoken");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker(
        broker_endpoint.clone(),
        backend_endpoint.clone(),
        BrokerScenario::TokenMismatch,
    );

    let connection = connect_to_backend(adopting_request(&broker_endpoint)).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[test]
fn malformed_relay_downgrades_to_backend_pipe() {
    let broker_endpoint = unique_socket_name("broker-adopt-malformed");
    let backend_endpoint = unique_socket_name("backend-adopt-malformed");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker(
        broker_endpoint.clone(),
        backend_endpoint.clone(),
        BrokerScenario::Malformed,
    );

    let connection = connect_to_backend(adopting_request(&broker_endpoint)).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    drop(connection.stream);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[test]
fn missing_negotiation_skips_the_relay_wait() {
    // A broker that issues no token (or no capability bit) must not make an
    // opted-in client wait on the relay at all: straight to backend_pipe.
    for (label, token, capabilities) in [
        ("notoken", Vec::new(), CAP_HANDLE_PASSING),
        ("nocap", vec![0xAB; 16], 0_u64),
    ] {
        let broker_endpoint = unique_socket_name(&format!("broker-adopt-{label}"));
        let backend_endpoint = unique_socket_name(&format!("backend-adopt-{label}"));
        let backend = spawn_accept_once(backend_endpoint.clone());
        let broker = spawn_raw_negotiated_broker(
            broker_endpoint.clone(),
            backend_endpoint.clone(),
            token,
            capabilities,
        );

        let mut request = adopting_request(&broker_endpoint);
        request.handoff_ready_timeout = Duration::from_secs(30);
        let started = Instant::now();
        let connection = connect_to_backend(request).unwrap();

        assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "client must not block on the relay when handoff was not negotiated"
        );
        drop(connection.stream);
        broker.join().unwrap().unwrap();
        backend.join().unwrap().unwrap();
    }
}

/// Test broker that answers Hello with a hand-rolled `Negotiated` so the
/// token/capability combination is fully controlled, then closes.
fn spawn_raw_negotiated_broker(
    broker_endpoint: String,
    backend_endpoint: String,
    handle_passed_token: Vec<u8>,
    server_capabilities: u64,
) -> thread::JoinHandle<io::Result<()>> {
    let display_name = broker_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&broker_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let _hello = running_process::broker::protocol::read_frame(&mut stream)
            .map_err(|err| io::Error::other(err.to_string()))?;
        let reply = HelloReply {
            result: Some(HelloReplyResult::Negotiated(Negotiated {
                negotiated_protocol: 1,
                daemon_version: "1.11.20".into(),
                backend_pipe: backend_endpoint,
                warnings: Vec::new(),
                server_capabilities,
                keepalive_interval_secs: 60,
                handle_passed_token,
                connection_id: 1,
            })),
        };
        let frame = Frame {
            envelope_version: 1,
            kind: FrameKind::Response as i32,
            payload_protocol: 0,
            payload: reply.encode_to_vec(),
            request_id: 1,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        };
        write_frame(&mut stream, &frame.encode_to_vec())
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&broker_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
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
