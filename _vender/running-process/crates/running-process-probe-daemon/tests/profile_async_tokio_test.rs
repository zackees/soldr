//! A live `console-api` subscription against a real tokio app (#788).
//!
//! The unit tests in `src/profile/async_tokio/tests.rs` drive the joining and
//! idle derivation from synthesized messages. This drives the actual gRPC
//! subscription against `testbins-tokio`, which is the part that cannot be
//! faked: whether we can connect, whether updates arrive over a window, and
//! whether a deliberately blocked task ends up dominating an idle-weighted
//! profile.
//!
//! # Why the fixture lives in its own workspace
//!
//! `console-subscriber` requires the profiled application to be built with
//! `--cfg tokio_unstable`, and Cargo has no per-crate `RUSTFLAGS`. Putting it
//! in the root workspace would apply the cfg to the published crate as well.
//! So `testbins-tokio` is excluded from the workspace and built separately:
//!
//! ```text
//! RUSTFLAGS="--cfg tokio_unstable" cargo build --manifest-path testbins-tokio/Cargo.toml
//! ```
//!
//! When it has not been built, these tests skip with that command in the
//! message rather than failing.

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use running_process_probe_daemon::profile::async_tokio;

/// How long to subscribe. Long enough for several update messages, short
/// enough not to dominate the suite.
const WINDOW: Duration = Duration::from_secs(3);

/// Locate the separately-built tokio fixture.
///
/// It does not land in the workspace target directory, so the usual
/// `current_exe()`-relative lookup does not apply. The manifest directory is
/// the stable anchor, and both the plain and `--target`-qualified layouts are
/// checked because CI cross-compiles.
fn tokio_fixture() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("testbins-tokio")
        .join("target");
    let leaf = format!("testbin-tokio-blocked{}", std::env::consts::EXE_SUFFIX);

    let direct = root.join("debug").join(&leaf);
    if direct.is_file() {
        return Some(direct);
    }
    // `target/<triple>/debug/<bin>`
    let entries = std::fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("debug").join(&leaf);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A fixture process that is killed when the guard drops.
struct Fixture(Child);

impl Drop for Fixture {
    fn drop(&mut self) {
        // Killed rather than waited on: it is built to outlive the sampling
        // window, so leaving it running would leak a process per test.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the fixture on `port` and wait for it to announce its endpoint.
///
/// The port is per-test. nextest runs each test in its own process and in
/// parallel, so a shared port had the three fixtures fighting over one bind
/// and failing intermittently — which showed up as a flaky pass on retry.
///
/// `None` means the fixture is not built, which the callers report as a skip.
/// Waiting on its `READY` line rather than sleeping: the gRPC server binds
/// during `console_subscriber::init()` but is not accepting immediately, and a
/// fixed sleep would be either flaky or slow.
fn start_fixture(port: u16) -> Option<(Fixture, String)> {
    let Some(path) = tokio_fixture() else {
        eprintln!(
            "skipping: testbin-tokio-blocked is not built. Build it with:\n  \
             RUSTFLAGS=\"--cfg tokio_unstable\" cargo build --manifest-path \
             testbins-tokio/Cargo.toml"
        );
        return None;
    };

    let mut child = Command::new(&path)
        .arg(port.to_string())
        .arg("60")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tokio fixture");

    let stdout = child.stdout.take().expect("fixture stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    // One line is all it prints before going quiet, so a single blocking read
    // is enough and cannot hang past the process's own lifetime.
    if reader.read_line(&mut line).is_err() || !line.starts_with("READY ") {
        eprintln!("skipping: fixture did not announce READY (got {line:?})");
        // Reaped as well as killed: without the wait this path leaves a
        // zombie for every skipped run.
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let endpoint = line.trim_start_matches("READY ").trim().to_string();
    Some((Fixture(child), endpoint))
}

#[test]
fn a_live_subscription_returns_task_samples() {
    let Some((_fixture, endpoint)) = start_fixture(16669) else {
        return;
    };

    let samples = match async_tokio::collect(&endpoint, WINDOW) {
        Ok(samples) => samples,
        Err(e) => panic!("subscription failed against a running subscriber: {e}"),
    };

    assert!(
        !samples.is_empty(),
        "a subscription to an instrumented runtime returned nothing"
    );
    // Every sample should carry a spawn site; a profile of anonymous tasks
    // cannot be acted on.
    assert!(
        samples.iter().all(|s| !s.name.is_empty()),
        "some samples have no spawn site: {samples:?}"
    );
}

#[test]
fn the_runtimes_own_workers_do_not_swamp_the_applications_tasks() {
    let Some((_fixture, endpoint)) = start_fixture(16670) else {
        return;
    };
    let samples = async_tokio::collect(&endpoint, WINDOW).expect("subscription");

    // A multi-thread runtime parks one blocking-pool worker per core. Measured
    // before filtering: 16 of 19 tasks were tokio's own scheduler, all with
    // identical idle time and all ranked above the fixture's blocked task.
    assert!(
        samples
            .iter()
            .all(|s| !s.name.replace('\\', "/").contains("/tokio-")),
        "runtime-internal tasks reached the profile: {:?}",
        samples.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        samples.len() < 8,
        "expected only the fixture's own handful of tasks, got {}: {:?}",
        samples.len(),
        samples.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn the_blocked_task_carries_the_idle_time_and_the_busy_one_carries_the_polls() {
    let Some((_fixture, endpoint)) = start_fixture(16671) else {
        return;
    };
    let samples = async_tokio::collect(&endpoint, WINDOW).expect("subscription");

    let idlest = samples
        .iter()
        .max_by_key(|s| s.idle_nanos)
        .expect("at least one sample");
    let busiest = samples
        .iter()
        .max_by_key(|s| s.busy_nanos)
        .expect("at least one sample");

    // The fixture parks one task on an hour-long sleep and spins another on a
    // 5ms loop, so the two questions an off-CPU profile answers — what is
    // waiting, what is working — should have different answers here.
    assert!(
        idlest.idle_nanos > 0,
        "nothing accrued idle time: {samples:?}"
    );
    assert!(
        busiest.polls > idlest.polls,
        "the busy task should be polled more than the blocked one: \
         busy={} polls={} vs idle={} polls={}",
        busiest.name,
        busiest.polls,
        idlest.name,
        idlest.polls
    );
    // Both tasks come from the fixture's own source, not from a dependency.
    assert!(
        samples
            .iter()
            .any(|s| s.name.replace('\\', "/").contains("blocked.rs")),
        "no sample points at the fixture's own source: {:?}",
        samples.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}
