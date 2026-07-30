//! Stable-image relocation for daemon entrypoints.

use crate::core::SoldrPaths;

use super::spawn;

/// Whether running-process launched this process through its detached-daemon
/// boundary.
///
/// Keep the marker interpretation next to the process-creation boundary so
/// callers do not need their own direct dependency on running-process.
pub fn current_process_is_declared_daemon() -> bool {
    std::env::var_os(running_process::DAEMON_MARKER_ENV_VAR).as_deref()
        == Some(std::ffi::OsStr::new("1"))
}

/// Re-exec from the runtime root using the one correct policy for a managed
/// daemon entrypoint: detach the relocated replacement when this process was
/// launched through running-process's managed daemon boundary (marker present,
/// no console), and wait on it — preserving the caller's terminal — only for a
/// genuine user-invoked foreground run (no marker).
///
/// Both daemon entrypoints (`daemon_entry::run` and the `soldr daemon start
/// --foreground` arm in `soldr_main`) MUST route through here rather than
/// passing a literal to [`reexec_from_runtime_root`]. Hardcoding `false` on the
/// managed `via_self` path is exactly what popped a visible `soldr-daemon`
/// console on Windows (soldr#2039); centralizing the decision makes that
/// bypass impossible to reintroduce by a wrong flag at a call site.
pub fn reexec_from_runtime_root_for_daemon_entry() {
    reexec_from_runtime_root(current_process_is_declared_daemon());
}

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
