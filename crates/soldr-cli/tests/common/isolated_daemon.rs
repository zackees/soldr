use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Place a test daemon at a route-local executable path and configure the
/// exact production endpoint names derived from that path.
pub(crate) fn isolated_daemon_command(source: &Path, root: &Path) -> Command {
    let executable = isolated_daemon_executable(source, root);
    let mut command = Command::new(&executable);
    super::scrub_outer_soldr_env(&mut command);
    configure_direct_daemon_endpoints(&mut command, &executable);
    command
}

pub(crate) fn configure_isolated_daemon_client(command: &mut Command, source: &Path, root: &Path) {
    let executable = isolated_daemon_executable(source, root);
    super::scrub_outer_soldr_env(command);
    configure_direct_daemon_endpoints(command, &executable);
}

/// A foreground daemon configured for one integration-test cache root.
///
/// Tests that exercise cacheable compiler traffic need a real embedded service;
/// they must not replace it with an externally executed fake zccache binary.
pub(crate) struct IsolatedDaemon {
    child: Option<Child>,
    source: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl IsolatedDaemon {
    pub(crate) fn spawn(source: &Path, root: &Path, home: &Path) -> Self {
        let mut command = isolated_daemon_command(source, root);
        command
            .args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", root)
            .env("HOME", home)
            .env("USERPROFILE", home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("spawn isolated soldr-daemon");
        let daemon = Self {
            child: Some(child),
            source: source.to_path_buf(),
            root: root.to_path_buf(),
            home: home.to_path_buf(),
        };
        daemon.wait_until_ready();
        daemon
    }

    pub(crate) fn configure_client(&self, command: &mut Command) {
        configure_isolated_daemon_client(command, &self.source, &self.root);
        command
            .env("SOLDR_CACHE_DIR", &self.root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home);
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            let mut status = Command::new(super::soldr_bin());
            self.configure_client(&mut status);
            let output = status.args(["daemon", "status", "--json"]).output();
            if output.is_ok_and(|output| output.status.success()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "isolated daemon never became ready under {}",
            self.root.display()
        );
    }
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let mut stop = Command::new(super::soldr_bin());
            self.configure_client(&mut stop);
            let _ = stop.args(["daemon", "stop"]).output();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn isolated_daemon_control_endpoint(source: &Path, root: &Path) -> PathBuf {
    let executable = isolated_daemon_executable(source, root);
    let endpoint = soldr_cli::broker_identity::daemon_session_endpoint_from_executable(&executable)
        .expect("derive test daemon endpoint");
    // The runtime conversion is load-bearing on Windows: the logical value
    // is a bare pipe leaf, and dialing it without the `\\.\pipe\` prefix is
    // a relative-file CreateFile that reports NotFound (-> NotRunning)
    // against a live daemon.
    soldr_cli::daemon::session_endpoint::runtime_control_endpoint_path(PathBuf::from(
        soldr_cli::daemon::session_endpoint::private_control_endpoint_from_session(&endpoint.path),
    ))
}

pub(crate) fn isolated_daemon_executable(source: &Path, root: &Path) -> PathBuf {
    let runtime = root.join("test-daemon-runtime");
    std::fs::create_dir_all(&runtime).expect("create test daemon runtime");
    let executable = runtime.join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "soldr-daemon.exe"
        } else {
            "soldr-daemon"
        },
    );
    if !super::files_equal(source, &executable) {
        let _ = std::fs::remove_file(&executable);
        if let Err(error) = std::fs::hard_link(source, &executable) {
            // Cross-volume. Link from a single shared copy that lives on the
            // *destination's* volume instead of copying per test (soldr#2734).
            let linked = shared_daemon_copy(source, root).is_some_and(|shared| {
                let linked = std::fs::hard_link(&shared, &executable).is_ok();
                if linked {
                    report_shared_copy_in_use(&shared);
                }
                linked
            });
            if !linked {
                report_daemon_copy_fallback(source, &executable, &error);
                std::fs::copy(source, &executable).expect("copy isolated test daemon");
            }
        }
    }
    executable
}

/// One daemon copy per volume, shared by every isolated-daemon test.
///
/// soldr#2734: on the win-gnu target-run lane the workspace is `D:` and the
/// test roots are on `C:`. A hard link cannot cross volumes, so the `copy`
/// fallback above stopped being a fallback -- it ran for *every* isolated-daemon
/// test, and nextest runs them concurrently. Measured on that lane: the
/// workspace volume did not move (143.40 GiB free before and after) while temp
/// went from 31.03 GiB to 13.60 GiB. **17.4 GiB, all of it duplicate copies of
/// one binary**, ending in `Os { code: 112, kind: StorageFull }`.
///
/// Staging one copy on the destination volume makes the link apply again,
/// because both ends are then on that volume. N copies become 1 plus N
/// directory entries.
///
/// ## Why not move the tests' temp root instead
///
/// That was tried and reverted. Pointing `RUNNER_TEMP` at the build volume is
/// the more obvious fix and it broke Windows toolchain resolution on both msvc
/// lanes -- `rustup could not choose a version of cargo to run` -- for reasons
/// that were never explained, including after moving the redirect into a
/// dedicated subdirectory. This change stays inside the test harness and does
/// not touch the OS temp root, so it cannot reproduce that.
///
/// Returns `None` if a shared copy cannot be established, leaving the caller on
/// the per-test copy path. Degrading to today's behaviour is always correct
/// here; failing a test over a caching optimisation would not be.
pub(crate) fn shared_daemon_copy(source: &Path, root: &Path) -> Option<PathBuf> {
    // A sibling of the per-test root, so it is on the same volume as the root
    // (which is what makes the link possible) but outside it (so it outlives
    // the `TempDir` that every test drops).
    let directory = root.parent()?.join("soldr-shared-test-daemon");
    std::fs::create_dir_all(&directory).ok()?;

    // Keyed by the source's identity, so a rebuilt daemon gets a new name
    // rather than racing to replace one that concurrent tests hold links to.
    let metadata = std::fs::metadata(source).ok()?;
    let stamp = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    let shared = directory.join(format!("soldr-daemon-{}-{stamp}", metadata.len()));

    if super::files_equal(source, &shared) {
        return Some(shared);
    }

    // Publish atomically: copy aside, then rename into place. A half-written
    // file under the shared name would be linked by every later test in the
    // shard -- the failure this staging exists to prevent, made permanent.
    let pending = directory.join(format!("pending-{}", std::process::id()));
    std::fs::copy(source, &pending).ok()?;
    if std::fs::rename(&pending, &shared).is_err() {
        // Windows refuses a rename onto an existing file, so the usual cause
        // is another test process publishing the same content first. Its copy
        // is as good as ours.
        let _ = std::fs::remove_file(&pending);
        return super::files_equal(source, &shared).then_some(shared);
    }
    Some(shared)
}

/// Say, once per process, that the cross-volume staging is carrying the run.
///
/// Without this the fix is invisible when it works: the only output on this
/// path was the fallback warning, so a lane could report nothing whether the
/// staging applied or the direct link did, and the two are the difference
/// between one daemon copy and one per test. soldr#2734 was diagnosed from disk
/// deltas across a whole shard precisely because nothing said which path ran.
///
/// One line per process, for the same reason as the fallback below: the
/// condition is a property of the two volumes, so it holds for every test in
/// the process.
fn report_shared_copy_in_use(shared: &Path) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let bytes = std::fs::metadata(shared).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "soldr test: hard link crossed volumes; every isolated test daemon in \
         this process links from one shared {bytes}-byte copy instead of \
         copying per test (soldr#2734).\n  shared: {}",
        shared.display(),
    );
}

/// Say, once per process, that the hard link did not apply.
///
/// soldr#2734: the `hard_link` above is the cheap path and the `copy` is meant
/// to be the exception. A hard link cannot cross volumes, so when the daemon
/// binary and the test root are on different ones the exception becomes the
/// rule -- every isolated-daemon test writes a *full* daemon binary into the
/// test root, and nextest runs them concurrently. On the win-gnu target-run
/// lane that shows up as `Os { code: 112, kind: StorageFull }` from the
/// `.expect` below, with nothing saying where the space went.
///
/// This now fires only when [`shared_daemon_copy`] *also* failed, so reaching
/// it means neither the direct link nor the shared-copy link applied and the
/// per-test copy is genuinely unavoidable. That is the state the issue
/// described, and it should now be rare rather than routine.
///
/// The Docker harness does not hit the cross-volume case at all: it sets
/// `TMPDIR=/target/tmp`, putting the test root on the same device as the build
/// output, so the direct link applies and neither fallback runs.
///
/// Reported once rather than per test: the condition is a property of the two
/// paths, so it holds for every test in the process, and one line per test
/// would bury it. The size is included because attributing the consumption is
/// the open question on that issue.
fn report_daemon_copy_fallback(source: &Path, destination: &Path, error: &std::io::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let bytes = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "soldr test: hard link failed ({error}); copying {bytes} bytes of daemon \
         per isolated test instead (the shared-copy path did not apply either).\n  from: {}\n  to:   {}\n  \
         Different volumes make the copy unconditional -- see soldr#2734.",
        source.display(),
        destination.display(),
    );
}

fn configure_direct_daemon_endpoints(command: &mut Command, executable: &Path) {
    let endpoint = soldr_cli::broker_identity::daemon_session_endpoint_from_executable(executable)
        .expect("derive test daemon endpoint");
    let control =
        soldr_cli::daemon::session_endpoint::private_control_endpoint_from_session(&endpoint.path);
    command
        .env(
            soldr_cli::daemon::session_endpoint::SOLDR_SESSION_ENDPOINT_PATH_ENV,
            &endpoint.path,
        )
        .env(
            soldr_cli::daemon::session_endpoint::SOLDR_CONTROL_ENDPOINT_PATH_ENV,
            control,
        )
        .env(soldr_cli::daemon::client::TEST_DIRECT_CONTROL_ENV, "1")
        .env(
            running_process::broker::server::BACKEND_ENV_ENDPOINT_NAMESPACE,
            &endpoint.namespace_id,
        );
}
