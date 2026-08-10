//! End-to-end coverage for the #412 backend integration SDK: a daemon
//! built on [`BackendEndpointMux`] answers real `BackendHandle` probes
//! and serves sequential [`FrameClient`] requests on one endpoint,
//! with identity persistence through the JSON sidecar helpers.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::backend_sdk::{
    read_daemon_identity_file, remove_daemon_identity_file, write_daemon_identity_file,
    BackendEndpointMux, FrameClient, LegacyClassification, MuxPoll,
};
use running_process::broker::protocol::{encode_framed, Endpoint, Frame};
use running_process::broker::server::local_socket_name;

use crate::backend_handle_common;

running_process::register_payload_protocol! {
    /// Private-use lane for this test daemon.
    const TEST_PAYLOAD_PROTOCOL: u32 = 0xF412;
}

/// Serve one accepted connection through the mux: answer probes, echo
/// consumer payloads as `pong:<payload>`, until the peer disconnects.
///
/// This is the canonical consumer accept-loop shape documented in
/// `docs/INTEGRATE.md` — keep the two in sync.
fn serve_connection<S, F>(stream: &mut S, mux: &BackendEndpointMux<F>) -> io::Result<()>
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
                    if buf.is_empty() {
                        return Ok(());
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "peer closed mid-message",
                    ));
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
                return Err(io::Error::other(
                    "test daemon has no legacy wire but mux classified bytes as legacy",
                ));
            }
        }
    }
}

/// Spawn a mux-backed daemon that serves exactly `connections` accepted
/// connections, then exits.
fn spawn_mux_daemon(
    daemon: DaemonProcess,
    connections: usize,
) -> thread::JoinHandle<io::Result<()>> {
    let endpoint_path = daemon.ipc_endpoint.path.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let handle = thread::spawn(move || {
        let name = local_socket_name(&endpoint_path)?;
        let listener = match ListenerOptions::new().name(name).create_sync() {
            Ok(listener) => {
                ready_tx.send(Ok(())).expect("ready channel");
                listener
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err.to_string()));
                return Err(err);
            }
        };
        let mux = BackendEndpointMux::new(daemon, &[TEST_PAYLOAD_PROTOCOL], |_buf: &[u8]| {
            LegacyClassification::NotLegacy
        });
        for _ in 0..connections {
            let mut stream = listener.accept()?;
            serve_connection(&mut stream, &mux)?;
        }
        Ok(())
    });
    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => handle,
        Ok(Err(err)) => panic!("mux daemon failed to bind: {err}"),
        Err(err) => panic!("timed out waiting for mux daemon bind: {err}"),
    }
}

fn unix_only_socket_endpoint() -> Endpoint {
    backend_handle_common::test_endpoint()
}

#[test]
fn mux_daemon_answers_backend_handle_probe_and_serves_frame_client() {
    let endpoint = unix_only_socket_endpoint();
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).expect("identity");
    // Connection 1: BackendHandle probe. Connection 2: FrameClient.
    let server = spawn_mux_daemon(daemon.clone(), 2);

    let handle = BackendHandle::probe_with_service("backend-sdk-test", "1.0.0", &endpoint, &daemon)
        .expect("mux-backed daemon must answer the real identity probe");
    assert_eq!(handle.service_name, "backend-sdk-test");
    assert!(handle.is_alive());

    let mut client = FrameClient::connect(&endpoint).expect("connect");
    for round in 1..=3u64 {
        let payload = format!("ping-{round}").into_bytes();
        let response = client
            .request(TEST_PAYLOAD_PROTOCOL, payload.clone())
            .expect("request round-trip");
        let mut expected = b"pong:".to_vec();
        expected.extend_from_slice(&payload);
        assert_eq!(response.payload, expected, "round {round}");
        assert_eq!(response.request_id, round);
    }
    drop(client);

    server.join().expect("daemon thread").expect("daemon serve");
}

#[test]
fn identity_sidecar_feeds_backend_handle_probe() {
    let endpoint = unix_only_socket_endpoint();
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).expect("identity");

    let dir = std::env::temp_dir().join(format!("rp-backend-sdk-sidecar-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("sidecar dir");
    let sidecar = dir.join("daemon-identity.json");
    write_daemon_identity_file(&sidecar, &daemon).expect("write sidecar");

    // A separate "client process" reads the sidecar and probes with it.
    let expected = read_daemon_identity_file(&sidecar).expect("sidecar present");
    assert_eq!(expected, daemon);

    let server = spawn_mux_daemon(daemon, 1);
    let handle = BackendHandle::probe_with_service(
        "backend-sdk-test",
        "1.0.0",
        &expected.ipc_endpoint,
        &expected,
    )
    .expect("probe via sidecar identity");
    assert_eq!(handle.daemon_process.pid, std::process::id());
    server.join().expect("daemon thread").expect("daemon serve");

    remove_daemon_identity_file(&sidecar);
    assert!(read_daemon_identity_file(&sidecar).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The service-keyed identity file has to survive the round trip between two
/// processes that never talk to each other: a daemon writes it, a broker
/// reads it, and each derives the path from `daemon_identity_path` alone.
///
/// What this proves: an identity published under a service name is readable
/// from that service name alone, and stops resolving once retracted.
///
/// What it does NOT prove — worth stating, because the wording is tempting —
/// is that a publisher and reader deriving the path *independently* agree.
/// It cannot: both sides call `daemon_identity_path`, so there is only one
/// derivation and no drift to detect. Confirmed by changing that function's
/// format string, which the test happily follows. Single-source-of-truth is
/// the mechanism preventing drift here; this test only guards the round trip
/// on top of it (running-process#532).
#[test]
fn a_service_identity_round_trips_through_its_derived_path() {
    use running_process::broker::lifecycle::names_v2::daemon_identity_path;
    use running_process::broker::protocol::Endpoint;
    use running_process::broker::secure_dir::ensure_private_dir;

    let service = format!("rt-test-{}", std::process::id());
    let path = daemon_identity_path(&service);
    ensure_private_dir(path.parent().expect("identity path has a parent")).expect("private dir");

    let endpoint = Endpoint {
        namespace_id: String::new(),
        path: "test-endpoint-path".to_string(),
    };
    let daemon = DaemonProcess::current_process(endpoint, None).expect("identity for this process");
    write_daemon_identity_file(&path, &daemon).expect("publish");

    // Read it back the way a broker would: from the service name only.
    let read_back = read_daemon_identity_file(&daemon_identity_path(&service))
        .expect("a published identity is readable from its derived path");
    assert_eq!(read_back.pid, std::process::id());
    assert_eq!(read_back.ipc_endpoint.path, "test-endpoint-path");

    remove_daemon_identity_file(&path);
    assert!(
        read_daemon_identity_file(&daemon_identity_path(&service)).is_none(),
        "a retracted identity must not still resolve"
    );
}
