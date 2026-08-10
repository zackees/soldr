//! Integration test for the slice 3c v2 broker scaffold.
//!
//! Spawns `running-process-broker-v2`, waits for it to print "bound at
//! <path>", connects to that path, and verifies the binary exits 0
//! within a deadline. Proves end-to-end that the v2 binary actually
//! claims a kernel resource on the host platform.

#![cfg(feature = "client")]

use std::io::{BufRead, BufReader};
use std::ops::{Deref, DerefMut};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::Stream;

const DEADLINE: Duration = Duration::from_secs(10);
const TEST_HELLO_TIMEOUT_MS: &str = "3000";

/// `--no-bind` short-circuit: the binary should print its banner and
/// exit 0 without touching the kernel namespace. Useful as a
/// platform-neutral build smoke test.
#[test]
fn binary_exits_clean_with_no_bind_flag() {
    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let output = Command::new(path)
        .arg("--no-bind")
        .output()
        .expect("spawn binary");

    assert!(
        output.status.success(),
        "--no-bind exit: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("running-process-broker-v2"),
        "expected version banner in stdout, got: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn sigterm_requests_bounded_clean_shutdown() {
    #[cfg(coverage)]
    let profiles_before = coverage_profile_files();

    let program = unique_program("sigterm");
    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let mut child = ChildGuard(
        Command::new(path)
            .arg("--program")
            .arg(&program)
            .env("RUNNING_PROCESS_BROKER_ALLOW_PRIVILEGED", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn broker"),
    );
    let pid = child.id();
    let (_socket_path, stdout_thread) =
        read_bound_path_bounded(child.stdout.take().expect("piped stdout"));

    // SAFETY: `pid` belongs to the live child we just spawned.
    let signal_result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    assert_eq!(
        signal_result,
        0,
        "send SIGTERM: {}",
        std::io::Error::last_os_error()
    );

    let exit = wait_with_deadline(&mut child, Duration::from_secs(3))
        .expect("broker must return through normal control flow after SIGTERM");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    stdout_thread.join().expect("stdout reader joins");
    assert!(
        exit.success(),
        "SIGTERM exit must be successful: {exit:?}\nstderr:\n{stderr}"
    );

    #[cfg(coverage)]
    assert_new_coverage_profiles_are_valid(&profiles_before);
}

/// Full bind/accept round-trip: spawn the binary, parse the bound path
/// from stdout, dial it, and assert the binary exits 0.
///
/// PR #533 added ServiceDefinitionLoader integration; the scaffold
/// service name has to be registered or the Hello returns
/// ErrorServiceUnknown. This test installs a stub servicedef in a
/// tempdir + points the broker at it via `RUNNING_PROCESS_SERVICE_DEF_DIR`.
#[test]
fn binary_binds_pipe_accepts_connection_and_exits() {
    // Install a stub servicedef so the broker's loader accepts the
    // test Hello. Per-test tempdir keeps concurrent runs isolated.
    let svc_dir = tempfile::tempdir().expect("tempdir for servicedef");
    let stub_binary = if cfg!(windows) {
        svc_dir.path().join("scaffold-stub.exe")
    } else {
        svc_dir.path().join("scaffold-stub")
    };
    std::fs::write(&stub_binary, b"#!/bin/sh\necho stub\n").expect("write stub binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&stub_binary).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&stub_binary, perms).unwrap();
    }
    // Use a unique --program per test invocation so concurrent / repeated
    // runs don't collide on the global per-user pipe namespace
    // (Windows ERROR_ACCESS_DENIED when an old broker on the same pipe
    // hasn't released yet).
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Keep total program length under v2_program_pipe's 64-char max
    // (the pipe name is `rpb-v2-<program>-<sid>-0` — sid is 16 hex,
    // and the final pipe name fits in Linux's UDS sun_path budget
    // after the per-OS prefix). 12-char nonce stays safely under.
    let program = format!("scaffold-{:012x}", nonce & 0xFFFF_FFFF_FFFF);
    running_process::broker::protocol_v2::ServiceDefinitionBuilder::shared_broker(
        &program,
        stub_binary.display().to_string(),
    )
    .install_in(svc_dir.path())
    .expect("install stub servicedef");

    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let mut child = Command::new(path)
        .arg("--once")
        .arg("--program")
        .arg(&program)
        .env("RUNNING_PROCESS_SERVICE_DEF_DIR", svc_dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn binary");

    let stdout = child
        .stdout
        .take()
        .expect("piped stdout must exist after spawn");
    let mut reader = BufReader::new(stdout);

    // Read until we see "bound at <path>" or timeout. The binary prints
    // the banner first, then the "bound at" line once the listener is
    // up, then waits for an `accept` call.
    let start = Instant::now();
    let mut socket_path: Option<String> = None;
    let mut all_stdout = String::new();
    while start.elapsed() < DEADLINE {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                all_stdout.push_str(&line);
                if let Some(rest) = line.strip_prefix("running-process-broker-v2 bound at ") {
                    // The full line is
                    //   `running-process-broker-v2 bound at <path> (program=<...>, mode=<...>)`
                    // — strip the trailing ` (program=..., mode=...)` so the
                    // captured value is just the socket path. Without this,
                    // the full suffix gets concatenated onto the "path" and
                    // pushes its length past Linux's `sun_path` 108-byte
                    // limit, surfacing as a misleading "exceeds capacity of
                    // sun_path" error from `Stream::connect` later.
                    let rest = rest.trim_end();
                    let path_only = rest.rsplit_once(" (").map(|(p, _)| p).unwrap_or(rest);
                    socket_path = Some(path_only.to_string());
                    break;
                }
            }
            Err(err) => {
                // best-effort cleanup
                let _ = child.kill();
                panic!("read stdout: {err}\ncaptured so far: {all_stdout}");
            }
        }
    }

    let socket_path = match socket_path {
        Some(p) => p,
        None => {
            let _ = child.kill();
            panic!(
                "did not observe 'bound at' line within {:?}; captured stdout:\n{all_stdout}",
                DEADLINE
            );
        }
    };

    // Dial the same pipe, run the full Hello / Negotiated round-trip,
    // and assert the binary echoes our connection_id back in its reply.
    let name = wrap_socket_name(&socket_path).expect("wrap_socket_name");
    let mut stream = Stream::connect(name).expect("connect to v2 broker pipe");

    use prost::Message;
    use running_process::broker::protocol::{
        hello_reply, read_frame, write_frame, Hello, HelloReply, ENVELOPE_VERSION,
    };

    let hello = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: program.clone(),
        wanted_version: "0.0.0".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "slice-3d-integration-test".to_string(),
        connection_id: 0xdead_beef,
        peer_pid: std::process::id(),
        client_lib_name: "slice-3d-test".to_string(),
        client_lib_version: env!("CARGO_PKG_VERSION").to_string(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 0,
    };
    let mut body = Vec::with_capacity(hello.encoded_len());
    hello.encode(&mut body).expect("encode Hello");
    write_frame(&mut stream, &body).expect("write Hello frame");

    let reply_bytes = read_frame(&mut stream).expect("read HelloReply frame");
    let reply = HelloReply::decode(reply_bytes.as_slice()).expect("decode HelloReply");
    let negotiated = match reply.result {
        Some(hello_reply::Result::Negotiated(n)) => n,
        Some(hello_reply::Result::Refused(r)) => panic!("expected Negotiated, got Refused: {r:?}"),
        None => panic!("HelloReply.result missing"),
    };
    assert_eq!(negotiated.negotiated_protocol, ENVELOPE_VERSION as u32);
    assert_eq!(negotiated.connection_id, 0xdead_beef);
    assert!(
        !negotiated.daemon_version.is_empty(),
        "daemon_version should be populated"
    );

    drop(stream);

    // Drain any remaining stdout so the binary can flush cleanly.
    let mut tail = String::new();
    let _ = reader.read_to_string(&mut tail);
    all_stdout.push_str(&tail);

    // Wait for exit — bounded by the same deadline.
    let exit = wait_with_deadline(&mut child, DEADLINE).expect("binary exited within deadline");
    assert!(
        exit.success(),
        "v2 binary exit: {:?}\nstdout: {all_stdout}",
        exit
    );

    assert!(
        all_stdout.contains("peer connected"),
        "expected 'peer connected' in stdout, got:\n{all_stdout}"
    );
    assert!(
        all_stdout.contains("Hello for service"),
        "expected Hello-handler log line in stdout, got:\n{all_stdout}"
    );
}

#[test]
fn silent_peer_does_not_hang_once_mode() {
    assert_once_stall_times_out(&[]);
}

#[test]
fn partial_frame_does_not_hang_once_mode() {
    assert_once_stall_times_out(&[running_process::broker::protocol::ENVELOPE_VERSION, 8, 0]);
}

fn assert_once_stall_times_out(initial_bytes: &[u8]) {
    let program = unique_program("once-timeout");
    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let mut child = ChildGuard(
        Command::new(path)
            .arg("--once")
            .arg("--program")
            .arg(&program)
            .env(
                "RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS",
                TEST_HELLO_TIMEOUT_MS,
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn binary"),
    );
    let (socket_path, stdout_thread) =
        read_bound_path_bounded(child.stdout.take().expect("piped stdout"));
    let name = wrap_socket_name(&socket_path).expect("wrap socket name");
    let mut stalled = Stream::connect(name).expect("connect stalled peer");
    if !initial_bytes.is_empty() {
        use std::io::Write as _;
        stalled
            .write_all(initial_bytes)
            .expect("write partial frame");
    }

    let exit = wait_with_deadline(&mut child, Duration::from_secs(6))
        .expect("--once must exit after the Hello deadline");
    assert!(!exit.success(), "a timed-out Hello must be an error");
    let stderr = {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("piped stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        stderr
    };
    assert!(
        stderr.contains("timed out"),
        "timeout must remain distinct from malformed/EOF; stderr:\n{stderr}"
    );
    stdout_thread.join().expect("stdout reader joins");
}

#[test]
fn silent_peers_release_all_handler_slots_after_deadline() {
    let program = unique_program("pool-timeout");
    let svc_dir = tempfile::tempdir().expect("service definition tempdir");
    let stub = svc_dir.path().join(if cfg!(windows) {
        "pool-stub.exe"
    } else {
        "pool-stub"
    });
    std::fs::write(&stub, b"stub").expect("write stub");
    running_process::broker::protocol_v2::ServiceDefinitionBuilder::shared_broker(
        &program,
        stub.display().to_string(),
    )
    .install_in(svc_dir.path())
    .expect("install service definition");

    let path = env!("CARGO_BIN_EXE_running-process-broker-v2");
    let mut child = ChildGuard(
        Command::new(path)
            .arg("--program")
            .arg(&program)
            .env("RUNNING_PROCESS_SERVICE_DEF_DIR", svc_dir.path())
            .env(
                "RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS",
                TEST_HELLO_TIMEOUT_MS,
            )
            .env("RUNNING_PROCESS_BROKER_MAX_INFLIGHT_HANDLERS", "4")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn binary"),
    );
    let (socket_path, stdout_thread) =
        read_bound_path_bounded(child.stdout.take().expect("piped stdout"));
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = stderr_tx.send(line);
        }
    });

    let mut silent = Vec::with_capacity(5);
    for _ in 0..=4 {
        let name = wrap_socket_name(&socket_path).expect("wrap socket name");
        silent.push(Stream::connect(name).expect("connect silent peer"));
    }
    let cap_deadline = Instant::now() + Duration::from_secs(3);
    let mut observed_cap = false;
    while Instant::now() < cap_deadline {
        if let Ok(line) = stderr_rx.recv_timeout(Duration::from_millis(50)) {
            if line.contains("MAX_INFLIGHT_HANDLERS") {
                observed_cap = true;
                break;
            }
        }
    }
    assert!(
        observed_cap,
        "test setup must deterministically exhaust all handler slots"
    );

    std::thread::sleep(Duration::from_millis(
        TEST_HELLO_TIMEOUT_MS.parse::<u64>().unwrap() + 500,
    ));
    let name = wrap_socket_name(&socket_path).expect("wrap socket name");
    let mut stream = Stream::connect(name).expect("connect recovery peer");
    write_test_hello(&mut stream, &program);
    let reply_reader = std::thread::spawn(move || read_test_reply(&mut stream));
    let reply = wait_thread_with_deadline(reply_reader, Duration::from_secs(3));

    drop(silent);
    let _ = child.kill();
    let _ = child.wait();
    stderr_thread.join().expect("stderr reader joins");
    stdout_thread.join().expect("stdout reader joins");
    assert_eq!(reply.connection_id, 0x609);
}

fn unique_program(prefix: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{:010x}", nonce & 0xFF_FFFF_FFFF)
}

fn read_bound_path_bounded(
    stdout: std::process::ChildStdout,
) -> (String, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !sent {
                if let Some(rest) = line.strip_prefix("running-process-broker-v2 bound at ") {
                    let path = rest
                        .trim_end()
                        .rsplit_once(" (")
                        .map(|(path, _)| path)
                        .unwrap_or(rest.trim_end())
                        .to_string();
                    let _ = tx.send(path);
                    sent = true;
                }
            }
        }
    });
    let path = rx
        .recv_timeout(DEADLINE)
        .expect("broker must print bound path within deadline");
    (path, thread)
}

fn write_test_hello(stream: &mut Stream, program: &str) {
    use prost::Message;
    use running_process::broker::protocol::{write_frame, Hello, ENVELOPE_VERSION};
    let hello = Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: program.to_string(),
        wanted_version: "0.0.0".to_string(),
        client_version: "test".to_string(),
        connection_id: 0x609,
        ..Hello::default()
    };
    write_frame(stream, &hello.encode_to_vec()).expect("write Hello");
}

fn read_test_reply(stream: &mut Stream) -> running_process::broker::protocol::Negotiated {
    use prost::Message;
    use running_process::broker::protocol::{hello_reply, read_frame, HelloReply};
    let bytes = read_frame(stream).expect("read HelloReply");
    let reply = HelloReply::decode(bytes.as_slice()).expect("decode HelloReply");
    match reply.result {
        Some(hello_reply::Result::Negotiated(value)) => value,
        other => panic!("expected Negotiated, got {other:?}"),
    }
}

fn wait_thread_with_deadline<T: Send + 'static>(
    thread: std::thread::JoinHandle<T>,
    deadline: Duration,
) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(thread.join());
    });
    rx.recv_timeout(deadline)
        .expect("operation completed within deadline")
        .expect("worker did not panic")
}

struct ChildGuard(std::process::Child);

impl Deref for ChildGuard {
    type Target = std::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wrap_socket_name(socket_path: &str) -> Result<interprocess::local_socket::Name<'_>, String> {
    use interprocess::local_socket::prelude::*;
    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        let bare = socket_path
            .strip_prefix(r"\\.\pipe\")
            .unwrap_or(socket_path);
        bare.to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("to_ns_name: {e}"))
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("to_fs_name: {e}"))
    }
}

fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Result<std::process::ExitStatus, String> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(format!("try_wait failed: {err}")),
        }
    }
    let _ = child.kill();
    Err(format!("binary did not exit within {deadline:?}"))
}

#[cfg(coverage)]
fn coverage_profile_files() -> std::collections::HashSet<std::path::PathBuf> {
    let pattern =
        std::env::var_os("LLVM_PROFILE_FILE").expect("cargo-llvm-cov must set LLVM_PROFILE_FILE");
    let pattern = std::path::PathBuf::from(pattern);
    let root = pattern
        .parent()
        .expect("LLVM_PROFILE_FILE has a parent directory");
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .expect("current directory")
            .join(root)
    };
    let mut profiles = std::collections::HashSet::new();
    collect_profraw(&root, &mut profiles);
    profiles
}

#[cfg(coverage)]
fn collect_profraw(
    directory: &std::path::Path,
    profiles: &mut std::collections::HashSet<std::path::PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_profraw(&path, profiles);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "profraw")
        {
            profiles.insert(path);
        }
    }
}

#[cfg(coverage)]
fn assert_new_coverage_profiles_are_valid(
    profiles_before: &std::collections::HashSet<std::path::PathBuf>,
) {
    let profiles_after = coverage_profile_files();
    let new_profiles: Vec<_> = profiles_after.difference(profiles_before).collect();
    assert!(
        !new_profiles.is_empty(),
        "graceful broker exit must write at least one new .profraw"
    );

    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("run rustc to locate llvm-profdata");
    assert!(sysroot.status.success(), "rustc --print sysroot failed");
    let sysroot = std::path::PathBuf::from(
        String::from_utf8(sysroot.stdout)
            .expect("UTF-8 sysroot")
            .trim(),
    );
    let llvm_profdata = std::fs::read_dir(sysroot.join("lib/rustlib"))
        .expect("read rustlib targets")
        .flatten()
        .map(|entry| entry.path().join("bin/llvm-profdata"))
        .find(|candidate| candidate.is_file())
        .expect("llvm-tools-preview llvm-profdata");

    for profile in new_profiles {
        let output = Command::new(&llvm_profdata)
            .arg("show")
            .arg(profile)
            .output()
            .expect("run llvm-profdata show");
        assert!(
            output.status.success(),
            "broker profile rejected by llvm-profdata: {}\nstdout:\n{}\nstderr:\n{}",
            profile.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

// `read_to_string` is on `Read`, not `BufRead`; import it explicitly so
// the drain at the end of the bind test compiles.
use std::io::Read as _;
