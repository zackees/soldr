#![cfg(feature = "daemon")]
//! First #131 telemetry slice: daemon-owned sessions can tee stream bytes into
//! non-blocking in-memory rings while no client is attached.

use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use running_process::daemon::pipe_sessions::{PipeSessionRegistry, PipeStreamSelect};
#[cfg(not(windows))]
use running_process::daemon::pty_sessions::PtySessionRegistry;
use running_process::daemon::telemetry::{
    TeeBackpressure, TeeEvent, TeeFileMode, TeeFileOptions, TeeRawOptions, TeeStream,
};

fn testbin_path(name: &str) -> PathBuf {
    // Fixtures are built once, before the suite runs (see `ci/test.py`).
    //
    // This used to invoke `cargo build -p testbins` on every call. That takes
    // cargo's build-directory lock, and nextest runs each test in its own
    // process, so a full-suite run had dozens of cargo invocations contending
    // for one lock. `Command::output` waits for EOF with no deadline, and
    // cargo's "Blocking waiting for file lock" note went to inherited stderr
    // the harness only shows on failure — so it presented as an unexplained
    // 30s+ hang. See running-process#747 for the symbolized stack.
    let exe = std::env::current_exe().expect("current exe");
    // .../target/<triple>/<profile>/deps/<test-binary>
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

fn wait_until_contains<F>(mut read: F, needle: &[u8]) -> Vec<u8>
where
    F: FnMut() -> Vec<u8>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let bytes = read();
        if bytes.windows(needle.len()).any(|window| window == needle) {
            return bytes;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {:?} in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&bytes)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit<F>(mut exited: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !exited() {
        if Instant::now() >= deadline {
            panic!("session did not exit within deadline");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_event_contains(receiver: &Receiver<TeeEvent>, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(TeeEvent::Bytes(bytes)) => {
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    return bytes;
                }
            }
            Ok(TeeEvent::MissedBytes(_)) => {}
            Err(_) if Instant::now() < deadline => {}
            Err(err) => panic!("timed out waiting for tee event: {err}"),
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for tee event containing {:?}",
                String::from_utf8_lossy(needle)
            );
        }
    }
}

fn wait_for_file_contains(path: &PathBuf, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let bytes = fs::read(path).unwrap_or_default();
        if bytes.windows(needle.len()).any(|window| window == needle) {
            return bytes;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for file {:?} to contain {:?}; got {:?}",
                path,
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&bytes)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn pipe_stdout_tee_ring_accumulates_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            "tee-ring-test".to_string(),
            "testbin-emitter".to_string(),
            false,
        )
        .expect("spawn pipe session");

    let handle = session
        .tee_stream_ring(PipeStreamSelect::Stdout, 128)
        .expect("register stdout tee");

    let bytes = wait_until_contains(
        || session.tee_snapshot(handle).expect("snapshot").bytes,
        b"tick",
    );
    let snapshot = session.tee_snapshot(handle).expect("snapshot");
    assert_eq!(snapshot.stream, TeeStream::Stdout);
    assert_eq!(snapshot.bytes, bytes);
    assert_eq!(snapshot.capacity, 128);

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
}

#[test]
fn pipe_stdout_tee_channel_receives_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            "tee-channel-test".to_string(),
            "testbin-emitter".to_string(),
            false,
        )
        .expect("spawn pipe session");

    let (handle, receiver) = session
        .tee_stream_channel(PipeStreamSelect::Stdout, 8)
        .expect("register stdout channel tee");

    let bytes = wait_for_event_contains(&receiver, b"tick");
    assert!(bytes.windows(b"tick".len()).any(|window| window == b"tick"));
    assert!(session.tee_snapshot(handle).is_none());
    let status = session.tee_status(handle).expect("status");
    assert_eq!(status.stream, TeeStream::Stdout);
    assert!(!status.disconnected);
    assert!(session.untee(handle));

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
}

#[test]
fn pipe_stdout_tee_file_receives_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stdout.log");
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            "tee-file-test".to_string(),
            "testbin-emitter".to_string(),
            false,
        )
        .expect("spawn pipe session");

    let handle = session
        .tee_stream_file(
            PipeStreamSelect::Stdout,
            &path,
            TeeFileOptions {
                mode: TeeFileMode::Truncate,
                queue_capacity: 8,
                write_missed_markers: true,
                backpressure: TeeBackpressure::DropOldest,
            },
        )
        .expect("register stdout file tee");

    let bytes = wait_for_file_contains(&path, b"tick");
    assert!(bytes.windows(b"tick".len()).any(|window| window == b"tick"));
    let status = session.tee_status(handle).expect("status");
    assert_eq!(status.stream, TeeStream::Stdout);
    assert!(!status.disconnected);
    assert!(session.untee(handle));

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
}

#[test]
fn pipe_stdout_tee_raw_os_sink_receives_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stdout-raw.log");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .expect("open raw tee file");
    let registry = Arc::new(PipeSessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            "tee-raw-test".to_string(),
            "testbin-emitter".to_string(),
            false,
        )
        .expect("spawn pipe session");

    #[cfg(unix)]
    let handle = session
        .tee_stream_raw_fd(
            PipeStreamSelect::Stdout,
            file.as_raw_fd(),
            TeeRawOptions {
                queue_capacity: 8,
                write_missed_markers: true,
                backpressure: TeeBackpressure::DropOldest,
            },
        )
        .expect("register stdout raw fd tee");

    #[cfg(windows)]
    let handle = session
        .tee_stream_raw_handle(
            PipeStreamSelect::Stdout,
            file.as_raw_handle(),
            TeeRawOptions {
                queue_capacity: 8,
                write_missed_markers: true,
                backpressure: TeeBackpressure::DropOldest,
            },
        )
        .expect("register stdout raw handle tee");

    let bytes = wait_for_file_contains(&path, b"tick");
    assert!(bytes.windows(b"tick".len()).any(|window| window == b"tick"));
    let status = session.tee_status(handle).expect("status");
    assert_eq!(status.stream, TeeStream::Stdout);
    assert!(!status.disconnected);
    assert!(session.untee(handle));

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
    drop(file);
}

#[test]
#[cfg(not(windows))]
fn pty_output_tee_ring_accumulates_without_attachment() {
    let emitter = testbin_path("testbin-emitter");
    let registry = Arc::new(PtySessionRegistry::new());
    let session = registry
        .spawn(
            vec![emitter.to_string_lossy().into_owned()],
            None,
            None,
            24,
            80,
            "tee-ring-test".to_string(),
            "testbin-emitter".to_string(),
        )
        .expect("spawn pty session");

    let handle = session.tee_output_ring(128);

    let bytes = wait_until_contains(
        || session.tee_snapshot(handle).expect("snapshot").bytes,
        b"tick",
    );
    let snapshot = session.tee_snapshot(handle).expect("snapshot");
    assert_eq!(snapshot.stream, TeeStream::PtyOutput);
    assert_eq!(snapshot.bytes, bytes);
    assert_eq!(snapshot.capacity, 128);

    let _ = session.terminate(Duration::from_millis(100));
    wait_for_exit(|| session.exit_state().is_some());
    registry.remove(&session.id);
}
