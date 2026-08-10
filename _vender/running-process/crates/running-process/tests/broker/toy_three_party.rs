//! Toy three-party broker example for issue #433.
//!
//! This is the *minimal* end-to-end shape a consumer (zccache, soldr, clud,
//! fbuild — #434-#437) must implement to adopt v1 of the broker API. It wires
//! all three contract parties together in one process so the whole handshake
//! is visible in a single test:
//!
//! ```text
//!   CLIENT          BROKER DAEMON                 APP DAEMON
//!   (consumer)      (running-process)             (consumer backend)
//!   ----------      ------------------            ------------------
//!   connect_to_backend(ConnectBackendRequest)
//!        |  Hello{service,version}  ->  handle_hello_connection()
//!        |  <- HelloReply::Negotiated{ backend_pipe }
//!        |----------------------- connect(backend_pipe) ----------> accept()
//!   FrameClient::from_stream(conn.stream)
//!        |  Frame{TOY_PROTO, "ping"} ----------------------------> BackendEndpointMux::poll()
//!        |  <- Frame{TOY_PROTO, "pong:ping"} ---------------------- serve loop
//! ```
//!
//! The three contracts under test:
//!   * CLIENT — `connect_to_backend` (broker Hello negotiation + dial).
//!   * BROKER — `handle_hello_connection` + `HelloHandler` (returns the app
//!     daemon endpoint as `backend_pipe`).
//!   * APP DAEMON — `BackendEndpointMux` accept-loop (answers identity probes
//!     automatically, serves consumer payload frames).

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

use interprocess::local_socket::traits::Listener as _;
use running_process::broker::adopt::BrokerSession;
#[cfg(feature = "client-async")]
use running_process::broker::adopt::{AsyncBrokerSession, OwnedConnectRequest};
use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{
    BackendEndpointMux, FrameClient, LegacyClassification, MuxPoll,
};
use running_process::broker::client::{
    connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::protocol::{
    encode_framed, BrokerIsolation, Endpoint, Frame, ServiceDefinition,
};
use running_process::broker::server::{
    handle_hello_connection, HelloHandler, PeerIdentity, RegisteredBackend,
};

use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, cleanup_test_socket, unique_socket_name,
};

const TOY_SERVICE: &str = "toy-service";
const TOY_VERSION: &str = "1.0.0";

running_process::register_payload_protocol! {
    /// Private-use lane (0xF000..=0xFFFF) for this toy consumer's payloads.
    const TOY_PAYLOAD_PROTOCOL: u32 = 0xF433;
}

// ---------------------------------------------------------------------------
// APP DAEMON contract: a BackendEndpointMux accept-loop.
//
// The mux owns the wire classification. Each `poll` over the accumulated
// buffer either asks for more bytes, answers an identity probe for us, or
// hands us a decoded consumer `Frame` to serve. A daemon with no legacy wire
// always reports `NotLegacy`.
// ---------------------------------------------------------------------------

fn serve_one_connection<S, F>(stream: &mut S, mux: &BackendEndpointMux<F>) -> io::Result<()>
where
    S: Read + Write,
    F: Fn(&[u8]) -> LegacyClassification,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match mux.poll(&buf).map_err(io::Error::other)? {
            MuxPoll::NeedMoreBytes => {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    return if buf.is_empty() {
                        Ok(())
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "peer closed mid-message",
                        ))
                    };
                }
                buf.extend_from_slice(&chunk[..read]);
            }
            MuxPoll::ProbeAnswered { reply, consumed } => {
                stream.write_all(&reply)?;
                stream.flush()?;
                buf.drain(..consumed);
            }
            MuxPoll::Payload { frame, consumed } => {
                buf.drain(..consumed);
                let mut payload = b"pong:".to_vec();
                payload.extend_from_slice(&frame.payload);
                let response = Frame::response_to(&frame, payload);
                let wire = encode_framed(&response).map_err(io::Error::other)?;
                stream.write_all(&wire)?;
                stream.flush()?;
            }
            MuxPoll::Legacy => {
                return Err(io::Error::other("toy daemon has no legacy wire"));
            }
        }
    }
}

fn spawn_app_daemon(
    socket_name: String,
    daemon: DaemonProcess,
) -> thread::JoinHandle<io::Result<()>> {
    let display = socket_name.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&socket_name, &ready_tx)?;
        let mux = BackendEndpointMux::new(daemon, &[TOY_PAYLOAD_PROTOCOL], |_buf: &[u8]| {
            LegacyClassification::NotLegacy
        });
        let mut stream = listener.accept()?;
        serve_one_connection(&mut stream, &mux)?;
        cleanup_test_socket(&socket_name);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display);
    handle
}

// ---------------------------------------------------------------------------
// BROKER DAEMON contract: a HelloHandler that resolves the service to a
// registered backend and replies HelloReply::Negotiated{ backend_pipe }.
//
// In production the broker discovers/spawns the app daemon and learns its
// endpoint; here we register it directly. `handle_hello_connection` performs
// the version negotiation and writes the reply.
// ---------------------------------------------------------------------------

fn toy_service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: TOY_SERVICE.into(),
        binary_path: "/usr/local/bin/toy-service".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: "1.0.0".into(),
        version_allow_list: vec![TOY_VERSION.into()],
        labels: Default::default(),
    }
}

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
                service_definition: toy_service_definition(),
                daemon_version: TOY_VERSION.into(),
                backend_pipe: backend_socket.clone(),
                server_capabilities: 0x01,
            })
            .map_err(|err| io::Error::other(err.to_string()))?;
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "toy-peer".into(),
        };
        handle_hello_connection(&mut stream, &handler, peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&broker_socket);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display);
    handle
}

// ---------------------------------------------------------------------------
// CLIENT contract: connect_to_backend negotiates through the broker, then the
// returned raw stream is wrapped in a FrameClient for request/response.
// ---------------------------------------------------------------------------

#[test]
fn toy_three_party_client_broker_app_daemon_round_trip() {
    let broker_socket = unique_socket_name("toy-broker");
    let backend_socket = unique_socket_name("toy-backend");

    // APP DAEMON: build its identity and start its mux accept-loop.
    let endpoint = Endpoint {
        namespace_id: "toy".into(),
        path: backend_socket.clone(),
    };
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).expect("daemon identity");
    let app = spawn_app_daemon(backend_socket.clone(), daemon);

    // BROKER: start the Hello responder that points clients at the app daemon.
    let broker = spawn_broker(broker_socket.clone(), backend_socket.clone());

    // CLIENT: negotiate through the broker, then talk frames to the app daemon.
    let request = ConnectBackendRequest::new(&broker_socket, TOY_SERVICE, TOY_VERSION, TOY_VERSION);
    let connection = connect_to_backend(request).expect("broker negotiation");

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_socket);
    assert_eq!(
        connection.negotiated.as_ref().unwrap().daemon_version,
        TOY_VERSION
    );

    let mut client = FrameClient::from_stream(connection.stream);
    let response = client
        .request(TOY_PAYLOAD_PROTOCOL, b"ping".to_vec())
        .expect("frame round-trip");

    assert_eq!(response.payload, b"pong:ping");
    assert_eq!(response.payload_protocol, TOY_PAYLOAD_PROTOCOL);
    drop(client);

    broker.join().unwrap().unwrap();
    app.join().unwrap().unwrap();
}

/// #433 R1: the same round-trip via the one-call [`BrokerSession::adopt`]
/// entry point — negotiate, dial, and wrap in a frame client in a single call,
/// instead of `connect_to_backend` + `FrameClient::from_stream` by hand.
#[test]
fn toy_three_party_broker_session_adopt_round_trip() {
    let broker_socket = unique_socket_name("toy-broker-adopt");
    let backend_socket = unique_socket_name("toy-backend-adopt");

    let endpoint = Endpoint {
        namespace_id: "toy".into(),
        path: backend_socket.clone(),
    };
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).expect("daemon identity");
    let app = spawn_app_daemon(backend_socket.clone(), daemon);
    let broker = spawn_broker(broker_socket.clone(), backend_socket.clone());

    let request = ConnectBackendRequest::new(&broker_socket, TOY_SERVICE, TOY_VERSION, TOY_VERSION);
    let mut session = BrokerSession::adopt(request).expect("broker session adopt");

    assert_eq!(session.route(), BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(session.endpoint(), backend_socket);
    assert_eq!(session.negotiated().unwrap().daemon_version, TOY_VERSION);

    let response = session
        .request(TOY_PAYLOAD_PROTOCOL, b"ping".to_vec())
        .expect("frame round-trip");
    assert_eq!(response.payload, b"pong:ping");
    drop(session);

    broker.join().unwrap().unwrap();
    app.join().unwrap().unwrap();
}

/// #433 R3: the adopt round-trip on the async path. A tokio daemon uses
/// [`AsyncBrokerSession::adopt`] (negotiation runs on `spawn_blocking`) and then
/// `.await`s the same frame round-trip the blocking session gives.
#[cfg(feature = "client-async")]
#[test]
fn toy_three_party_async_broker_session_adopt_round_trip() {
    let broker_socket = unique_socket_name("toy-broker-async");
    let backend_socket = unique_socket_name("toy-backend-async");

    let endpoint = Endpoint {
        namespace_id: "toy".into(),
        path: backend_socket.clone(),
    };
    let daemon = DaemonProcess::current_process(endpoint, Some(30)).expect("daemon identity");
    let app = spawn_app_daemon(backend_socket.clone(), daemon);
    let broker = spawn_broker(broker_socket.clone(), backend_socket.clone());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let request =
            OwnedConnectRequest::new(broker_socket.clone(), TOY_SERVICE, TOY_VERSION, TOY_VERSION);
        let mut session = AsyncBrokerSession::adopt(request)
            .await
            .expect("async broker session adopt");

        assert_eq!(session.route(), BackendConnectionRoute::BrokerNegotiated);
        assert_eq!(session.endpoint(), backend_socket);
        assert_eq!(session.negotiated().unwrap().daemon_version, TOY_VERSION);

        let response = session
            .request(TOY_PAYLOAD_PROTOCOL, b"ping".to_vec())
            .await
            .expect("frame round-trip");
        assert_eq!(response.payload, b"pong:ping");
    });

    broker.join().unwrap().unwrap();
    app.join().unwrap().unwrap();
}
