//! Stable-image relocation for daemon entrypoints.

use crate::core::SoldrPaths;

use super::spawn;

/// Re-exec the daemon from its stable runtime image before taking ownership.
///
/// This function lives in the daemon crate so the entire production
/// process-creation surface is covered by the crate-scoped Dylint.
///
/// Managed startup uses `detach_replacement = true`: its first image is only a
/// short-lived relocation trampoline, and exiting it immediately leaves one
/// long-lived daemon. Explicit foreground startup keeps its terminal contract
/// by waiting for the relocated child through running-process.
pub fn reexec_from_runtime_root(detach_replacement: bool) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let Some(target) = crate::self_relocate::daemon_should_reexec(&paths, &current) else {
        return;
    };

    let mut command = std::process::Command::new(&target);
    command.args(std::env::args_os().skip(1));
    command.env(crate::self_relocate::DAEMON_REEXEC_MARKER_ENV_VAR, "1");
    eprintln!(
        "soldr-daemon: re-executing from {} so this process does not pin {} for its lifetime \
         (soldr#1987)",
        target.display(),
        current.display()
    );
    if detach_replacement {
        spawn_detached_replacement(&mut command, &paths, &target);
    } else {
        spawn_foreground_replacement(&mut command, &target);
    }
}

fn spawn_detached_replacement(
    command: &mut std::process::Command,
    paths: &SoldrPaths,
    target: &std::path::Path,
) {
    let log_file = spawn::open_spawn_log_at(&paths.root.join("daemon-spawn.log"));
    let stdio = spawn::daemon_stdio(log_file.as_ref());
    match running_process::spawn_daemon_with_stdio_and_env_policy(
        command,
        stdio,
        running_process::EnvironmentPolicy::Inherit,
    ) {
        Ok(_) => std::process::exit(0),
        Err(err) => eprintln!(
            "soldr-daemon: could not re-exec from {}: {err}; continuing in place",
            target.display()
        ),
    }
}

fn spawn_foreground_replacement(command: &mut std::process::Command, target: &std::path::Path) {
    let stdio = running_process::SpawnStdio {
        stdin: running_process::StdioSource::Parent,
        stdout: running_process::StdioSource::Parent,
        stderr: running_process::StdioSource::Parent,
        drain_timeout: None,
        show_console: true,
    };
    match running_process::spawn_with_env_policy(
        command,
        stdio,
        running_process::EnvironmentPolicy::Inherit,
    ) {
        Ok(mut child) => match child.wait() {
            Ok(code) => std::process::exit(code),
            Err(err) => eprintln!(
                "soldr-daemon: relocated foreground process at {} could not be waited: {err}; \
                 continuing in place",
                target.display()
            ),
        },
        Err(err) => eprintln!(
            "soldr-daemon: could not re-exec from {}: {err}; continuing in place",
            target.display()
        ),
    }
}
