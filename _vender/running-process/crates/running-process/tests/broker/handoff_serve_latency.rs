//! Serve-path latency evidence for the production handle-passing handoff
//! (#387 follow-up).
//!
//! Unlike `handoff_latency_e2e` (which benchmarks the orchestrators against
//! a pre-wiring harness), this benchmark drives the REAL
//! [`serve_registered_backend`] accept loop end to end and measures the
//! client-visible connect latency for both production routes:
//!
//! - **handoff**: the serve config opts in via `with_handoff_endpoint`; an
//!   opted-in [`connect_to_backend`] performs the full Hello negotiation,
//!   the broker runs the platform handoff (`DuplicateHandle` on Windows,
//!   `sendmsg(SCM_RIGHTS)` on Unix) against a real backend handoff
//!   listener speaking the production offer/ACK wire protocol, relays the
//!   handoff-ready EVENT, and the client adopts the connection that
//!   carried Hello (`BackendConnectionRoute::HandlePassed`).
//! - **reconnect**: the serve config leaves handoff disabled (the
//!   production default); a non-opted-in client performs the same Hello
//!   negotiation and reconnects through the negotiated `backend_pipe`
//!   (`BackendConnectionRoute::BrokerNegotiated`).
//!
//! Both timed regions end after one probe/reply byte round trip on the
//! resulting backend connection, so each sample proves the route actually
//! serves traffic. Methodology mirrors `handoff_latency_e2e`:
//! [`collect_latency_samples`] (5 warmup + 50 measured iterations,
//! monotonic `Instant` timing) summarized by [`summarize_latency_samples`]
//! at nearest-rank P50/P99. The test asserts sample sanity and PRINTS the
//! measured numbers — it deliberately does not assert which route is
//! faster, so CI scheduler noise cannot flake it. Measured numbers are
//! recorded in `docs/v1-handoff-optimization.md`.

#![cfg(feature = "client")]

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Listener as _;
use running_process::broker::client::{
    connect_to_backend, BackendConnection, BackendConnectionRoute, ConnectBackendRequest,
};
use running_process::broker::server::handoff::{
    collect_latency_samples, summarize_latency_samples, HandoffLatencySummary,
};
use running_process::broker::server::serve_registered_backend;

use crate::handoff_serve_e2e::{
    serve_config, serve_one_handoff, spawn_configured_backend_probe, write_service_definition_dir,
    BackendBehavior, BACKEND_REPLY, CLIENT_PROBE,
};
use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, bind_test_socket, cleanup_test_socket,
    unique_socket_name,
};

const WARMUP_ITERATIONS: usize = 5;
const MEASURED_ITERATIONS: usize = 50;
const TOTAL_CONNECTIONS: usize = WARMUP_ITERATIONS + MEASURED_ITERATIONS;

#[test]
fn serve_path_handoff_vs_reconnect_latency_evidence() {
    let handoff = measure_serve_handoff();
    let reconnect = measure_serve_reconnect();
    println!(
        "serve-path handoff-latency[{os}]: handoff p50={}us p99={}us (n={}) \
         reconnect p50={}us p99={}us (n={})",
        handoff.p50.as_micros(),
        handoff.p99.as_micros(),
        handoff.sample_count,
        reconnect.p50.as_micros(),
        reconnect.p99.as_micros(),
        reconnect.sample_count,
        os = std::env::consts::OS,
    );
}

/// Measure the opted-in handoff route through the production serve loop.
fn measure_serve_handoff() -> HandoffLatencySummary {
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name("handoff-serve-lat");
    // No listener is ever re-bound on the backend endpoint after the
    // startup probe: a wrong fallback to reconnect would fail loudly.
    let backend_endpoint = unique_socket_name("handoff-serve-lat-be");
    let handoff_endpoint = unique_socket_name("handoff-serve-lat-ho");
    let backend_probe = spawn_configured_backend_probe(&backend_endpoint);
    let handoff_backend = spawn_backend_handoff_loop(handoff_endpoint.clone());
    let config = serve_config(
        tmp.path().join("services").as_path(),
        socket_name.clone(),
        backend_endpoint,
        TOTAL_CONNECTIONS,
    )
    .with_handoff_endpoint(handoff_endpoint);
    let server = thread::spawn(move || serve_registered_backend(config));

    let samples = collect_latency_samples(WARMUP_ITERATIONS, MEASURED_ITERATIONS, || {
        // Timed region: the full client-visible connect — Hello through the
        // real serve loop, platform handoff, handoff-ready relay, adoption —
        // plus one probe/reply round trip proving the adopted socket serves.
        let started = Instant::now();
        let mut connection = connect_serve_client(&socket_name, true);
        let reply = probe_roundtrip(&mut connection);
        let elapsed = started.elapsed();
        assert_eq!(connection.route, BackendConnectionRoute::HandlePassed);
        assert_eq!(reply, BACKEND_REPLY);
        drop(connection);
        elapsed
    });

    server.join().unwrap().unwrap();
    backend_probe.join().unwrap().unwrap();
    handoff_backend.join().unwrap().unwrap();
    assert_sane(&samples, "serve-path handoff")
}

/// Measure the reconnect route through the same serve loop with handoff
/// left disabled (no handoff endpoint — the production default).
fn measure_serve_reconnect() -> HandoffLatencySummary {
    let tmp = write_service_definition_dir();
    let socket_name = unique_socket_name("reconn-serve-lat");
    let backend_endpoint = unique_socket_name("reconn-serve-lat-be");
    let backend_probe = spawn_configured_backend_probe(&backend_endpoint);
    let config = serve_config(
        tmp.path().join("services").as_path(),
        socket_name.clone(),
        backend_endpoint.clone(),
        TOTAL_CONNECTIONS,
    );
    let server = thread::spawn(move || serve_registered_backend(config));
    // The startup probe owns the backend endpoint until verification ends;
    // only then can the reconnect listener take its place.
    backend_probe.join().unwrap().unwrap();
    let reconnect_backend = spawn_reconnect_probe_loop(backend_endpoint);

    let samples = collect_latency_samples(WARMUP_ITERATIONS, MEASURED_ITERATIONS, || {
        // Timed region: the full client-visible connect — Hello through the
        // real serve loop plus the backend_pipe reconnect — plus the same
        // probe/reply round trip as the handoff benchmark.
        let started = Instant::now();
        let mut connection = connect_serve_client(&socket_name, false);
        let reply = probe_roundtrip(&mut connection);
        let elapsed = started.elapsed();
        assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
        assert_eq!(reply, BACKEND_REPLY);
        drop(connection);
        elapsed
    });

    server.join().unwrap().unwrap();
    reconnect_backend.join().unwrap().unwrap();
    assert_sane(&samples, "serve-path reconnect")
}

/// One probe byte out, one reply byte back, on the backend connection.
fn probe_roundtrip(connection: &mut BackendConnection) -> u8 {
    connection.stream.write_all(&[CLIENT_PROBE]).unwrap();
    let mut reply = [0_u8; 1];
    connection.stream.read_exact(&mut reply).unwrap();
    reply[0]
}

/// Client connect through the broker, retrying only while the broker
/// socket is not yet bound. Each successful dial performs one full Hello
/// negotiation through the production serve loop.
fn connect_serve_client(broker_endpoint: &str, adopt: bool) -> BackendConnection {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut request =
            ConnectBackendRequest::new(broker_endpoint, "zccache", "1.11.20", "1.11.20");
        request.adopt_handed_off_connection = adopt;
        request.handoff_ready_timeout = Duration::from_secs(10);
        match connect_to_backend(request) {
            Ok(connection) => return connection,
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!("timed out connecting through broker {broker_endpoint}: {err}");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Long-lived backend handoff listener: one production offer/ACK exchange
/// (plus adopted-connection probe service) per benchmark iteration.
fn spawn_backend_handoff_loop(handoff_endpoint: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = handoff_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&handoff_endpoint, &ready_tx)?;
        for _ in 0..TOTAL_CONNECTIONS {
            let mut stream = listener.accept()?;
            serve_one_handoff(&mut stream, &BackendBehavior::Accept)?;
        }
        cleanup_test_socket(&handoff_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

/// Long-lived reconnect backend: accept one `backend_pipe` connection per
/// benchmark iteration and serve the probe/reply exchange. Binding retries
/// while the just-closed startup-probe listener still holds the pipe name.
fn spawn_reconnect_probe_loop(backend_endpoint: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = backend_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let listener = loop {
            match bind_test_socket(&backend_endpoint) {
                Ok(listener) => break listener,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return Err(error);
                }
            }
        };
        ready_tx.send(Ok(())).unwrap();
        for _ in 0..TOTAL_CONNECTIONS {
            let mut stream = listener.accept()?;
            let mut probe = [0_u8; 1];
            stream.read_exact(&mut probe)?;
            if probe != [CLIENT_PROBE] {
                return Err(io::Error::other(
                    "unexpected probe byte on reconnect backend",
                ));
            }
            stream.write_all(&[BACKEND_REPLY])?;
        }
        cleanup_test_socket(&backend_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

/// Sanity gate shared by both benchmarks: all iterations produced a
/// sample, every sample is non-zero, and the percentiles are ordered.
fn assert_sane(samples: &[Duration], label: &str) -> HandoffLatencySummary {
    assert_eq!(
        samples.len(),
        MEASURED_ITERATIONS,
        "{label} benchmark must collect every measured iteration"
    );
    assert!(
        samples.iter().all(|sample| *sample > Duration::ZERO),
        "{label} samples must be non-zero monotonic-clock durations"
    );
    let summary = summarize_latency_samples(samples).expect("non-empty sample set");
    assert!(
        summary.p50 <= summary.p99,
        "{label} P50 must not exceed P99"
    );
    summary
}
