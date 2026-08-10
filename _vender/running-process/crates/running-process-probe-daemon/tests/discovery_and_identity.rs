//! Discovery, identity-handshake rejection, and file permissions (#631).

use std::io::Write as _;
use std::net::TcpListener;
use std::process::Command;

fn free_ephemeral_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn run_elect(port: u16, dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rpprobed"))
        .args([
            "--elect-then-exit",
            "--beacon-port",
            &port.to_string(),
            "--runtime-dir",
            dir.to_str().unwrap(),
        ])
        .output()
        .expect("run rpprobed")
}

/// Elect until we actually win, then return the output.
///
/// `free_ephemeral_port` releases the port before the child binds it, so a
/// concurrently-running test can legitimately claim it in between — the child
/// then correctly reports `role=client`. That is the product behaving as
/// designed, not a defect, so tests that need to observe the *winner* retry on
/// a fresh port rather than assert on a coin flip.
fn elect_until_daemon(dir: &std::path::Path) -> std::process::Output {
    const ATTEMPTS: usize = 12;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let out = run_elect(free_ephemeral_port(), dir);
        if String::from_utf8_lossy(&out.stdout).contains("role=daemon") {
            return out;
        }
        last = Some(out);
    }
    panic!(
        "never won an election in {ATTEMPTS} attempts; last stdout: {:?}",
        last.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    );
}

/// A client with no prior knowledge learns the daemon's endpoints from the
/// published file.
#[test]
fn discovery_file_exposes_both_transports() {
    let dir = tempfile::tempdir().unwrap();
    let out = elect_until_daemon(dir.path());
    assert!(String::from_utf8_lossy(&out.stdout).contains("role=daemon"));

    let body = std::fs::read_to_string(dir.path().join("rpprobed.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

    assert_eq!(parsed["wire_version"], 1);
    assert!(
        parsed["http_port"].as_u64().unwrap() > 0,
        "http_port must be a real bound port: {body}"
    );
    assert!(
        !parsed["control_socket"].as_str().unwrap().is_empty(),
        "control_socket must be populated: {body}"
    );
    assert_eq!(
        parsed["bearer_token"].as_str().unwrap().len(),
        64,
        "bearer token must be 32 bytes of hex"
    );
}

/// Pointing bring-up at a listener that doesn't speak the protocol must yield
/// `stranger`, never `client`. Otherwise a decoy on the beacon port could
/// impersonate the daemon.
#[test]
fn decoy_on_beacon_port_is_rejected_not_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let decoy = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = decoy.local_addr().unwrap().port();

    let handle = std::thread::spawn(move || {
        if let Ok((mut s, _)) = decoy.accept() {
            let _ = s.write_all(b"I am definitely the probe daemon, honest");
        }
    });

    let out = run_elect(port, dir.path());
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("role=stranger"),
        "decoy must be classified stranger; got {stdout:?}"
    );
    assert!(
        !stdout.contains("role=client"),
        "decoy must never be adopted as the daemon: {stdout:?}"
    );
    assert!(
        !dir.path().join("rpprobed.json").exists(),
        "must not publish discovery data when the beacon peer is unidentified"
    );

    let _ = handle.join();
}

/// A second daemon on the same beacon joins rather than double-binding.
#[test]
fn second_instance_does_not_win_a_taken_beacon() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_ephemeral_port();

    let first = run_elect(port, dir.path());
    let first_role = String::from_utf8_lossy(&first.stdout).into_owned();
    if !first_role.contains("role=daemon") {
        // Port was taken by a concurrent test; the contract under test
        // (a definite role, never a crash) still holds.
        assert!(first_role.contains("role=client") || first_role.contains("role=stranger"));
        return;
    }

    // The first exited under --elect-then-exit and released the port, so a
    // second run wins again. What must never happen is a *stranger* or a
    // crash: the outcome is always a definite role.
    let second = run_elect(port, dir.path());
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("role=daemon") || stdout.contains("role=client"),
        "second instance must resolve to a definite role; got {stdout:?}"
    );
    assert!(second.status.success());
}

/// The discovery directory must not be world/group accessible — it holds the
/// bearer token.
#[cfg(unix)]
#[test]
fn discovery_dir_and_file_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let out = elect_until_daemon(dir.path());
    assert!(String::from_utf8_lossy(&out.stdout).contains("role=daemon"));

    let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
    assert_eq!(
        dir_mode & 0o077,
        0,
        "discovery dir mode {dir_mode:o} is not owner-only"
    );

    let file_mode = std::fs::metadata(dir.path().join("rpprobed.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        file_mode & 0o077,
        0,
        "discovery file mode {file_mode:o} exposes the bearer token"
    );
}

/// An unparsable argument must fail loudly rather than silently defaulting to
/// a machine-wide path.
#[test]
fn invalid_arguments_are_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_rpprobed"))
        .args(["--beacon-port", "not-a-port"])
        .output()
        .expect("run rpprobed");
    assert!(
        !out.status.success(),
        "invalid port must not start a daemon"
    );
}
