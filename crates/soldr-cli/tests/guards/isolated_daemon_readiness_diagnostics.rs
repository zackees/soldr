//! RED/GREEN regression for soldr#3380: `IsolatedDaemon` must not swallow the
//! daemon's own stdout/stderr, and must fail fast — not only after the full
//! 90s deadline — when the daemon process has already exited.
//!
//! Drives `IsolatedDaemon::spawn_with_command` with a fake, fast-failing
//! "daemon" (a tiny shell script that writes a marker to stderr and exits 1)
//! instead of a real `soldr-daemon` binary, so the readiness/failure-report
//! path is exercised in milliseconds rather than minutes.
//!
//! Before the soldr#3380 fix this test failed two ways: the panic message
//! never contained the marker (`.stderr(Stdio::null())` discarded it), and
//! reaching that panic took the full 90s deadline instead of firing as soon
//! as the dead child was noticed.

use std::panic::AssertUnwindSafe;
use std::process::Command;

use crate::common;
use crate::common::isolated_daemon::IsolatedDaemon;

const MARKER: &str = "MARKER-3380-daemon-startup-failure";

/// A "daemon" that writes [`MARKER`] to stderr and exits non-zero.
///
/// Deliberately a plain `sh` script rather than anything gated by
/// `#[cfg(unix)]`: that attribute is banned outside `crates/soldr-platform`
/// by `.github/scripts/platform_cfg_boundary_ratchet.py`. The test below
/// skips at runtime on Windows instead (matching the pattern already used
/// by `save_roundtrip/symlinks.rs`), so this content is only ever executed
/// on a host where a shebang script is directly runnable.
fn marker_daemon_script() -> String {
    format!("#!/bin/sh\necho \"{MARKER}\" >&2\nexit 1\n")
}

#[test]
fn readiness_panic_carries_the_dead_daemons_stderr() {
    // Hard-linking a fake executable into the isolated-daemon identity path
    // only works when the fake keeps its own executable shape (a shebang
    // script). On Windows that identity path is always named
    // `soldr-daemon.exe`, so a batch-script fake hard-linked there would not
    // be a valid Win32 image — a real platform limit, not a reason to reach
    // for `#[cfg(unix)]`. The scenario is exercised on the canonical Linux
    // `guards` lane regardless (see `ci/target-run-ownership.json`
    // `guards-linux-once`).
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        println!(
            "readiness_panic_carries_the_dead_daemons_stderr: skipped on windows \
             (fake daemon is a unix shell script; see module docs)"
        );
        return;
    }

    let root = common::unique_temp_dir("isolated-daemon-readiness-root");
    let home = common::unique_temp_dir("isolated-daemon-readiness-home");
    let script = root.join("fake-daemon");
    common::write_fake_script(&script, &marker_daemon_script());

    let command = Command::new(&script);
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        IsolatedDaemon::spawn_with_command(command, &script, &root, &home)
    }));

    let error = match result {
        Ok(_) => panic!(
            "spawning a daemon that exits immediately must panic instead of returning a ready fixture"
        ),
        Err(error) => error,
    };
    let message = error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| error.downcast_ref::<&str>().map(|value| value.to_string()))
        .expect("panic payload is a string message");
    assert!(
        message.contains(MARKER),
        "readiness panic must surface the dead daemon's own stderr; got:\n{message}",
    );
}
