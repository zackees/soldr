//! The election must produce exactly one daemon under concurrency (#631).
//!
//! This is the test the bind-wins design exists for. A check-then-act
//! bring-up passes a sequential test and fails here, because the TOCTOU
//! window only opens when several processes race.

use std::io::Read as _;
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// Ask the OS for a port and immediately release it.
///
/// Inherently racy in the abstract, but it is what keeps concurrent test runs
/// off each other's ports, and the daemons under test are the only things
/// deliberately binding in this range.
fn free_ephemeral_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn role_line(stdout: &str) -> String {
    stdout
        .lines()
        .find(|l| l.starts_with("role="))
        .unwrap_or("<no role line>")
        .to_string()
}

#[test]
fn exactly_one_process_wins_the_election() {
    const RACERS: usize = 8;

    let port = free_ephemeral_port();
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_rpprobed");

    // Launch all racers before reading any output, so they genuinely overlap.
    let kids: Vec<_> = (0..RACERS)
        .map(|_| {
            Command::new(bin)
                .args([
                    "--elect-then-exit",
                    "--beacon-port",
                    &port.to_string(),
                    "--runtime-dir",
                    dir.path().to_str().unwrap(),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn rpprobed")
        })
        .collect();

    let mut roles = Vec::new();
    for mut kid in kids {
        let mut out = String::new();
        if let Some(mut so) = kid.stdout.take() {
            let _ = so.read_to_string(&mut out);
        }
        let status = kid.wait().expect("wait");
        assert!(status.success(), "racer exited {status:?}; stdout: {out:?}");
        roles.push(role_line(&out));
    }

    let daemons = roles
        .iter()
        .filter(|r| r.starts_with("role=daemon"))
        .count();
    let clients = roles
        .iter()
        .filter(|r| r.starts_with("role=client"))
        .count();

    assert_eq!(
        daemons, 1,
        "exactly one racer may win the election; roles were {roles:?}"
    );
    assert_eq!(
        daemons + clients,
        RACERS,
        "every racer must resolve to daemon or client (no strangers, no \
         failures); roles were {roles:?}"
    );
}

/// The winner publishes a discovery file the losers could read.
#[test]
fn election_winner_publishes_discovery_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = env!("CARGO_BIN_EXE_rpprobed");

    // Retry: free_ephemeral_port releases the port before the child binds it,
    // so a concurrent test can claim it first and this run would correctly
    // report role=client. Retrying keeps the assertion about the winner's
    // behavior rather than about winning the port lottery.
    let mut out = None;
    for _ in 0..12 {
        let o = Command::new(bin)
            .args([
                "--elect-then-exit",
                "--beacon-port",
                &free_ephemeral_port().to_string(),
                "--runtime-dir",
                dir.path().to_str().unwrap(),
            ])
            .output()
            .expect("run rpprobed");
        if String::from_utf8_lossy(&o.stdout).contains("role=daemon") {
            out = Some(o);
            break;
        }
    }
    out.expect("never won an election in 12 attempts");

    let published = dir.path().join("rpprobed.json");
    assert!(
        published.exists(),
        "winner must publish {}",
        published.display()
    );

    let body = std::fs::read_to_string(&published).expect("read discovery file");
    for key in [
        "wire_version",
        "control_socket",
        "http_port",
        "bearer_token",
        "daemon_pid",
    ] {
        assert!(body.contains(key), "discovery file missing {key}: {body}");
    }
}
