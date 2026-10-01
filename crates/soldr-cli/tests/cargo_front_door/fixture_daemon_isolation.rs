//! soldr#3516: a broker-launched fixture daemon must stay inside its fixture
//! and must not outlive it.

use crate::common::{self, isolated_soldr_command, unique_temp_dir, BrokerHomeGuard};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Start the broker-owned daemon for a fixture whose client commands run
/// under `home_root` and `cache_root`, and return the daemon image the
/// clients must name.
pub(crate) fn start_fixture_broker_daemon(cache_root: &Path, home_root: &Path) -> PathBuf {
    let daemon_executable = common::isolated_daemon::isolated_daemon_executable(
        &common::soldr_daemon_bin(),
        cache_root,
    );
    let daemon_start = isolated_soldr_command()
        .args(["daemon", "start"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env(
            soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
            &daemon_executable,
        )
        .output()
        .expect("start broker-owned daemon for a fixture");
    assert!(
        daemon_start.status.success(),
        "broker-owned fixture daemon start failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&daemon_start.stdout),
        String::from_utf8_lossy(&daemon_start.stderr)
    );
    daemon_executable
}

/// Read one variable from a live process's environment through procfs.
fn linux_process_env(pid: u32, name: &str) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{name}=");
    environ
        .split(|byte| *byte == 0)
        .map(String::from_utf8_lossy)
        .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_string))
}

/// soldr#3516: a broker-launched daemon started from a fixture must live
/// entirely inside that fixture. The broker used to spawn it under
/// running-process's `UserBaseline`, which rebuilt `HOME` from passwd, so the
/// daemon carried the developer's real `HOME`; and the fixture's guard only
/// stopped the broker, which retains daemon routes, so the daemon outlived the
/// test and wedged the real `~/.soldr` root. Assert both halves: the spawned
/// daemon's `HOME`, root and image are under the fixture, and dropping the
/// fixture's guard leaves no daemon behind.
#[test]
fn fixture_broker_daemon_stays_inside_the_fixture_and_is_reaped() {
    use soldr_platform::process::inspect;

    let cache_root = unique_temp_dir("fixture-daemon-isolation");
    let home_root = cache_root.join("home");
    fs::create_dir_all(&home_root).expect("create fixture home");
    let broker = BrokerHomeGuard::new(&cache_root, &home_root);
    start_fixture_broker_daemon(&cache_root, &home_root);

    let pid = common::route_claim::route_claim_pid(&cache_root)
        .expect("the broker-owned daemon publishes a route claim under the fixture root");
    assert!(
        inspect::is_alive(pid),
        "fixture daemon PID {pid} is not running"
    );
    let exe = inspect::executable_path(pid).expect("fixture daemon executable path");
    assert!(
        common::path_is_under_any(&exe, &[&cache_root, &home_root]),
        "fixture daemon image {} is outside the fixture {}",
        exe.display(),
        cache_root.display()
    );

    // procfs is the only portable-enough way to read another process's
    // environment; the image/root/reap halves above and below run everywhere.
    if soldr_platform::host::facts::os() == soldr_platform::host::facts::HostOs::Linux {
        let home = linux_process_env(pid, "HOME").expect("fixture daemon HOME");
        assert!(
            common::path_is_under_any(Path::new(&home), &[&cache_root]),
            "fixture daemon HOME={home} escaped the fixture {} (soldr#3516)",
            cache_root.display()
        );
        let root = linux_process_env(pid, "SOLDR_CACHE_DIR").expect("fixture daemon root");
        assert!(
            common::path_is_under_any(Path::new(&root), &[&cache_root]),
            "fixture daemon SOLDR_CACHE_DIR={root} escaped the fixture {}",
            cache_root.display()
        );
    }

    drop(broker);
    let deadline = Instant::now() + Duration::from_secs(10);
    while inspect::is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !inspect::is_alive(pid),
        "fixture daemon PID {pid} outlived its BrokerHomeGuard (soldr#3516)"
    );
}
