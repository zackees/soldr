use std::path::{Path, PathBuf};
use std::process::Command;

/// Place a test daemon at a route-local executable path and configure the
/// exact production endpoint names derived from that path.
pub(crate) fn isolated_daemon_command(source: &Path, root: &Path) -> Command {
    let executable = isolated_daemon_executable(source, root);
    let mut command = Command::new(&executable);
    configure_direct_daemon_endpoints(&mut command, &executable);
    command
}

pub(crate) fn configure_isolated_daemon_client(command: &mut Command, source: &Path, root: &Path) {
    let executable = isolated_daemon_executable(source, root);
    configure_direct_daemon_endpoints(command, &executable);
}

pub(crate) fn isolated_daemon_control_endpoint(source: &Path, root: &Path) -> PathBuf {
    let executable = isolated_daemon_executable(source, root);
    let endpoint = soldr_cli::broker_identity::daemon_session_endpoint_from_executable(&executable)
        .expect("derive test daemon endpoint");
    PathBuf::from(
        soldr_cli::daemon::session_endpoint::private_control_endpoint_from_session(&endpoint.path),
    )
}

fn isolated_daemon_executable(source: &Path, root: &Path) -> PathBuf {
    let runtime = root.join("test-daemon-runtime");
    std::fs::create_dir_all(&runtime).expect("create test daemon runtime");
    let executable = runtime.join(if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    });
    if !super::files_equal(source, &executable) {
        let _ = std::fs::remove_file(&executable);
        if std::fs::hard_link(source, &executable).is_err() {
            std::fs::copy(source, &executable).expect("copy isolated test daemon");
        }
    }
    executable
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
