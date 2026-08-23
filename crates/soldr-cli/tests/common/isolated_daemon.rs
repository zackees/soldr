use std::path::{Path, PathBuf};
use std::process::Command;

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
            report_daemon_copy_fallback(source, &executable, &error);
            std::fs::copy(source, &executable).expect("copy isolated test daemon");
        }
    }
    executable
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
/// The Docker harness does not hit this, and the reason is the fix: it sets
/// `TMPDIR=/target/tmp`, putting the test root on the same device as the
/// build output, so the link applies. `_ci-target-run.yml` now does the
/// equivalent with `RUNNER_TEMP`.
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
         per isolated test instead.\n  from: {}\n  to:   {}\n  \
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
