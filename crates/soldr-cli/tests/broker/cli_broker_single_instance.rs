//! Soldr's CLI must explain an already-bound broker endpoint and exit with
//! the supervisor-retryable status without adding the silent-failure marker.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::common;
use crate::common::tracked_child::TrackedChild;

const LOSER_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// `soldr broker serve` with both pipes draining from spawn (soldr#3197): the
/// broker outlives the test body, so an undrained pipe would stall it.
fn spawn_broker(home: &Path) -> TrackedChild {
    let mut command = common::isolated_soldr_command();
    command
        .args(["broker", "serve"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null());
    common::tracked_child::spawn_tracked(&mut command).expect("spawn soldr broker serve")
}

fn broker_bind_endpoint(home: &Path) -> String {
    if soldr_platform::host::facts::os() == soldr_platform::host::facts::HostOs::Windows {
        let executable =
            soldr_cli::broker_identity::authoritative_broker_executable(home, "soldr-broker.exe");
        let pipe = soldr_cli::broker_identity::windows_broker_pipe_from_executable(
            &executable.display().to_string(),
        )
        .expect("derive broker pipe");
        format!(r"\\.\pipe\{}", pipe.pipe_leaf)
    } else {
        let executable =
            soldr_cli::broker_identity::authoritative_broker_executable(home, "soldr-broker");
        soldr_cli::broker_identity::resolve_unix_for_executable(
            &executable,
            &soldr_platform::ipc::endpoint::machine_runtime_dir(),
            None,
            soldr_platform::ipc::endpoint::sun_path_capacity(),
        )
        .expect("resolve broker endpoint")
        .bind_endpoint
    }
}

#[test]
fn already_bound_endpoint_reports_cli_diagnostic_and_exit_75() {
    let home = common::unique_temp_dir("broker-single-home");
    let endpoint = broker_bind_endpoint(&home);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("broker endpoint runtime");
    let _runtime_guard = runtime.enter();
    let _occupied = soldr_platform::ipc::broker::bind_listener(&endpoint, 1024)
        .expect("occupy broker endpoint");

    let loser = spawn_broker(&home).wait_bounded(LOSER_EXIT_TIMEOUT);
    assert!(
        !loser.timed_out,
        "broker stayed alive for {LOSER_EXIT_TIMEOUT:?} after its endpoint was occupied ({})",
        loser.disposition()
    );
    let output = loser.into_output();

    assert_eq!(
        output.status.code(),
        Some(75),
        "loser broker must exit EX_TEMPFAIL(75), got {:?}",
        output.status
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("another broker is already bound"),
        "second broker exited without explaining that another broker already \
             owned the bind path; output was:\n{combined}"
    );
    // soldr#2024 exit-guard regression check: an explained non-zero exit
    // must not ALSO get the generic "fault in soldr itself" annotation
    // (this is the mark_spoke() bug this test would have caught).
    assert!(
        !combined.contains("fault in soldr itself"),
        "the already-bound refusal is a real explanation and must not trip \
             the silent-failure annotation; output was:\n{combined}"
    );
}
