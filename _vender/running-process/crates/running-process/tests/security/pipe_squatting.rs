use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::{fs, path::Path};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::ListenerOptions;
use prost::Message;
use running_process::broker::protocol::{
    write_frame, BrokerIsolation, Frame, FrameKind, Hello, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    local_socket_name, serve_local_socket_connections, HelloHandler, RegisteredBackend,
};

#[test]
fn broker_bind_refuses_precreated_control_socket() {
    let socket_name = unique_socket_name();
    cleanup_test_socket(&socket_name);
    let attacker_listener = precreate_local_socket(&socket_name);

    let (result_tx, result_rx) = mpsc::channel();
    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        let result = serve_local_socket_connections(&server_socket, Arc::new(handler()), 1)
            .map_err(|err| err.to_string());
        result_tx.send(result).unwrap();
    });

    let result = match result_rx.recv_timeout(Duration::from_millis(300)) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            send_hello_to_possible_broker(&socket_name);
            result_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("broker bind path blocked after a pre-created socket")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("broker bind thread exited without reporting a result")
        }
    };

    drop(attacker_listener);
    cleanup_test_socket(&socket_name);
    server.join().unwrap();

    assert!(
        result.is_err(),
        "broker bind path silently served on a pre-created socket"
    );
}

fn precreate_local_socket(socket_name: &str) -> interprocess::local_socket::Listener {
    prepare_test_socket(socket_name);
    let name = local_socket_name(socket_name).unwrap();
    ListenerOptions::new().name(name).create_sync().unwrap()
}

fn prepare_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let path = Path::new(socket_name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let _ = fs::remove_file(path);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

fn send_hello_to_possible_broker(socket_name: &str) {
    let name = local_socket_name(socket_name).unwrap().into_owned();
    if let Ok(mut client) = LocalSocketStream::connect(name.borrow()) {
        let request_frame = frame_for_hello(&hello());
        let _ = write_frame(&mut client, &request_frame.encode_to_vec());
    }
}

fn handler() -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: "rpb-v1-test-backend".into(),
            server_capabilities: 0x01,
        })
        .unwrap()
}

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

fn hello() -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-pipe-squatting".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: env!("CARGO_PKG_VERSION").into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn frame_for_hello(hello: &Hello) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello.encode_to_vec(),
        request_id: 241,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn unique_socket_name() -> String {
    crate::socket_common::unique_socket_name("security-pipe-squat")
}
