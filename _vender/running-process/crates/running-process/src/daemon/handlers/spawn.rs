//! `SpawnDaemon` handler and helpers for spawning + tracking detached
//! commands.

use std::process::Command;
use std::sync::Arc;

use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, KeyValue, SpawnDaemonResponse, StatusCode,
};
use crate::ORIGINATOR_ENV_VAR;
use sysinfo::{Pid, ProcessRefreshKind, System};

use crate::daemon::registry::{self, TrackedEntry};

use super::util::{error_response, unix_now_seconds};
use super::DaemonState;

#[derive(Debug)]
struct SpawnedChild {
    pid: u32,
    created_at: f64,
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        // IMPORTANT: do NOT use `raw_arg` here. running_process's
        // sanitized spawn rebuilds the Win32 command line from
        // `cmd.get_program()` + `cmd.get_args()` (it can't reach
        // Rust stdlib's internal `raw_arg` storage), so any
        // raw_arg-only args are LOST and cmd.exe is launched with
        // zero arguments — it exits immediately with no output and
        // never appears in `list_active`. Use regular `arg()` and
        // let Rust's CRT-escape rule + cmd's `/S` flag strip the
        // outer quotes again on the cmd side.
        let mut cmd = Command::new("cmd.exe");
        cmd.arg("/D").arg("/S").arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        // Use absolute /bin/sh so callers that override PATH via
        // SpawnCommandRequest::with_env (or with_env_replace) don't
        // break shell resolution. POSIX mandates /bin/sh; both Linux
        // and macOS satisfy this.
        //
        // `-c` (not `-lc`): we deliberately avoid login-shell mode so
        // /etc/profile and /etc/profile.d/*.sh don't post-mutate the
        // env the caller set. On Ubuntu CI runners /etc/profile.d
        // prepends /snap/bin to PATH, which would silently override
        // a caller's PATH override. Matches Python's
        // subprocess.Popen(shell=True) behavior.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

fn process_created_at(pid: u32) -> Option<f64> {
    let mut system = System::new();
    let sysinfo_pid = Pid::from_u32(pid);
    system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());
    system
        .process(sysinfo_pid)
        .map(|process| process.start_time() as f64)
}

/// Normalize the caller-supplied env list into a deterministic
/// `(key, value)` sequence ready for `Command::envs`.
///
/// On Windows, env var names are case-insensitive at the kernel level but
/// Rust's `Command::env` collapses duplicates via a case-insensitive
/// `EnvKey` with **last-write-wins** semantics. If a caller passes both
/// `("PATH", inherited)` and `("Path", override)` and we hand them to
/// `Command::envs` in iteration order, whichever was inserted last wins —
/// and HashMap / protobuf-map iteration order would race that.
///
/// We dedup case-insensitively here, preserving the LAST entry per
/// case-folded key, so the caller's intended override always wins
/// regardless of upstream ordering.
fn canonical_env_pairs(env: &[KeyValue]) -> Vec<(String, String)> {
    #[cfg(windows)]
    {
        use std::collections::BTreeMap;
        let mut seen: BTreeMap<String, (String, String)> = BTreeMap::new();
        for kv in env {
            seen.insert(
                kv.key.to_ascii_uppercase(),
                (kv.key.clone(), kv.value.clone()),
            );
        }
        seen.into_values().collect()
    }
    #[cfg(not(windows))]
    {
        env.iter()
            .map(|kv| (kv.key.clone(), kv.value.clone()))
            .collect()
    }
}

fn spawn_and_track_detached(
    command_text: &str,
    cwd: &str,
    env: &[KeyValue],
    clear_inherited_env: bool,
    originator: &str,
    state: &DaemonState,
) -> Result<SpawnedChild, String> {
    let mut command = shell_command(command_text);

    if !cwd.is_empty() {
        command.current_dir(cwd);
    }
    // Two env modes, gated on `clear_inherited_env`:
    //
    // - false (default, backward-compatible): LAYER the caller's env on
    //   top of the daemon's inherited env. The subprocess sees
    //   <daemon env> ∪ <env>, the caller's entries winning ties.
    //
    // - true: REPLACE — the subprocess sees ONLY the caller's env. The
    //   daemon's env is not inherited. Mirrors Python's
    //   `subprocess.Popen(env=…)` semantic; useful for sandbox-style
    //   compiler wrapping where you want a deterministic env. Windows
    //   callers will typically still need SystemRoot in this list so
    //   `cmd.exe` can load its DLLs (see #115/PR review for context).
    if clear_inherited_env {
        command.env_clear();
    }
    if !env.is_empty() {
        command.envs(canonical_env_pairs(env));
    }
    if !originator.is_empty() {
        command.env(ORIGINATOR_ENV_VAR, originator);
    }

    // Route through `spawn_daemon_with_clear_env` so the child gets the
    // structurally-safe sanitized handle list (no orphan inheritable
    // handles), NUL stdio, CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP
    // on Windows (no console popup, ignores parent's Ctrl-C), and setsid
    // + close-extra-fds on Unix. The `clear_env` flag is the bit that
    // makes Rust stdlib's `command.env_clear()` actually observable
    // through our manual CreateProcessW path on Windows.
    let mut detached = crate::spawn_daemon_with_clear_env(&mut command, clear_inherited_env)
        .map_err(|e| format!("failed to spawn detached command: {e}"))?;

    let pid = detached.id();
    let created_at = process_created_at(pid).unwrap_or_else(unix_now_seconds);
    let created_at_ms = registry::created_at_to_ms(created_at);

    let entry = TrackedEntry {
        pid,
        created_at_ms,
        kind: "subprocess".to_string(),
        command: command_text.to_string(),
        cwd: cwd.to_string(),
        originator: originator.to_string(),
        containment: "detached".to_string(),
        registered_at: unix_now_seconds(),
    };

    if let Err(e) = state.registry.register(entry) {
        let _ = detached.kill();
        let _ = detached.wait();
        return Err(format!("registry error: {e}"));
    }

    let registry = Arc::clone(&state.registry);
    std::thread::spawn(move || {
        let _ = detached.wait();
        let _ = registry.unregister_exact(pid, created_at_ms);
    });

    Ok(SpawnedChild { pid, created_at })
}

/// Handle a `SpawnDaemon` request by spawning and tracking a detached command.
pub fn handle_spawn_daemon(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.spawn_daemon else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing spawn_daemon payload".into(),
        );
    };

    let command_text = req.command.trim();
    if command_text.is_empty() {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "command must not be empty".into(),
        );
    }

    let effective_originator = if req.originator.trim().is_empty() {
        request.client_name.clone()
    } else {
        req.originator.clone()
    };

    match spawn_and_track_detached(
        command_text,
        &req.cwd,
        &req.env,
        req.clear_inherited_env,
        &effective_originator,
        state,
    ) {
        Ok(spawned) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: String::new(),
            spawn_daemon: Some(SpawnDaemonResponse {
                pid: spawned.pid,
                created_at: spawned.created_at,
                command: command_text.to_string(),
                cwd: req.cwd.clone(),
                originator: effective_originator,
                containment: "detached".to_string(),
            }),
            ..Default::default()
        },
        Err(message) => error_response(request.id, StatusCode::Internal, message),
    }
}
