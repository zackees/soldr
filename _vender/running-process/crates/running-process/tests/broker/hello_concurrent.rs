#![cfg(feature = "client")]

use std::sync::{Arc, Barrier};
use std::thread;

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, BrokerIsolation, Frame, FrameKind, Hello,
    PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{HelloHandler, PeerIdentity, RegisteredBackend};

const CLIENTS: usize = 100;

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

fn hello(index: usize) -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: format!("req-concurrent-{index}"),
        connection_id: 0,
        peer_pid: 0,
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

fn frame_for_hello(index: usize, hello: &Hello) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello.encode_to_vec(),
        request_id: (index + 1) as u64,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

#[test]
fn handler_assigns_deterministic_unique_connection_ids_under_concurrency() {
    let handler = Arc::new(handler());
    let barrier = Arc::new(Barrier::new(CLIENTS));
    let mut clients = Vec::with_capacity(CLIENTS);

    for index in 0..CLIENTS {
        let handler = Arc::clone(&handler);
        let barrier = Arc::clone(&barrier);
        clients.push(thread::spawn(move || {
            let request = hello(index);
            let frame = frame_for_hello(index, &request);
            barrier.wait();
            match handler.handle_frame(frame, peer()).result.unwrap() {
                HelloReplyResult::Negotiated(negotiated) => {
                    assert_eq!(negotiated.daemon_version, "1.11.20");
                    negotiated.connection_id
                }
                HelloReplyResult::Refused(refused) => {
                    panic!("unexpected refusal for client {index}: {refused:?}")
                }
            }
        }));
    }

    let mut connection_ids = clients
        .into_iter()
        .map(|client| client.join().unwrap())
        .collect::<Vec<_>>();
    connection_ids.sort_unstable();

    assert_eq!(connection_ids, (1..=CLIENTS as u64).collect::<Vec<_>>());
}
