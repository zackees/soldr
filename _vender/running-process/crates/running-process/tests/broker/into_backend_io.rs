//! End-to-end coverage for [`BrokerSession::into_backend_io`] (#720).
//!
//! A consumer adopts through the broker, then — instead of speaking the
//! FrameV1 request/response wire — takes ownership of the live negotiated
//! socket and runs its own raw protocol over it. On Unix the handed-back
//! [`OwnedFd`](std::os::fd::OwnedFd) is wrapped in a
//! [`std::os::unix::net::UnixStream`] and proven live with a raw echo
//! round-trip. On Windows the `OwnedHandle` path is deferred, so
//! `into_backend_io()` reports [`IntoBackendIoError::WindowsUnsupported`].

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

use interprocess::local_socket::traits::Listener as _;
use running_process::broker::adopt::BrokerSession;
#[cfg(all(unix, feature = "client-async"))]
use running_process::broker::adopt::{AsyncBrokerSession, OwnedConnectRequest};
use running_process::broker::client::{BackendConnectionRoute, ConnectBackendRequest};
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    handle_hello_connection, HelloHandler, PeerIdentity, RegisteredBackend,
};

use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, cleanup_test_socket, unique_socket_name,
};

const IO_SERVICE: &str = "io-test";
const IO_VERSION: &str = "1.0.0";

fn io_service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: IO_SERVICE.into(),
        binary_path: "/usr/local/bin/io-test".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: "1.0.0".into(),
        version_allow_list: vec![IO_VERSION.into()],
        labels: Default::default(),
    }
}

/// Broker that negotiates one Hello and points the client at `backend_socket`.
fn spawn_broker(
    broker_socket: String,
    backend_socket: String,
) -> thread::JoinHandle<io::Result<()>> {
    let display = broker_socket.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&broker_socket, &ready_tx)?;
        let handler = HelloHandler::new()
            .with_backend(RegisteredBackend {
                service_definition: io_service_definition(),
                daemon_version: IO_VERSION.into(),
                backend_pipe: backend_socket.clone(),
                server_capabilities: 0x01,
            })
            .map_err(|err| io::Error::other(err.to_string()))?;
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "io-peer".into(),
        };
        handle_hello_connection(&mut stream, &handler, peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&broker_socket);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display);
    handle
}

/// Backend that accepts one connection and echoes raw bytes until EOF.
///
/// Unlike the [`BackendEndpointMux`](running_process::broker::backend_sdk::BackendEndpointMux)
/// app daemons, this speaks no frame wire: it proves the socket handed back by
/// `into_backend_io()` is the live negotiated connection by mirroring whatever
/// the consumer writes over it directly.
fn spawn_raw_echo_backend(backend_socket: String) -> thread::JoinHandle<io::Result<()>> {
    let display = backend_socket.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&backend_socket, &ready_tx)?;
        let mut stream = listener.accept()?;
        let mut chunk = [0u8; 4096];
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            stream.write_all(&chunk[..read])?;
            stream.flush()?;
        }
        cleanup_test_socket(&backend_socket);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display);
    handle
}

#[cfg(unix)]
#[test]
fn into_backend_io_hands_off_live_unix_socket() {
    use std::os::unix::net::UnixStream;

    let broker_socket = unique_socket_name("io-broker");
    let backend_socket = unique_socket_name("io-backend");

    let backend = spawn_raw_echo_backend(backend_socket.clone());
    let broker = spawn_broker(broker_socket.clone(), backend_socket.clone());

    let request = ConnectBackendRequest::new(&broker_socket, IO_SERVICE, IO_VERSION, IO_VERSION);
    let session = BrokerSession::adopt(request).expect("broker session adopt");
    assert_eq!(session.route(), BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(session.endpoint(), backend_socket);

    let io = session.into_backend_io().expect("into_backend_io on unix");
    let fd = io.into_owned_fd();
    let mut raw = UnixStream::from(fd);

    let proof = b"live-socket-proof";
    raw.write_all(proof).expect("write over raw socket");
    raw.flush().expect("flush");
    let mut echoed = vec![0u8; proof.len()];
    raw.read_exact(&mut echoed).expect("read raw echo");
    assert_eq!(
        &echoed, proof,
        "raw socket must echo over the live connection"
    );

    drop(raw);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[cfg(windows)]
#[test]
fn into_backend_io_is_unsupported_on_windows() {
    use running_process::broker::adopt::IntoBackendIoError;

    let broker_socket = unique_socket_name("io-broker-win");
    let backend_socket = unique_socket_name("io-backend-win");

    let backend = spawn_raw_echo_backend(backend_socket.clone());
    let broker = spawn_broker(broker_socket.clone(), backend_socket.clone());

    let request = ConnectBackendRequest::new(&broker_socket, IO_SERVICE, IO_VERSION, IO_VERSION);
    let session = BrokerSession::adopt(request).expect("broker session adopt");
    assert_eq!(session.route(), BackendConnectionRoute::BrokerNegotiated);

    let err = session
        .into_backend_io()
        .expect_err("into_backend_io must be unsupported on Windows for now");
    assert!(
        matches!(err, IntoBackendIoError::WindowsUnsupported),
        "expected WindowsUnsupported, got {err:?}"
    );

    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}

#[cfg(all(unix, feature = "client-async"))]
#[test]
fn async_into_backend_io_hands_off_live_unix_socket() {
    use std::os::unix::net::UnixStream;

    let broker_socket = unique_socket_name("io-broker-async");
    let backend_socket = unique_socket_name("io-backend-async");

    let backend = spawn_raw_echo_backend(backend_socket.clone());
    let broker = spawn_broker(broker_socket.clone(), backend_socket.clone());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let fd = runtime.block_on(async {
        let request =
            OwnedConnectRequest::new(broker_socket.clone(), IO_SERVICE, IO_VERSION, IO_VERSION);
        let session = AsyncBrokerSession::adopt(request)
            .await
            .expect("async broker session adopt");
        assert_eq!(session.route(), BackendConnectionRoute::BrokerNegotiated);
        session
            .into_backend_io()
            .expect("async into_backend_io on unix")
            .into_owned_fd()
    });

    let mut raw = UnixStream::from(fd);
    let proof = b"async-live-socket-proof";
    raw.write_all(proof).expect("write over raw socket");
    raw.flush().expect("flush");
    let mut echoed = vec![0u8; proof.len()];
    raw.read_exact(&mut echoed).expect("read raw echo");
    assert_eq!(
        &echoed, proof,
        "raw socket must echo over the live connection"
    );

    drop(raw);
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
}
