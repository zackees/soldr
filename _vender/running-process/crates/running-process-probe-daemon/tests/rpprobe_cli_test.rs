//! `rpprobe` against a real `rpprobed` (S14 / #643).
//!
//! Spawns the actual daemon on a private runtime directory and drives the CLI
//! in-process against it. In-process rather than by spawning the `rpprobe`
//! binary: the assertions are about *behaviour* (which processes a selection
//! targets, what a refusal says, what `doctor` concludes), and reading those
//! back out of a subprocess's stdout would test the formatter more than the
//! logic.
//!
//! The daemon is a real process, though — the transport, the peer-credential
//! check, and the discovery file are the parts most likely to be wrong, and
//! none of them exist in a mock.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use running_process_probe_daemon::cli::transport::{load_discovery, CliError};
use running_process_probe_daemon::cli::{commands, Cli, Command as Cmd};
use tempfile::TempDir;

/// A running `rpprobed` on its own runtime directory.
struct Daemon {
    child: Child,
    dir: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// Start the daemon and wait until it has published its discovery file.
    ///
    /// Waits for the file rather than sleeping: a fixed sleep is either a slow
    /// test or a flaky one, and on a loaded CI runner it is both.
    fn start() -> Option<Self> {
        let binary = daemon_binary()?;
        let dir = TempDir::new().expect("temp dir");

        // `--beacon-port 0` lets the OS pick, so this instance always wins its
        // own election. The default beacon port is per-user and machine-wide —
        // correct for the real daemon (one per user is the whole point) but
        // fatal for tests, where every test process would see whichever
        // daemon started first, resolve to `role=client`, and never publish a
        // discovery file of its own.
        let mut child = Command::new(binary)
            .arg("--runtime-dir")
            .arg(dir.path())
            .arg("--beacon-port")
            .arg("0")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rpprobed");

        // The daemon prints `role=daemon …` once it owns the endpoint.
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);

        // Ready means "answers a request", not "has written a file". Polling
        // the file alone would pass the moment the daemon published itself and
        // then race its accept loop — the difference between a test that is
        // slow and a test that is flaky.
        let daemon = Self { child, dir };
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last: Option<CliError> = None;
        while Instant::now() < deadline {
            match run(&daemon, ps(Some(1))) {
                Ok(_) => return Some(daemon),
                Err(error) => last = Some(error),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("rpprobed never became ready; first stdout line was {line:?}, last error {last:?}");
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }
}

/// Locate the built `rpprobed`.
///
/// Returns `None` rather than failing when it is absent: a bare `cargo test`
/// on a crate whose binaries have not been built yet should skip this, not
/// report a false failure about the daemon.
fn daemon_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // …/target/<triple>/debug/deps/<test>-<hash>.exe → …/debug/
    let dir = exe.parent()?.parent()?;
    let name = format!("rpprobed{}", std::env::consts::EXE_SUFFIX);
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

/// Build a CLI invocation against `daemon`.
fn cli(daemon: &Daemon, command: Cmd) -> Cli {
    Cli {
        discovery: Some(daemon.dir().to_path_buf()),
        json: true,
        http: false,
        command,
    }
}

fn run(daemon: &Daemon, command: Cmd) -> Result<String, CliError> {
    commands::dispatch(&cli(daemon, command))
}

fn ps(limit: Option<u32>) -> Cmd {
    Cmd::Ps {
        name: None,
        include_unregistered: false,
        env: false,
        limit,
    }
}

// --- against a running daemon --------------------------------------------

#[test]
fn ps_reaches_a_running_daemon_over_the_control_socket() {
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let output = run(&daemon, ps(Some(10))).expect("ps should reach the daemon");
    // No process has registered with this fresh daemon, so the correct answer
    // is an empty list — and getting one proves the whole round trip worked,
    // which an error would not.
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_eq!(parsed.as_array().map(Vec::len), Some(0));
}

#[test]
fn crashes_reaches_the_store_through_the_daemon() {
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let output = run(
        &daemon,
        Cmd::Crashes {
            class: None,
            class_like: None,
            signature: None,
            stats: false,
            limit: Some(10),
        },
    )
    .expect("crashes should reach the daemon");
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert!(parsed.is_array());
}

#[test]
fn crash_stats_report_zero_rather_than_failing_on_an_empty_store() {
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let output = run(
        &daemon,
        Cmd::Crashes {
            class: None,
            class_like: None,
            signature: None,
            stats: true,
            limit: None,
        },
    )
    .expect("crash stats should reach the daemon");
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("json");
    assert_eq!(parsed["total"].as_u64(), Some(0));
}

#[test]
fn dumping_an_unregistered_pid_says_it_is_not_registered() {
    // The probe model is enrollment, not discovery. A pid the daemon has never
    // heard of is not an internal error and must not read like one — the
    // operator needs to be told the app has to register.
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let error = run(
        &daemon,
        Cmd::Dump {
            pid: Some(std::process::id()),
            name: None,
            instance: None,
            all: false,
            force: false,
            max_depth: 32,
        },
    )
    .expect_err("an unregistered pid must not be capturable");
    assert!(
        error.to_string().contains("not registered"),
        "unhelpful error: {error}"
    );
}

#[test]
fn dumping_a_name_that_matches_nothing_names_the_pattern() {
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let error = run(
        &daemon,
        Cmd::Dump {
            pid: None,
            name: Some("*nothing-matches-this*".into()),
            instance: None,
            all: false,
            force: false,
            max_depth: 32,
        },
    )
    .expect_err("no match must be an error, not an empty success");
    assert!(error.to_string().contains("*nothing-matches-this*"));
}

#[test]
fn doctor_reports_every_check_and_fails_when_one_does() {
    // A fresh daemon has no registrations, so `doctor` must fail — that is the
    // whole point of the command, and a green report on a daemon that can
    // capture nothing would be worse than no command at all.
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let error = run(&daemon, Cmd::Doctor).expect_err("no registrations means unhealthy");
    assert!(error.to_string().contains("checks failed"));
}

#[test]
fn the_http_surface_is_reachable_with_the_discovered_token() {
    // Proves the CLI's HTTP fallback and the daemon's token agree — which is
    // what `rpprobe fetch` depends on for artifacts too large for the socket.
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let (_, info) = load_discovery(Some(daemon.dir())).expect("discovery");
    let body = running_process_probe_daemon::cli::transport::http_get(&info, "/v1/ps?limit=1")
        .expect("http surface should answer with the discovered token");
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
}

#[test]
fn a_bad_token_is_refused_by_the_http_surface() {
    let Some(daemon) = Daemon::start() else {
        eprintln!("skipping: rpprobed binary not built");
        return;
    };
    let (_, mut info) = load_discovery(Some(daemon.dir())).expect("discovery");
    info.bearer_token = "0".repeat(64);
    let error = running_process_probe_daemon::cli::transport::http_get(&info, "/v1/ps?limit=1")
        .expect_err("a wrong token must not be served");
    assert!(error.to_string().contains("401"), "unexpected: {error}");
}

// --- without a daemon -----------------------------------------------------

#[test]
fn every_command_fails_cleanly_when_no_daemon_is_running() {
    // Not a panic and not a hang. `rpprobe` is what an operator reaches for
    // when something is already wrong, so its own failure mode has to be a
    // sentence.
    let dir = TempDir::new().expect("temp dir");
    for command in [
        ps(Some(10)),
        Cmd::Doctor,
        Cmd::Crashes {
            class: None,
            class_like: None,
            signature: None,
            stats: false,
            limit: None,
        },
    ] {
        let cli = Cli {
            discovery: Some(dir.path().to_path_buf()),
            json: false,
            http: false,
            command,
        };
        let error = commands::dispatch(&cli).expect_err("must fail without a daemon");
        assert!(matches!(error, CliError::NoDaemon { .. }));
        assert!(error.to_string().contains("rpprobed"));
    }
}
