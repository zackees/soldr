#![cfg(feature = "client")]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, write_frame, BrokerIsolation, Frame,
    FrameKind, Hello, HelloReply, PayloadEncoding, ServiceDefinition,
};
use running_process::broker::server::{
    enforce_hello_latency_budget, local_socket_name, serve_local_socket_connections,
    summarize_hello_latencies, HelloHandler, PerfGuardError, RegisteredBackend, HELLO_P50_BUDGET,
    HELLO_P99_BUDGET, HELLO_PERF_GUARD_ENV, HELLO_PERF_SAMPLE_COUNT,
};

const HELLO_PERF_WARMUP_COUNT: usize = 256;

#[test]
fn hello_perf_budget_constants_are_frozen() {
    assert_eq!(HELLO_PERF_SAMPLE_COUNT, 10_000);
    assert_eq!(HELLO_P50_BUDGET, Duration::from_micros(200));
    assert_eq!(HELLO_P99_BUDGET, Duration::from_millis(1));
}

#[test]
fn summarize_hello_latencies_uses_nearest_rank_percentiles() {
    let samples = [
        Duration::from_micros(300),
        Duration::from_micros(100),
        Duration::from_micros(200),
        Duration::from_micros(400),
    ];

    let summary = summarize_hello_latencies(&samples).unwrap();

    assert_eq!(summary.sample_count, 4);
    assert_eq!(summary.p50, Duration::from_micros(200));
    assert_eq!(summary.p99, Duration::from_micros(400));
}

#[test]
fn hello_perf_guard_accepts_samples_inside_budget() {
    let samples = vec![Duration::from_micros(100); HELLO_PERF_SAMPLE_COUNT];

    let summary = enforce_hello_latency_budget(&samples).unwrap();

    assert_eq!(summary.p50, Duration::from_micros(100));
    assert_eq!(summary.p99, Duration::from_micros(100));
}

#[test]
fn hello_perf_guard_rejects_too_few_samples() {
    let samples = vec![Duration::from_micros(100); HELLO_PERF_SAMPLE_COUNT - 1];

    let err = enforce_hello_latency_budget(&samples).unwrap_err();

    assert_eq!(
        err,
        PerfGuardError::TooFewSamples {
            required: HELLO_PERF_SAMPLE_COUNT,
            actual: HELLO_PERF_SAMPLE_COUNT - 1
        }
    );
}

#[test]
fn hello_perf_guard_rejects_slow_p50() {
    let samples = vec![Duration::from_micros(201); HELLO_PERF_SAMPLE_COUNT];

    let err = enforce_hello_latency_budget(&samples).unwrap_err();

    assert_eq!(
        err,
        PerfGuardError::P50Exceeded {
            actual: Duration::from_micros(201),
            budget: HELLO_P50_BUDGET
        }
    );
}

#[test]
fn hello_perf_guard_rejects_slow_p99() {
    let mut samples = vec![Duration::from_micros(100); HELLO_PERF_SAMPLE_COUNT];
    let slow_start = HELLO_PERF_SAMPLE_COUNT - 101;
    for sample in &mut samples[slow_start..] {
        *sample = Duration::from_millis(2);
    }

    let err = enforce_hello_latency_budget(&samples).unwrap_err();

    assert_eq!(
        err,
        PerfGuardError::P99Exceeded {
            actual: Duration::from_millis(2),
            budget: HELLO_P99_BUDGET
        }
    );
}

#[test]
fn real_socket_hello_roundtrip_perf_gate() {
    if std::env::var_os(HELLO_PERF_GUARD_ENV).is_none() {
        eprintln!("skipping real socket Hello perf gate; set {HELLO_PERF_GUARD_ENV}=1");
        return;
    }

    let samples = collect_real_socket_hello_samples(HELLO_PERF_SAMPLE_COUNT);
    let summary = enforce_hello_latency_budget(&samples).unwrap_or_else(|err| {
        let summary = summarize_hello_latencies(&samples).unwrap();
        panic!(
            "real socket Hello perf gate failed: {err}; samples={}, p50={:?}, p99={:?}",
            summary.sample_count, summary.p50, summary.p99
        );
    });

    eprintln!(
        "real socket Hello perf gate: samples={}, p50={:?}, p99={:?}",
        summary.sample_count, summary.p50, summary.p99
    );
}

fn collect_real_socket_hello_samples(sample_count: usize) -> Vec<Duration> {
    let socket_name = unique_socket_name();
    let server_socket = socket_name.clone();
    let total_connections = HELLO_PERF_WARMUP_COUNT + sample_count;
    let server = thread::spawn(move || {
        serve_local_socket_connections(&server_socket, Arc::new(handler()), total_connections)
    });

    let name = local_socket_name(&socket_name).unwrap().into_owned();
    let request = encoded_hello_frame();
    let mut samples = Vec::with_capacity(sample_count);
    for index in 0..total_connections {
        let mut client = connect_with_retry(name.clone());
        if index < HELLO_PERF_WARMUP_COUNT {
            roundtrip_hello(&mut client, &request);
            continue;
        }

        let started = Instant::now();
        let reply = roundtrip_hello(&mut client, &request);
        samples.push(started.elapsed());
        assert_negotiated(reply);
    }

    server.join().unwrap().unwrap();
    samples
}

fn roundtrip_hello(client: &mut interprocess::local_socket::Stream, request: &[u8]) -> HelloReply {
    write_frame(client, request).unwrap();
    let response_bytes = read_frame(client).unwrap();
    let response_frame = Frame::decode(response_bytes.as_slice()).unwrap();
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    HelloReply::decode(response_frame.payload.as_slice()).unwrap()
}

fn assert_negotiated(reply: HelloReply) {
    match reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refused) => panic!("unexpected refusal: {refused:?}"),
    }
}

fn encoded_hello_frame() -> Vec<u8> {
    let request = Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-perf".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    };
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: request.encode_to_vec(),
        request_id: 42,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    frame.encode_to_vec()
}

fn handler() -> HelloHandler {
    HelloHandler::new()
        .with_rate_limit(u32::MAX, Duration::from_secs(1))
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: "rpb-v1-perf-backend".into(),
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
    crate::socket_common::unique_socket_name("perf")
}
