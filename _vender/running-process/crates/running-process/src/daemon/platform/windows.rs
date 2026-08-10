// Windows-specific daemon operations.

/// Re-spawn the current executable as a detached background process.
///
/// The spawned child receives all of `args` plus `--daemon-internal` so it
/// knows it is already running detached. The current process exits after a
/// successful spawn.
pub fn daemonize(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(args);
    cmd.arg("--daemon-internal");
    crate::spawn_daemon(&mut cmd)
        .map_err(|e| format!("failed to spawn detached daemon: {e}").to_string())?;

    // The parent exits, returning the user to the shell.
    std::process::exit(0);
}
