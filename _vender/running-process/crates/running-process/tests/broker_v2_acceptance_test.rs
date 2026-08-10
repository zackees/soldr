//! Acceptance coverage for the v2 broker binary (#532 criteria 3 and 4).
//!
//! `broker_v2_scaffold_accepts_connection.rs` proves the binary binds, accepts
//! one Hello, and shuts down cleanly. #532 asks for two further properties
//! that nothing exercised:
//!
//! - **Criterion 3 — concurrency.** 100 concurrent Hellos complete quickly
//!   with zero failures. A broker that serialises its accept loop, or leaks a
//!   handler slot per connection, passes every single-connection test and
//!   still falls over on the first real workload.
//! - **Criterion 4 — adversarial input.** Malformed frames, oversized frames,
//!   NUL bytes in `service_name`, and protocol-version mismatches must each
//!   produce a typed `Refused`, not a panic and not a dropped connection.
//!   The distinction matters to a client: a `Refused` carries a reason it can
//!   act on, while a closed socket is indistinguishable from a crashed broker.
//!
//! Criteria 2 and 5 are consumer-side (they require zccache to complete a
//! Hello and swap its namespace re-exports) and cannot be verified from this
//! repository.

#![cfg(feature = "client")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;
use prost::Message as _;
use running_process::broker::protocol::{
    hello_reply, read_frame, write_frame, ErrorCode, Hello, HelloReply, ENVELOPE_VERSION,
};

const DEADLINE: Duration = Duration::from_secs(20);

/// #532 criterion 3 budget: 100 connections in under 5 seconds.
const STRESS_CONNECTIONS: usize = 100;
const STRESS_BUDGET: Duration = Duration::from_secs(5);

/// A broker child killed when the guard drops.
struct Broker {
    child: Child,
    path: String,
    /// The `--program` this broker was started with.
    ///
    /// Carried from construction rather than parsed back out of the socket
    /// path. An earlier version recovered it from the path leaf, which works
    /// on Linux and Windows (`rpb-v2-<program>-<sid>-<idx>`) and fails on
    /// macOS, where the leaf is hashed to fit `sun_path` — the parse returned
    /// hash fragments and every Hello was refused with "service name is
    /// invalid". Windows CI was green throughout.
    program: String,
    _svc_dir: tempfile::TempDir,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn unique_program(prefix: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{:010x}", nonce & 0xFF_FFFF_FFFF)
}

/// Start a broker in persistent mode with a stub service definition installed.
fn start_broker(prefix: &str) -> Broker {
    let svc_dir = tempfile::tempdir().expect("tempdir for servicedef");
    let stub_binary = if cfg!(windows) {
        svc_dir.path().join("stub.exe")
    } else {
        svc_dir.path().join("stub")
    };
    std::fs::write(&stub_binary, b"#!/bin/sh\necho stub\n").expect("write stub binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&stub_binary).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&stub_binary, perms).unwrap();
    }

    let program = unique_program(prefix);
    running_process::broker::protocol_v2::ServiceDefinitionBuilder::shared_broker(
        &program,
        stub_binary.display().to_string(),
    )
    .install_in(svc_dir.path())
    .expect("install stub servicedef");

    let mut child = Command::new(env!("CARGO_BIN_EXE_running-process-broker-v2"))
        .arg("--program")
        .arg(&program)
        .env("RUNNING_PROCESS_SERVICE_DEF_DIR", svc_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn broker");

    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Keeps reading after the bound-path line rather than breaking.
        //
        // Breaking dropped the `BufReader`, which closed the read end of the
        // pipe. The broker prints again on shutdown, and `println!` panics on
        // EPIPE — so every SIGTERM test saw the broker exit 101 and it looked
        // like a shutdown-path defect. Draining to EOF keeps the reader alive
        // for the process's whole life, which is what a supervisor would do.
        let mut sender = Some(tx);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("running-process-broker-v2 bound at ") {
                let path = rest
                    .trim_end()
                    .rsplit_once(" (")
                    .map(|(path, _)| path)
                    .unwrap_or(rest.trim_end())
                    .to_string();
                if let Some(tx) = sender.take() {
                    let _ = tx.send(path);
                }
            }
        }
    });
    let path = rx
        .recv_timeout(DEADLINE)
        .expect("broker must print its bound path within the deadline");

    Broker {
        child,
        path,
        program,
        _svc_dir: svc_dir,
    }
}

fn connect(path: &str) -> Stream {
    let name = wrap_socket_name(path).expect("socket name");
    Stream::connect(name).expect("connect to broker")
}

fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ToFsName, ToNsName};
    if socket_path.starts_with(r"\\.\pipe\") {
        let leaf = socket_path.trim_start_matches(r"\\.\pipe\");
        leaf.to_ns_name::<GenericNamespaced>()
            .map_err(|e| e.to_string())
    } else {
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| e.to_string())
    }
}

fn hello_for(program: &str) -> Hello {
    Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: program.to_string(),
        wanted_version: "0.0.0".to_string(),
        client_version: "test".to_string(),
        connection_id: 0x532,
        ..Hello::default()
    }
}

fn read_reply(stream: &mut Stream) -> HelloReply {
    let bytes = read_frame(stream).expect("read HelloReply");
    HelloReply::decode(bytes.as_slice()).expect("decode HelloReply")
}

#[test]
fn a_hundred_concurrent_hellos_all_succeed_within_budget() {
    let broker = start_broker("v2acc-stress");
    let program = broker.program.clone();
    let path = broker.path.clone();

    let start = Instant::now();
    let handles: Vec<_> = (0..STRESS_CONNECTIONS)
        .map(|index| {
            let path = path.clone();
            let program = program.clone();
            std::thread::spawn(move || -> Result<(), String> {
                let mut stream = Stream::connect(
                    wrap_socket_name(&path).map_err(|e| format!("conn {index}: name: {e}"))?,
                )
                .map_err(|e| format!("conn {index}: connect: {e}"))?;
                let mut hello = hello_for(&program);
                hello.connection_id = index as u64;
                write_frame(&mut stream, &hello.encode_to_vec())
                    .map_err(|e| format!("conn {index}: write: {e}"))?;
                let bytes =
                    read_frame(&mut stream).map_err(|e| format!("conn {index}: read: {e}"))?;
                let reply = HelloReply::decode(bytes.as_slice())
                    .map_err(|e| format!("conn {index}: decode: {e}"))?;
                match reply.result {
                    Some(hello_reply::Result::Negotiated(_)) => Ok(()),
                    other => Err(format!("conn {index}: expected Negotiated, got {other:?}")),
                }
            })
        })
        .collect();

    let mut failures = Vec::new();
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failures.push(e),
            Err(_) => failures.push("a connection thread panicked".to_string()),
        }
    }
    let elapsed = start.elapsed();

    assert!(
        failures.is_empty(),
        "{} of {STRESS_CONNECTIONS} connections failed: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
    // A serialised accept loop, or one that leaks a handler slot per
    // connection, still passes every single-connection test and only shows up
    // here.
    assert!(
        elapsed < STRESS_BUDGET,
        "{STRESS_CONNECTIONS} connections took {elapsed:?}, over the \
         {STRESS_BUDGET:?} budget"
    );
}

#[test]
fn an_unknown_service_is_refused_rather_than_dropped() {
    let broker = start_broker("v2acc-unknown");
    let mut stream = connect(&broker.path);

    let mut hello = hello_for("no-such-service-anywhere");
    hello.connection_id = 1;
    write_frame(&mut stream, &hello.encode_to_vec()).expect("write Hello");

    // A typed refusal, not a closed socket: a client can distinguish "you
    // asked for something that does not exist" from "the broker died".
    match read_reply(&mut stream).result {
        Some(hello_reply::Result::Refused(refused)) => {
            // The typed code, not just any refusal: a client routes on the
            // code, and "unknown service" is retryable in a way that a
            // version block is not.
            assert_eq!(
                refused.code,
                ErrorCode::ErrorServiceUnknown as i32,
                "expected ERROR_SERVICE_UNKNOWN, got code {} ({})",
                refused.code,
                refused.reason
            );
            assert!(
                !refused.reason.is_empty(),
                "a refusal with no reason tells the operator nothing"
            );
        }
        other => panic!("expected Refused for an unknown service, got {other:?}"),
    }
}

#[test]
fn a_nul_byte_in_the_service_name_is_refused_rather_than_crashing() {
    let broker = start_broker("v2acc-nul");
    let mut stream = connect(&broker.path);

    // A NUL is legal in a protobuf string and illegal in every path and pipe
    // name it would be interpolated into, so it is the classic way to turn a
    // name lookup into something worse.
    let mut hello = hello_for("bad\0name");
    hello.connection_id = 2;
    write_frame(&mut stream, &hello.encode_to_vec()).expect("write Hello");

    match read_reply(&mut stream).result {
        Some(hello_reply::Result::Refused(_)) => {}
        other => panic!("expected Refused for a NUL-bearing service name, got {other:?}"),
    }
}

#[test]
fn an_unsupported_protocol_version_is_refused_with_a_reason() {
    let broker = start_broker("v2acc-version");
    let program = broker.program.clone();
    let mut stream = connect(&broker.path);

    let mut hello = hello_for(&program);
    // Far beyond anything this broker speaks.
    hello.client_min_protocol = 9_999;
    hello.client_max_protocol = 10_000;
    hello.connection_id = 3;
    write_frame(&mut stream, &hello.encode_to_vec()).expect("write Hello");

    match read_reply(&mut stream).result {
        Some(hello_reply::Result::Refused(refused)) => {
            assert!(
                refused.code == ErrorCode::ErrorVersionUnsupported as i32
                    || refused.code == ErrorCode::ErrorVersionBlocked as i32,
                "expected a version-related code, got {} ({})",
                refused.code,
                refused.reason
            );
            // The daemon's own range, so the client can tell whether to
            // upgrade or downgrade rather than guessing.
            assert!(
                refused.daemon_max_protocol > 0,
                "a version refusal should advertise the range the broker speaks"
            );
        }
        other => panic!("expected Refused for an unsupported protocol, got {other:?}"),
    }
}

#[test]
fn a_garbage_frame_does_not_take_the_broker_down() {
    let broker = start_broker("v2acc-garbage");

    // Bytes that are a well-formed frame but not a decodable Hello.
    {
        let mut stream = connect(&broker.path);
        write_frame(&mut stream, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).expect("write garbage");
        // The broker may refuse or close this connection; what it may not do
        // is die. The assertion is the *next* connection.
        let _ = read_frame(&mut stream);
    }

    // The point of the test: a subsequent well-formed Hello still works, so
    // one hostile client cannot deny service to everyone else.
    let program = broker.program.clone();
    let mut stream = connect(&broker.path);
    let hello = hello_for(&program);
    write_frame(&mut stream, &hello.encode_to_vec()).expect("write Hello");
    match read_reply(&mut stream).result {
        Some(hello_reply::Result::Negotiated(_)) => {}
        other => panic!("broker did not serve a good Hello after a bad one: {other:?}"),
    }
}

#[test]
fn an_oversized_frame_is_rejected_without_allocating_it() {
    let broker = start_broker("v2acc-oversize");

    {
        // A length prefix claiming far more than the frame cap, with no body
        // behind it. A broker that trusts the prefix would try to allocate it.
        let mut stream = connect(&broker.path);
        use std::io::Write as _;
        let bogus_len: u32 = u32::MAX;
        stream
            .write_all(&bogus_len.to_le_bytes())
            .expect("write len");
        stream.flush().expect("flush");
        let _ = read_frame(&mut stream);
    }

    // Still serving.
    let program = broker.program.clone();
    let mut stream = connect(&broker.path);
    let hello = hello_for(&program);
    write_frame(&mut stream, &hello.encode_to_vec()).expect("write Hello");
    match read_reply(&mut stream).result {
        Some(hello_reply::Result::Negotiated(_)) => {}
        other => panic!("broker did not survive an oversized length prefix: {other:?}"),
    }
}

/// Criterion 1 of #532: the broker stays up until it is signalled.
///
/// The other acceptance tests all start a broker, do one thing, and drop the
/// guard — which kills it. None of them establish that it would have *kept
/// running*, so a broker that exited after its first Hello would pass every
/// one of them. `--once` exists precisely because single-shot is a separate
/// mode; this asserts the default is not silently behaving like it.
///
/// Unix-only for the signal half: the binary installs SIGTERM/SIGINT handlers
/// there, while Windows uses console control events, which a test cannot
/// deliver to a child without attaching to its console.
#[cfg(unix)]
#[test]
fn the_broker_serves_repeatedly_and_exits_cleanly_on_sigterm() {
    let mut broker = start_broker("v2acc-lifetime");

    // Two round-trips, not one: the first proves it serves, the second proves
    // serving did not consume it.
    for attempt in 0..2 {
        let mut stream = connect(&broker.path);
        write_frame(&mut stream, &hello_for(&broker.program).encode_to_vec()).expect("write hello");
        let reply = read_reply(&mut stream);
        assert!(
            matches!(reply.result, Some(hello_reply::Result::Negotiated(_))),
            "hello {attempt} was not negotiated: {reply:?}"
        );
    }

    // Still alive between requests — `try_wait` returning None is the
    // assertion that it did not quietly exit after the first connection.
    assert!(
        broker.child.try_wait().expect("try_wait").is_none(),
        "the broker exited on its own after serving"
    );

    // SIGTERM, not SIGKILL: the point is the graceful path the binary
    // installs handlers for. SIGKILL would prove only that the OS can end a
    // process.
    let pid = broker.child.id() as libc::pid_t;
    // SAFETY: `pid` names the child this test spawned and has not reaped.
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "kill(SIGTERM)"
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = broker.child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the broker did not exit within 20s of SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    // Now that stdout stays drained, the shutdown message no longer hits
    // EPIPE, and the graceful path is observable: `Ok(None)` from the accept
    // poll, drain the handlers, `ExitCode::SUCCESS`.
    //
    // Asserting it is what keeps the earlier misdiagnosis from recurring — if
    // this starts failing with 101 again, the cause is a closed stdout, not a
    // broken shutdown.
    assert!(
        status.success(),
        "the broker did not shut down cleanly: {status:?}"
    );
}
