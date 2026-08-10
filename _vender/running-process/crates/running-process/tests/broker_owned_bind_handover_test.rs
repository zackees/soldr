//! The broker-owned bind handover, end to end (#500 slice 32).
//!
//! The unit tests in `broker_owned_bind` cover the broker's half: it binds,
//! clears `FD_CLOEXEC`, publishes the descriptor, and refuses to adopt a
//! descriptor that is not a listening socket. None of that proves the claim a
//! client actually depends on — that a *separate process*, after `exec`,
//! answers on the endpoint the broker bound.
//!
//! This test spans the process boundary, so it can. The fixture
//! (`testbin-inherited-listener-daemon`) adopts the passed descriptor and
//! writes a marker; a marker arriving proves the bytes came from the child
//! rather than from a broker that happened to still hold the socket.
//!
//! Unix only, because the handover is. `support()` reports the Windows gap
//! with a reason and the spawn-then-probe path applies there instead.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::Command;

use running_process::broker::broker_owned_bind::{support, InheritableListener, Support};

/// Locate a fixture binary built before the suite runs.
///
/// Deliberately does not build on demand: doing that takes cargo's
/// build-directory lock once per test process and presented as an
/// unexplained multi-second hang (#747).
fn testbin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
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

/// A socket path short enough for `sun_path`, which silently truncates.
fn socket_path(dir: &tempfile::TempDir) -> String {
    dir.path().join("s").display().to_string()
}

#[test]
fn a_spawned_daemon_serves_the_listener_the_broker_bound() {
    assert!(
        support().is_supported(),
        "this test is cfg'd to Unix, where the handover is supported"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = socket_path(&dir);

    let mut listener = InheritableListener::bind(&path).expect("broker binds the endpoint");
    let mut command = Command::new(testbin_path("testbin-inherited-listener-daemon"));
    listener.prepare(&mut command).expect("prepare");

    let mut child = command.spawn().expect("spawn the fixture daemon");
    // The child owns the endpoint now, so the broker must stop reclaiming the
    // socket file. Without this, dropping `listener` below unlinks a socket
    // the child is still serving.
    listener.disown_endpoint();
    drop(listener);

    // Connect *after* the broker has released its listener entirely — the
    // point being that the endpoint belongs to the child at this stage.
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::{GenericFilePath, Stream, ToFsName as _};
    let name = path
        .as_str()
        .to_fs_name::<GenericFilePath>()
        .expect("socket name");
    let stream = Stream::connect(name).expect("the child must be serving the inherited endpoint");

    let mut served = String::new();
    BufReader::new(stream)
        .read_line(&mut served)
        .expect("read the marker");
    assert_eq!(
        served.trim(),
        "SERVED",
        "the marker must come from the child, not the broker"
    );

    let status = child.wait().expect("wait for the fixture");
    assert!(
        status.success(),
        "fixture exited with {status:?}; 2 = nothing inherited, 3 = inherited but not adoptable"
    );
}

#[test]
fn a_daemon_started_without_a_handover_says_so() {
    // The negative half. Spawning the fixture with no descriptor published
    // must report "nothing was passed" rather than adopting something
    // arbitrary — and it is exit 2 specifically, not a generic failure, so a
    // future regression that breaks adoption cannot masquerade as this.
    let output = Command::new(testbin_path("testbin-inherited-listener-daemon"))
        .output()
        .expect("spawn the fixture daemon");

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected the no-listener exit code; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_windows_gap_is_reported_with_a_reason() {
    // Unreachable here (this file is cfg'd to Unix) but kept as an executable
    // statement of the contract: on a platform without the handover, callers
    // branch on `support()` and fall back, rather than discovering the gap
    // through a failure.
    match support() {
        Support::Supported => {}
        Support::Unsupported { reason } => panic!("unexpected on Unix: {reason}"),
    }
}
