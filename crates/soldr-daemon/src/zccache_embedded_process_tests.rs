use std::io;
use std::process::Command;
use std::time::Duration;

#[derive(Debug)]
pub(super) struct CompilerProbeOutput {
    pub(super) success: bool,
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

impl From<running_process::RunOutput> for CompilerProbeOutput {
    fn from(output: running_process::RunOutput) -> Self {
        Self {
            success: output.exit_code == 0,
            exit_code: Some(output.exit_code),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

pub(super) fn bounded_output(command: Command) -> io::Result<running_process::RunOutput> {
    running_process::run_std_command_bounded(command, Some(Duration::from_secs(30)), 64 * 1024)
        .map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(windows)]
pub(super) fn contained_status(command: Command) -> io::Result<i32> {
    bounded_output(command).map(|output| output.exit_code)
}
