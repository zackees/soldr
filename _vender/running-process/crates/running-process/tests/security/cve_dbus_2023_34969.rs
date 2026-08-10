use std::io::Cursor;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::{fs, path::Path};

use interprocess::local_socket::prelude::*;
use prost::Message;
use running_process::broker::protocol::{
    BrokerIsolation, Frame, FrameKind, Hello, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    handle_hello_connection_with_peer_policy, local_socket_name,
    serve_local_socket_connections_with_peer_policy, HelloHandler, PeerCredentialPolicy,
    PeerIdentity, RegisteredBackend,
};

#[test]
fn foreign_peer_is_dropped_before_payload_identity_spoofing() {
    let mut stream = Cursor::new(Vec::new());
    running_process::broker::protocol::write_frame(
        &mut stream,
        &frame_for_hello(current_process_id()).encode_to_vec(),
    )
    .unwrap();
    stream.set_position(0);

    let reply = handle_hello_connection_with_peer_policy(
        &mut stream,
        &handler(),
        PeerIdentity {
            pid: current_process_id(),
            uid_or_sid: account_id("attacker").into(),
        },
        &PeerCredentialPolicy::owner_only(account_id("owner")),
    )
    .unwrap();

    assert!(
        reply.is_none(),
        "foreign peers must be dropped before trusting spoofable Hello fields"
    );
    assert_eq!(
        stream.position(),
        0,
        "owner-only rejection should not consume attacker-controlled payload bytes"
    );
}

#[test]
fn rejected_peer_accept_loop_releases_local_socket_path() {
    let socket_name = unique_socket_name();
    cleanup_test_socket(&socket_name);

    let server_socket = socket_name.clone();
    let server = thread::spawn(move || {
        serve_local_socket_connections_with_peer_policy(
            &server_socket,
            Arc::new(handler()),
            1,
            &PeerCredentialPolicy::owner_only(account_id("foreign-owner")),
        )
    });

    let mut client = connect_with_retry(&socket_name);
    running_process::broker::protocol::write_frame(
        &mut client,
        &frame_for_hello(current_process_id()).encode_to_vec(),
    )
    .unwrap();
    drop(client);

    server
        .join()
        .expect("broker accept thread should not panic")
        .expect("rejected peer should not poison accept-loop cleanup");

    #[cfg(unix)]
    assert!(
        !Path::new(&socket_name).exists(),
        "socket path should be removed after a rejected connection"
    );

    cleanup_test_socket(&socket_name);
}

fn connect_with_retry(socket_name: &str) -> interprocess::local_socket::Stream {
    let name = local_socket_name(socket_name).unwrap().into_owned();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match LocalSocketStream::connect(name.borrow()) {
            Ok(stream) => return stream,
            Err(err) if std::time::Instant::now() < deadline => {
                let _ = err;
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("failed to connect to broker test socket {socket_name:?}: {err}"),
        }
    }
}

fn handler() -> HelloHandler {
    HelloHandler::new().with_backend(backend()).unwrap()
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

fn frame_for_hello(peer_pid: u32) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello(peer_pid).encode_to_vec(),
        request_id: 241,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
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
        request_id: "req-dbus-cleanup".into(),
        connection_id: 0,
        peer_pid,
        client_lib_name: "running-process".into(),
        client_lib_version: env!("CARGO_PKG_VERSION").into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn current_process_id() -> u32 {
    std::process::id()
}

fn account_id(kind: &'static str) -> &'static str {
    if cfg!(windows) {
        match kind {
            "owner" => "S-1-5-21-1000",
            "attacker" => "S-1-5-21-2000",
            "foreign-owner" => "S-1-5-21-9999",
            _ => "S-1-5-21-9998",
        }
    } else {
        match kind {
            "owner" => "uid:1000",
            "attacker" => "uid:2000",
            "foreign-owner" => "uid:9999",
            _ => "uid:9998",
        }
    }
}

fn unique_socket_name() -> String {
    crate::socket_common::unique_socket_name("security-dbus-cleanup")
}

fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

fn platform_absolute_path(leaf: &str) -> String {
    if cfg!(windows) {
        format!(r"C:\running-process-test\{leaf}.exe")
    } else {
        format!("/opt/running-process-test/{leaf}")
    }
}
