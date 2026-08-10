#![cfg(feature = "client")]

use std::io::{self, Cursor};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, write_frame, BrokerIsolation, ErrorCode,
    Frame, FrameKind, FramingError, Hello, HelloReply, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    handle_hello_connection, handle_hello_connection_with_peer_policy, local_socket_name,
    serve_local_socket_connections, serve_one_local_socket,
    serve_one_local_socket_with_peer_policy, BrokerConnectionError, HelloHandler,
    PeerCredentialPolicy, PeerIdentity, RegisteredBackend,
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

fn hello(peer_pid: u32) -> Hello {
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
        peer_pid,
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
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: "vendor=value".into(),
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
fn handle_hello_connection_returns_framed_negotiated_reply() {
    let request = encode_framed_frame(&frame_for_hello(&hello(std::process::id())));
    let request_len = request.len();
    let mut stream = Cursor::new(request);

    let reply = handle_hello_connection(&mut stream, &handler(), peer()).unwrap();
    let response_bytes = &stream.get_ref()[request_len..];
    let (frame, decoded_reply) = decode_response_frame(response_bytes);

    assert_eq!(frame.envelope_version, 1);
    assert_eq!(FrameKind::try_from(frame.kind), Ok(FrameKind::Response));
    assert_eq!(frame.payload_protocol, 0);
    assert_eq!(frame.request_id, 7);
    assert_eq!(
        frame.traceparent,
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    assert_eq!(reply, decoded_reply);

    match decoded_reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.backend_pipe, r"\\.\pipe\rpb-v1-test-backend");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn owner_policy_allows_matching_peer_before_hello_handling() {
    let request = encode_framed_frame(&frame_for_hello(&hello(std::process::id())));
    let request_len = request.len();
    let mut stream = Cursor::new(request);
    let policy = PeerCredentialPolicy::owner_only("owner-1");

    let reply = handle_hello_connection_with_peer_policy(
        &mut stream,
        &handler(),
        peer_with_owner("owner-1"),
        &policy,
    )
    .unwrap()
    .expect("matching owner should be handled");

    let response_bytes = &stream.get_ref()[request_len..];
    let (_frame, decoded_reply) = decode_response_frame(response_bytes);
    assert_eq!(reply, decoded_reply);
    assert!(matches!(
        decoded_reply.result,
        Some(HelloReplyResult::Negotiated(_))
    ));
}

#[test]
fn owner_policy_drops_foreign_peer_before_reading_hello() {
    let request = encode_framed_frame(&frame_for_hello(&hello(std::process::id())));
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

#[test]
fn owner_policy_rejects_empty_expected_owner() {
    let policy = PeerCredentialPolicy::owner_only("");

    assert!(!policy.allows(&peer_with_owner("")));
    assert!(!policy.allows(&peer_with_owner("owner-1")));
}

#[test]
fn current_user_policy_uses_non_empty_platform_owner() {
    let policy = PeerCredentialPolicy::current_user().expect("current user policy");

    match policy {
        PeerCredentialPolicy::OwnerOnly { uid_or_sid } => {
            assert!(!uid_or_sid.is_empty());
        }
        PeerCredentialPolicy::AllowAny => panic!("current user policy must be owner-only"),
    }
}

#[test]
fn malformed_frame_body_gets_refused_response_frame() {
    let mut request = Vec::new();
    write_frame(&mut request, &[0xFF, 0xFF, 0xFF]).unwrap();
    let request_len = request.len();
    let mut stream = Cursor::new(request);

    let returned_reply = handle_hello_connection(&mut stream, &handler(), peer()).unwrap();

    let response_bytes = &stream.get_ref()[request_len..];
    let (frame, reply) = decode_response_frame(response_bytes);
    assert_eq!(returned_reply, reply);
    assert_eq!(FrameKind::try_from(frame.kind), Ok(FrameKind::Response));
    assert_eq!(decoded_reply_code(&reply), ErrorCode::ErrorPeerRejected);
    match reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => {
            assert_eq!(
                ErrorCode::try_from(refused.code),
                Ok(ErrorCode::ErrorPeerRejected)
            );
            assert_eq!(refused.reason, "malformed broker Frame");
        }
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refusal, got {negotiated:?}")
        }
    }
}

fn decoded_reply_code(reply: &HelloReply) -> ErrorCode {
    match reply.result.as_ref().unwrap() {
        HelloReplyResult::Refused(refused) => ErrorCode::try_from(refused.code).unwrap(),
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refused, got negotiated {negotiated:?}")
        }
    }
}

#[test]
fn serve_one_local_socket_round_trips_hello() {
    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || serve_one_local_socket(&server_socket, &handler()));

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let mut client = connect_with_retry(name);
    let request_frame = frame_for_hello(&hello(0));
    write_frame(&mut client, &request_frame.encode_to_vec()).unwrap();

    let response_bytes = read_frame(&mut client).unwrap();
    let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
    let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();

    let server_reply = server.join().unwrap().unwrap();
    assert_eq!(server_reply, reply);
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    assert_eq!(response_frame.request_id, 7);
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn serve_one_local_socket_times_out_a_silent_peer_and_cleans_up() {
    let _env_lock = crate::HELLO_TIMEOUT_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _timeout = crate::HelloTimeoutOverride::new("50");

    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || serve_one_local_socket(&server_socket, &handler()));
    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let client = connect_with_retry(name);

    let started = Instant::now();
    let error = server.join().unwrap().unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        matches!(
        error,
        BrokerConnectionError::Framing(FramingError::Io(ref error))
            if error.kind() == io::ErrorKind::TimedOut
        ),
        "unexpected serve-once error: {error:?}"
    );
    drop(client);
    #[cfg(unix)]
    assert!(!std::path::Path::new(&socket_name).exists());
}

#[test]
fn serve_one_local_socket_current_user_policy_allows_same_user() {
    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let policy = PeerCredentialPolicy::current_user().expect("current user policy");
    let server = thread::spawn(move || {
        let handler = handler();
        serve_one_local_socket_with_peer_policy(&server_socket, &handler, &policy)
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let mut client = connect_with_retry(name);
    let request_frame = frame_for_hello(&hello(0));
    write_frame(&mut client, &request_frame.encode_to_vec()).unwrap();

    let response_bytes = read_frame(&mut client).unwrap();
    let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
    let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
    let server_reply = server.join().unwrap().unwrap().expect("same user allowed");

    assert_eq!(server_reply, reply);
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

#[test]
fn serve_local_socket_connections_handles_concurrent_hellos() {
    const CLIENTS: usize = 100;

    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        serve_local_socket_connections(&server_socket, Arc::new(handler()), CLIENTS)
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let mut clients = Vec::with_capacity(CLIENTS);
    for index in 0..CLIENTS {
        let name = name.clone();
        clients.push(thread::spawn(move || {
            let mut client = connect_with_retry(name);
            let mut request = hello(0);
            request.request_id = format!("req-{index}");
            let mut frame = frame_for_hello(&request);
            frame.request_id = (index + 1) as u64;
            write_frame(&mut client, &frame.encode_to_vec()).unwrap();

            let response_bytes = read_frame(&mut client).unwrap();
            let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
            assert_eq!(
                FrameKind::try_from(response_frame.kind),
                Ok(FrameKind::Response)
            );
            assert_eq!(response_frame.request_id, (index + 1) as u64);
            let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
            match reply.result.unwrap() {
                HelloReplyResult::Negotiated(negotiated) => negotiated.connection_id,
                HelloReplyResult::Refused(refused) => {
                    panic!("unexpected refusal for client {index}: {refused:?}")
                }
            }
        }));
    }

    let mut connection_ids = Vec::with_capacity(CLIENTS);
    for client in clients {
        connection_ids.push(client.join().unwrap());
    }
    server.join().unwrap().unwrap();

    connection_ids.sort_unstable();
    connection_ids.dedup();
    assert_eq!(connection_ids.len(), CLIENTS);
    assert_eq!(connection_ids[0], 1);
    assert_eq!(connection_ids[CLIENTS - 1], CLIENTS as u64);
}

#[test]
fn serve_local_socket_connections_handles_herd_at_attachment_cap() {
    const MAX_BROKER_PIPE_ATTACHMENTS: usize = 64;

    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        serve_local_socket_connections(
            &server_socket,
            Arc::new(handler()),
            MAX_BROKER_PIPE_ATTACHMENTS,
        )
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let herd = Arc::new(Barrier::new(MAX_BROKER_PIPE_ATTACHMENTS));
    let mut clients = Vec::with_capacity(MAX_BROKER_PIPE_ATTACHMENTS);
    for index in 0..MAX_BROKER_PIPE_ATTACHMENTS {
        let name = name.clone();
        let herd = Arc::clone(&herd);
        clients.push(thread::spawn(move || {
            let mut client = connect_with_retry(name);
            herd.wait();

            let mut request = hello(0);
            request.request_id = format!("req-herd-{index}");
            let mut frame = frame_for_hello(&request);
            frame.request_id = (index + 1) as u64;
            write_frame(&mut client, &frame.encode_to_vec()).unwrap();

            let response_bytes = read_frame(&mut client).unwrap();
            let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
            assert_eq!(
                FrameKind::try_from(response_frame.kind),
                Ok(FrameKind::Response)
            );
            assert_eq!(response_frame.request_id, (index + 1) as u64);
            let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
            match reply.result.unwrap() {
                HelloReplyResult::Negotiated(negotiated) => negotiated.connection_id,
                HelloReplyResult::Refused(refused) => {
                    panic!("unexpected refusal for herd client {index}: {refused:?}")
                }
            }
        }));
    }

    let mut connection_ids = Vec::with_capacity(MAX_BROKER_PIPE_ATTACHMENTS);
    for client in clients {
        connection_ids.push(client.join().unwrap());
    }
    server.join().unwrap().unwrap();

    connection_ids.sort_unstable();
    assert_eq!(
        connection_ids,
        (1..=MAX_BROKER_PIPE_ATTACHMENTS as u64).collect::<Vec<_>>()
    );
}

#[test]
fn serve_local_socket_connections_rejects_admission_after_attachment_cap() {
    const ADMISSION_CAP: usize = 64;
    const OVERFLOW_ATTEMPTS: usize = 16;

    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        serve_local_socket_connections(&server_socket, Arc::new(handler()), ADMISSION_CAP)
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let herd = Arc::new(Barrier::new(ADMISSION_CAP));
    let mut clients = Vec::with_capacity(ADMISSION_CAP);
    for index in 0..ADMISSION_CAP {
        let name = name.clone();
        let herd = Arc::clone(&herd);
        clients.push(thread::spawn(move || {
            let mut client = connect_with_retry(name);
            herd.wait();

            let mut request = hello(0);
            request.request_id = format!("req-overcap-{index}");
            let mut frame = frame_for_hello(&request);
            frame.request_id = (index + 1) as u64;
            write_frame(&mut client, &frame.encode_to_vec()).unwrap();

            let response_bytes = read_frame(&mut client).unwrap();
            let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
            assert_eq!(
                FrameKind::try_from(response_frame.kind),
                Ok(FrameKind::Response)
            );
            let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
            match reply.result.unwrap() {
                HelloReplyResult::Negotiated(negotiated) => negotiated.connection_id,
                HelloReplyResult::Refused(refused) => {
                    panic!("unexpected refusal for admitted client {index}: {refused:?}")
                }
            }
        }));
    }

    let mut connection_ids = Vec::with_capacity(ADMISSION_CAP);
    for client in clients {
        connection_ids.push(client.join().unwrap());
    }
    server.join().unwrap().unwrap();
    connection_ids.sort_unstable();

    assert_eq!(
        connection_ids,
        (1..=ADMISSION_CAP as u64).collect::<Vec<_>>(),
        "the broker must assign IDs only to the admitted attachment budget"
    );
    for _ in 0..OVERFLOW_ATTEMPTS {
        assert!(
            try_connect_with_deadline(name.clone(), Duration::from_millis(100)).is_err(),
            "over-cap clients must not be admitted after the bounded accept loop returns"
        );
    }
}

#[test]
fn serve_local_socket_connections_refuses_malformed_hello_flood() {
    const MALFORMED_CLIENTS: usize = 96;

    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        serve_local_socket_connections(&server_socket, Arc::new(handler()), MALFORMED_CLIENTS)
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let herd = Arc::new(Barrier::new(MALFORMED_CLIENTS));
    let mut clients = Vec::with_capacity(MALFORMED_CLIENTS);
    for index in 0..MALFORMED_CLIENTS {
        let name = name.clone();
        let herd = Arc::clone(&herd);
        clients.push(thread::spawn(move || {
            let mut client = connect_with_retry(name);
            herd.wait();

            let expected_reason = if index % 2 == 0 {
                write_frame(&mut client, &[0xFF, 0xFF, 0xFF]).unwrap();
                "malformed broker Frame"
            } else {
                let mut frame = frame_for_hello(&hello(0));
                frame.request_id = (index + 1) as u64;
                frame.payload = vec![0xFF, 0xFF, 0xFF];
                write_frame(&mut client, &frame.encode_to_vec()).unwrap();
                "malformed Hello payload"
            };

            let response_bytes = read_frame(&mut client).unwrap();
            let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
            assert_eq!(
                FrameKind::try_from(response_frame.kind),
                Ok(FrameKind::Response)
            );
            let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
            match reply.result.unwrap() {
                HelloReplyResult::Refused(refused) => {
                    assert_eq!(
                        ErrorCode::try_from(refused.code),
                        Ok(ErrorCode::ErrorPeerRejected)
                    );
                    assert_eq!(refused.reason, expected_reason);
                }
                HelloReplyResult::Negotiated(negotiated) => {
                    panic!("expected refusal for malformed client {index}, got {negotiated:?}")
                }
            }
        }));
    }

    for client in clients {
        client.join().unwrap();
    }
    server.join().unwrap().unwrap();
}

#[test]
fn serve_local_socket_connections_rate_limits_concurrent_hello_flood() {
    if std::env::consts::OS == "macos" {
        eprintln!(
            "skipping live peer-PID rate-limit proof on macOS: local socket peer credentials do \
             not expose a verified peer PID"
        );
        return;
    }

    const RATE_LIMIT: usize = 8;
    const CLIENTS: usize = 32;

    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        let handler = handler().with_rate_limit(RATE_LIMIT as u32, Duration::from_secs(60));
        serve_local_socket_connections(&server_socket, Arc::new(handler), CLIENTS)
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let herd = Arc::new(Barrier::new(CLIENTS));
    let mut clients = Vec::with_capacity(CLIENTS);
    for index in 0..CLIENTS {
        let name = name.clone();
        let herd = Arc::clone(&herd);
        clients.push(thread::spawn(move || {
            let mut client = connect_with_retry(name);
            herd.wait();

            let mut request = hello(0);
            request.request_id = format!("req-rate-limit-{index}");
            let mut frame = frame_for_hello(&request);
            frame.request_id = (index + 1) as u64;
            write_frame(&mut client, &frame.encode_to_vec()).unwrap();

            let response_bytes = read_frame(&mut client).unwrap();
            let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
            assert_eq!(
                FrameKind::try_from(response_frame.kind),
                Ok(FrameKind::Response)
            );
            assert_eq!(response_frame.request_id, (index + 1) as u64);
            let reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
            match reply.result.unwrap() {
                HelloReplyResult::Negotiated(negotiated) => Ok(negotiated.connection_id),
                HelloReplyResult::Refused(refused) => {
                    assert_eq!(
                        ErrorCode::try_from(refused.code),
                        Ok(ErrorCode::ErrorRateLimited)
                    );
                    assert_eq!(refused.reason, "Hello rate limit exceeded");
                    assert!(
                        refused.retry_after_ms > 0,
                        "rate-limited clients should receive a retry hint"
                    );
                    Err(())
                }
            }
        }));
    }

    let mut connection_ids = Vec::with_capacity(RATE_LIMIT);
    let mut rate_limited = 0;
    for client in clients {
        match client.join().unwrap() {
            Ok(connection_id) => connection_ids.push(connection_id),
            Err(()) => rate_limited += 1,
        }
    }
    server.join().unwrap().unwrap();

    connection_ids.sort_unstable();
    assert_eq!(
        connection_ids,
        (1..=RATE_LIMIT as u64).collect::<Vec<_>>(),
        "only the per-peer rate-limit budget should negotiate"
    );
    assert_eq!(
        rate_limited,
        CLIENTS - RATE_LIMIT,
        "every over-budget client should receive ERROR_RATE_LIMITED"
    );
}

fn connect_with_retry(
    name: interprocess::local_socket::Name<'static>,
) -> interprocess::local_socket::Stream {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match LocalSocketStream::connect(name.borrow()) {
            Ok(stream) => return stream,
            Err(err) if Instant::now() < deadline => {
                if !is_pending_bind_error(&err) {
                    panic!("failed to connect to broker local socket: {err}");
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("timed out connecting to broker local socket: {err}"),
        }
    }
}

fn try_connect_with_deadline(
    name: interprocess::local_socket::Name<'static>,
    timeout: Duration,
) -> io::Result<interprocess::local_socket::Stream> {
    let deadline = Instant::now() + timeout;
    loop {
        match LocalSocketStream::connect(name.borrow()) {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() < deadline && is_pending_bind_error(&err) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}

fn is_pending_bind_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
    )
}

fn unique_socket_name() -> String {
    crate::socket_common::unique_socket_name("serve-once")
}
