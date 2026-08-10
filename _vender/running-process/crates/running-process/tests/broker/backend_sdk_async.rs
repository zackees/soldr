//! Async end-to-end coverage for the #414 async backend SDK surface:
//! `BackendHandle::probe_with_service_async` plus
//! `AsyncFrameClient::request` running against the same
//! `BackendEndpointMux`-backed daemon shape used by the blocking e2e
//! in `backend_sdk.rs`.

#![cfg(feature = "client-async")]

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerOptions;
use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::backend_sdk::{
    AsyncFrameClient, BackendEndpointMux, LegacyClassification, MuxPoll,
};
use running_process::broker::protocol::{encode_framed, Endpoint, Frame};
use running_process::broker::server::local_socket_name;

use crate::backend_handle_common;

running_process::register_payload_protocol! {
    /// Private-use lane for this async test daemon.
    const ASYNC_TEST_PAYLOAD_PROTOCOL: u32 = 0xF414;
}

/// Same serve shape as `backend_sdk::serve_connection` — kept duplicated
/// here so the async test does not cross-link into a sibling test file
/// (cargo's test build model treats them as independent).
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
        let mux =
            BackendEndpointMux::new(daemon, &[ASYNC_TEST_PAYLOAD_PROTOCOL], |_buf: &[u8]| {
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
fn async_mux_daemon_answers_probe_and_serves_frame_client() {
    let endpoint = unix_only_socket_endpoint();
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).expect("identity");
    // Connection 1: BackendHandle::probe_with_service_async.
    // Connection 2: AsyncFrameClient.
    let server = spawn_mux_daemon(daemon.clone(), 2);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let handle = BackendHandle::probe_with_service_async(
            "backend-sdk-async-test",
            "1.0.0",
            &endpoint,
            &daemon,
        )
        .await
        .expect("mux-backed daemon must answer the async identity probe");
        assert_eq!(handle.service_name, "backend-sdk-async-test");
        assert!(handle.is_alive());

        let mut client = AsyncFrameClient::connect(&endpoint)
            .await
            .expect("async connect");
        for round in 1..=3u64 {
            let payload = format!("ping-{round}").into_bytes();
            let response = client
                .request(ASYNC_TEST_PAYLOAD_PROTOCOL, payload.clone())
                .await
                .expect("async request round-trip");
            let mut expected = b"pong:".to_vec();
            expected.extend_from_slice(&payload);
            assert_eq!(response.payload, expected, "round {round}");
            assert_eq!(response.request_id, round);
        }
        drop(client);
    });

    server.join().expect("daemon thread").expect("daemon serve");
}

#[test]
fn async_frame_client_connect_timeout_is_loud() {
    // No listener is bound — connect should fail (refused or timeout)
    // without hanging forever.
    let endpoint = unix_only_socket_endpoint();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    runtime.block_on(async {
        let result = AsyncFrameClient::connect_with_timeout(
            &endpoint,
            std::time::Duration::from_millis(250),
        )
        .await;
        assert!(result.is_err(), "connect must fail without a listener");
    });
}
