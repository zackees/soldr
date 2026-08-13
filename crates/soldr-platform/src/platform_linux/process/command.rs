//! Linux command configuration.

use std::process::Command;

/// Linux has no console-window flash to suppress.
pub fn suppress_console(_command: &mut Command) {}

/// Put the child in its own process group so a tree kill can target the
/// whole group with one signal.
pub fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Replace argv[0] so the child reports the multicall name it was invoked
/// as rather than the physical binary.
pub fn arg0(command: &mut Command, name: &str) {
    use std::os::unix::process::CommandExt;
    command.arg0(name);
}
