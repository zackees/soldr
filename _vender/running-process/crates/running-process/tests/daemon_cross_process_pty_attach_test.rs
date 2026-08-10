#![cfg(feature = "daemon")]
//! Cross-process integration test for daemon-owned PTY sessions
//! (#130 milestone 2).
//!
//! Unlike `pty_session_attach_test.rs`, which spawns a `DaemonServer` on a
//! tokio task in the test process, this file launches the
//! `running-process-daemon` binary itself as a separate OS process. The
//! client then communicates over the OS socket — closing the gap that the
//! in-process test does not cover: daemon-binary startup, socket-path
//! handshake, PTY ownership across an OS process boundary, and the
//! invariant that the PTY child outlives its first client even when that
//! client's *OS process* goes away (not just its tokio task).
//!
//! Notes:
//!   * The daemon binary is built via `cargo build -p running-process-daemon`
//!     in test setup; we then locate it via the cargo-emitted JSON.
//!   * Each test uses a unique `--scope` so the socket/db paths do not
//!     collide.
//!   * The `DaemonGuard` struct ensures the spawned daemon is killed when
//!     the test ends, even on assertion failure.

use running_process::client::client::DaemonClient;
use running_process::client::paths;
use running_process::client::pty_session::{PtyAttachment, PtySpawnRequest};

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Build / spawn helpers
// ---------------------------------------------------------------------------

// Fix Wave T2 of #165: post-mono-crate, the daemon binary lives inside
// `running-process` and is selected by `--bin running-process-daemon`
// (under `--features daemon`); the testbin lives inside `testbins`.
// This helper takes the cargo args directly so each caller can pin the
// right package + bin selector + feature set.
fn build_artifact(cargo_args: &[&str], bin_name: &str) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(cargo_args)
        .arg("--message-format=json")
        .stderr(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| panic!("failed to invoke cargo build {cargo_args:?}: {e}"));
    assert!(
        output.status.success(),
        "cargo build {cargo_args:?} exited with {:?}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(bin_name) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-artifact" {
            continue;
        }
        let target_name = v["target"]["name"].as_str().unwrap_or("");
        if target_name != bin_name {
            continue;
        }
        let is_bin = v["target"]["kind"]
            .as_array()
            .is_some_and(|a| a.iter().any(|k| k == "bin"));
        if !is_bin {
            continue;
        }
        if let Some(exe) = v["executable"].as_str() {
            let path = PathBuf::from(exe);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !path.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                path.exists(),
                "cargo emitted {path:?} but it does not exist"
            );
            return path;
        }
    }
    panic!("cargo build {cargo_args:?} produced no bin artifact named {bin_name}");
}

fn daemon_binary() -> PathBuf {
    build_artifact(
        &[
            "build",
            "-p",
            "running-process",
            "--features",
            "daemon",
            "--bin",
            "running-process-daemon",
        ],
        "running-process-daemon",
    )
}

fn sleeper_binary() -> PathBuf {
    build_artifact(
        &["build", "-p", "testbins", "--bin", "testbin-sleeper"],
        "testbin-sleeper",
    )
}

struct DaemonGuard {
    child: Option<Child>,
    socket: String,
}

impl DaemonGuard {
    fn new(scope: String) -> Self {
        let bin = daemon_binary();
        let socket = paths::socket_path(Some(&scope));
        let db_path = paths::db_path(Some(&scope)).to_string_lossy().into_owned();

        // Clean any previous socket/db at this path so a flaky earlier run
        // does not poison this one.
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(&db_path);

        let child = Command::new(&bin)
            .arg("start")
            .arg("--scope")
            .arg(&scope)
            .arg("--socket-path")
            .arg(&socket)
            .arg("--db-path")
            .arg(&db_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn daemon binary");

        // Poll until the socket accepts a connection.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if DaemonClient::connect_to(&socket).is_ok() {
                return Self {
                    child: Some(child),
                    socket,
                };
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut g = Self {
            child: Some(child),
            socket,
        };
        g.shutdown();
        panic!("daemon did not become ready within 10s");
    }

    fn socket(&self) -> &str {
        &self.socket
    }

    /// Best-effort polite shutdown then SIGKILL fallback.
    fn shutdown(&mut self) {
        if let Ok(mut client) = DaemonClient::connect_to(&self.socket) {
            let _ = client.shutdown(true, 2.0);
        }
        if let Some(mut child) = self.child.take() {
            // Brief grace.
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        // Best-effort socket cleanup on Unix.
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn pty_session_survives_first_client_disconnect_then_second_client_attaches() {
    let scope = format!("cross-proc-pty-{}-{}", std::process::id(), line!());
    let mut guard = DaemonGuard::new(scope);
    let socket = guard.socket().to_string();
    let sleeper = sleeper_binary();

    // First client connection: spawn the PTY session, then drop the
    // connection. The session must outlive this drop.
    let session_id = {
        let mut client = DaemonClient::connect_to(&socket).expect("client A connect");
        let req = PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
            .with_originator("cross-proc-test");
        let spawned = client.spawn_pty_session(&req).expect("spawn");
        assert!(spawned.pid > 0);
        spawned.session_id
        // client (and its socket) drops here.
    };

    // Tiny pause so the daemon definitely sees the disconnect.
    std::thread::sleep(Duration::from_millis(100));

    // Second client connection: list should still show the session.
    {
        let mut client = DaemonClient::connect_to(&socket).expect("client B connect");
        let listed = client.list_pty_sessions("").expect("list");
        let entry = listed
            .iter()
            .find(|s| s.session_id == session_id)
            .expect("session must survive first client disconnect");
        assert!(!entry.attached);
        assert!(!entry.exited);
    }

    // Third connection: attach, write input, detach. Session keeps running.
    {
        let mut attachment = PtyAttachment::attach_to(&socket, &session_id, 24, 80, false)
            .expect("attach from third connection");
        attachment.send_input(b"ping\n").expect("send_input");
        attachment.detach().expect("detach");
    }
    std::thread::sleep(Duration::from_millis(100));

    // Fourth connection: terminate and wait for exited state.
    {
        let mut client = DaemonClient::connect_to(&socket).expect("client D connect");
        client
            .terminate_pty_session(&session_id, 1000)
            .expect("terminate");

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let listed = client.list_pty_sessions("").expect("list during wait");
            if let Some(entry) = listed.iter().find(|s| s.session_id == session_id) {
                if entry.exited {
                    break;
                }
            }
            if Instant::now() >= deadline {
                panic!("session did not exit within budget after terminate");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    guard.shutdown();
}

#[test]
fn daemon_shutdown_reaps_sessions_no_orphans() {
    // #130 M8: when the daemon shuts down, every session it owns must
    // be torn down. On Windows the Job Object kill-on-close handles
    // this implicitly, but on POSIX the daemon must explicitly issue
    // kill_tree before exiting or the children become orphans. This
    // test passes on both platforms with the explicit reap path in
    // `server::reap_all_sessions`.
    let scope = format!("cross-proc-reap-{}-{}", std::process::id(), line!());
    let mut guard = DaemonGuard::new(scope);
    let socket = guard.socket().to_string();
    let sleeper = sleeper_binary();

    let child_pid = {
        let mut client = DaemonClient::connect_to(&socket).expect("connect");
        let session = client
            .spawn_pty_session(
                &PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
                    .with_originator("reap-test"),
            )
            .expect("spawn");
        assert!(session.pid > 0);
        session.pid
    };

    // Sanity: the PID is alive before shutdown.
    assert!(pid_is_alive(child_pid));

    // Shut the daemon down via its polite RPC. The guard's drop will
    // verify the process exited; we additionally verify the child PID
    // is gone.
    {
        let mut client = DaemonClient::connect_to(&socket).expect("connect");
        let _ = client.shutdown(true, 5.0);
    }

    // Give the daemon a moment to finish reaping + exit.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut child_gone = false;
    while Instant::now() < deadline {
        if !pid_is_alive(child_pid) {
            child_gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        child_gone,
        "child PID {child_pid} should not be alive after daemon shutdown"
    );

    guard.shutdown();
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::ntdef::NULL;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{GetExitCodeProcess, OpenProcess};
    use winapi::um::winnt::PROCESS_QUERY_INFORMATION;

    const STILL_ACTIVE: DWORD = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        if handle == NULL {
            return false;
        }
        let mut exit_code: DWORD = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code as *mut _);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    use libc::{kill, ESRCH};
    unsafe {
        if kill(pid as i32, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
    }
}

#[test]
fn concurrent_attach_attempts_resolve_to_exactly_one_winner() {
    let scope = format!("cross-proc-race-{}-{}", std::process::id(), line!());
    let mut guard = DaemonGuard::new(scope);
    let socket = guard.socket().to_string();
    let sleeper = sleeper_binary();

    let session_id = {
        let mut client = DaemonClient::connect_to(&socket).expect("connect");
        let req = PtySpawnRequest::new([sleeper.to_string_lossy().into_owned()])
            .with_originator("race-test");
        client.spawn_pty_session(&req).expect("spawn").session_id
    };

    // Fire two attach attempts in parallel from independent OS threads.
    let socket_a = socket.clone();
    let id_a = session_id.clone();
    let handle_a =
        std::thread::spawn(move || PtyAttachment::attach_to(&socket_a, &id_a, 24, 80, false));
    let socket_b = socket.clone();
    let id_b = session_id.clone();
    let handle_b =
        std::thread::spawn(move || PtyAttachment::attach_to(&socket_b, &id_b, 24, 80, false));

    let result_a = handle_a.join().expect("thread A");
    let result_b = handle_b.join().expect("thread B");

    let winners = [result_a.is_ok(), result_b.is_ok()];
    let winner_count = winners.iter().filter(|w| **w).count();
    assert_eq!(
        winner_count, 1,
        "exactly one attach should win, got winners={winners:?}"
    );

    // Cleanup.
    let mut client = DaemonClient::connect_to(&socket).expect("cleanup connect");
    client
        .terminate_pty_session(&session_id, 500)
        .expect("terminate");

    guard.shutdown();
}
