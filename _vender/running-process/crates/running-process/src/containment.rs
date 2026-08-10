//! Process group with originator-env injection that delegates to the
//! two-mode [`crate::spawn()`] surface.
//!
//! `ContainedProcessGroup` no longer carries OS-level containment state of
//! its own (the new `spawn` builds a Job Object per-spawn on Windows and
//! places each child in its own process group on Unix). The group's
//! responsibility is now scoped to:
//!
//! - holding an optional `originator` label,
//! - injecting [`ORIGINATOR_ENV_VAR`] into every *contained* child the group
//!   spawns, and stripping it from every *daemon* child (a daemon outlives its
//!   spawner, so it must not be attributable to it — see
//!   [`ContainedProcessGroup::spawn_daemon`]),
//! - dispatching to either [`crate::spawn()`] or [`crate::spawn_daemon`].
//!
//! # `RUNNING_PROCESS_ORIGINATOR` environment variable
//!
//! When an `originator` is set on a `ContainedProcessGroup`, all spawned child
//! processes inherit the environment variable `RUNNING_PROCESS_ORIGINATOR` with
//! the format `TOOL:PID`, where:
//!
//! - **TOOL** is the originator name (e.g., `"CLUD"`, `"JUPYTER"`)
//! - **PID** is the process ID of the parent that spawned the group
//!
//! Example value: `RUNNING_PROCESS_ORIGINATOR=CLUD:12345`
//!
//! ## Purpose
//!
//! This env var enables **cross-process session discovery** after crashes.
//!
//! ## Example
//!
//! ```no_run
//! use running_process::{ContainedProcessGroup, SpawnStdio};
//!
//! let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
//! let mut cmd = std::process::Command::new("sleep");
//! cmd.arg("60");
//! let _child = group.spawn(&mut cmd, SpawnStdio::default()).unwrap();
//! ```

use std::process::Command;

use crate::spawn::{
    spawn as free_spawn, spawn_daemon as free_spawn_daemon, DaemonChild, SpawnStdio, SpawnedChild,
};

/// The environment variable name injected into child processes for
/// cross-process session discovery.
pub const ORIGINATOR_ENV_VAR: &str = "RUNNING_PROCESS_ORIGINATOR";

/// A logical group of spawned processes that share an originator label.
///
/// Each [`ContainedProcessGroup::spawn`] call builds its own OS-level
/// containment (Job Object on Windows, process-group on Unix), so the
/// group itself is just metadata.
pub struct ContainedProcessGroup {
    originator: Option<String>,
}

/// Format the originator env var value: `TOOL:PID`.
fn format_originator_value(tool: &str) -> String {
    format!("{}:{}", tool, std::process::id())
}

impl ContainedProcessGroup {
    /// Create a new process group without an originator.
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self { originator: None })
    }

    /// Create a new process group with an originator name.
    pub fn with_originator(originator: &str) -> Result<Self, std::io::Error> {
        Ok(Self {
            originator: Some(originator.to_string()),
        })
    }

    /// Returns the originator name, if set.
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    /// Returns the full originator env var value (`TOOL:PID`), if set.
    pub fn originator_value(&self) -> Option<String> {
        self.originator.as_ref().map(|o| format_originator_value(o))
    }

    fn inject_originator_env(&self, command: &mut Command) {
        if let Some(ref originator) = self.originator {
            command.env(ORIGINATOR_ENV_VAR, format_originator_value(originator));
        } else {
            command.env_remove(ORIGINATOR_ENV_VAR);
        }
    }

    /// Spawn a contained child process. The child is contained by its own
    /// Job Object on Windows / process group on Unix and is killed when
    /// the returned [`SpawnedChild`] is dropped.
    pub fn spawn(
        &self,
        command: &mut Command,
        stdio: SpawnStdio<'_>,
    ) -> Result<SpawnedChild, std::io::Error> {
        self.inject_originator_env(command);
        free_spawn(command, stdio)
    }

    /// Spawn a detached daemon child. The child has NUL stdio, a sanitized
    /// handle list, and survives the returned [`DaemonChild`] being
    /// dropped. To terminate, call [`DaemonChild::kill`].
    ///
    /// The originator env var is deliberately **stripped**, never injected.
    ///
    /// A daemon outlives the process that happened to start it — that is the
    /// entire point of spawning one. Tagging it `TOOL:<spawner pid>` makes it
    /// indistinguishable from an abandoned descendant, so as soon as the
    /// spawner exits, any originator-based reaper sees a live process whose
    /// originator is dead and kills it. Build-cache daemons (zccache, sccache)
    /// are the common casualty: they are started lazily by whichever compiler
    /// invocation happens to run first, inherit that session's tag, and get
    /// reaped when it ends — taking the warm cache down with them.
    ///
    /// This also restores the intent of the daemon environment policy.
    /// [`crate::EnvironmentPolicy::Auto`] resolves to `UserBaseline` for
    /// daemons, which rebuilds the environment from the user's login identity
    /// and drops process-local variables. Explicit `command.env(...)` entries
    /// are applied *last*, so injecting here silently defeated that policy for
    /// this one variable.
    pub fn spawn_daemon(&self, command: &mut Command) -> Result<DaemonChild, std::io::Error> {
        Self::strip_originator_env(command);
        free_spawn_daemon(command)
    }

    /// Remove the originator tag from `command`. Split out from
    /// [`Self::spawn_daemon`] so the policy is unit-testable without
    /// spawning a real detached process.
    fn strip_originator_env(command: &mut Command) {
        command.env_remove(ORIGINATOR_ENV_VAR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn originator_entry(command: &Command) -> Option<Option<&std::ffi::OsStr>> {
        let key = std::ffi::OsStr::new(ORIGINATOR_ENV_VAR);
        command.get_envs().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The two halves must compose: a daemon drops the originator tag *and*
    /// carries the positive marker. Absence alone is ambiguous — it also
    /// describes a process whose environment was clobbered — so a reaper
    /// needs the declaration, not just the missing tag (clud#522).
    #[test]
    fn daemon_declares_itself_and_drops_the_originator_tag() {
        let mut cmd = Command::new("echo");
        cmd.env(ORIGINATOR_ENV_VAR, "CLUD:12345");
        ContainedProcessGroup::strip_originator_env(&mut cmd);
        crate::spawn::mark_as_daemon(&mut cmd);

        assert_eq!(
            originator_entry(&cmd),
            Some(None),
            "the originator tag must still be removed"
        );
        let marker = std::ffi::OsStr::new(crate::DAEMON_MARKER_ENV_VAR);
        assert_eq!(
            cmd.get_envs()
                .find(|(k, _)| *k == marker)
                .and_then(|(_, v)| v),
            Some(std::ffi::OsStr::new("1")),
            "a daemon must positively declare itself"
        );
    }

    /// A daemon must not carry its spawner's originator tag: it outlives the
    /// spawner, so the tag turns it into reaper bait the moment the spawner
    /// exits. `env_remove` must win even when the caller explicitly set it.
    #[test]
    fn spawn_daemon_strips_originator_env() {
        let mut cmd = Command::new("echo");
        cmd.env(ORIGINATOR_ENV_VAR, "CLUD:12345");
        ContainedProcessGroup::strip_originator_env(&mut cmd);
        assert_eq!(
            originator_entry(&cmd),
            Some(None),
            "daemon command must carry an explicit removal of {ORIGINATOR_ENV_VAR}"
        );
    }

    /// The stripping must be unconditional — a group *with* an originator is
    /// exactly the case that used to inject the tag into daemons.
    #[test]
    fn spawn_daemon_strips_originator_even_for_tagged_group() {
        let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
        assert!(group.originator().is_some());
        let mut cmd = Command::new("echo");
        ContainedProcessGroup::strip_originator_env(&mut cmd);
        assert_eq!(originator_entry(&cmd), Some(None));
    }

    /// Contrast: *contained* children keep the tag. That is what makes the
    /// on-exit reaper able to find genuinely abandoned descendants.
    #[test]
    fn contained_spawn_still_injects_originator_env() {
        let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
        let mut cmd = Command::new("echo");
        group.inject_originator_env(&mut cmd);
        let expected = format!("CLUD:{}", std::process::id());
        assert_eq!(
            originator_entry(&cmd),
            Some(Some(std::ffi::OsStr::new(expected.as_str())))
        );
    }

    #[test]
    fn contained_process_group_creates_successfully() {
        let group = ContainedProcessGroup::new();
        assert!(group.is_ok());
    }

    #[test]
    fn with_originator_creates_successfully() {
        let group = ContainedProcessGroup::with_originator("CLUD");
        assert!(group.is_ok());
        let group = group.unwrap();
        assert_eq!(group.originator(), Some("CLUD"));
    }

    #[test]
    fn originator_value_format() {
        let group = ContainedProcessGroup::with_originator("CLUD").unwrap();
        let value = group.originator_value().unwrap();
        let expected = format!("CLUD:{}", std::process::id());
        assert_eq!(value, expected);
    }

    #[test]
    fn no_originator_returns_none() {
        let group = ContainedProcessGroup::new().unwrap();
        assert!(group.originator().is_none());
        assert!(group.originator_value().is_none());
    }

    #[test]
    fn format_originator_value_correct() {
        let value = format_originator_value("JUPYTER");
        let parts: Vec<&str> = value.splitn(2, ':').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "JUPYTER");
        assert_eq!(parts[1], std::process::id().to_string());
    }
}
