//! Two-mode process spawning. Free functions only — no module-internal traits.
//!
//! Modes (only two; the dangerous combination `detached + caller-pipes` has no
//! API surface):
//!
//!   * [`spawn_daemon`] — detached lifetime, sanitized file-or-NUL stdio,
//!     sanitized handle list, no console window, ignores parent's Ctrl-C. The
//!     returned [`DaemonChild`] does NOT die when dropped.
//!   * [`spawn`] — contained lifetime, caller-controlled stdio via
//!     [`SpawnStdio`], sanitized handle list, no console window by default
//!     (opt in via [`SpawnStdio::show_console`]), bounded drain. The returned
//!     [`SpawnedChild`] kills the child on Drop.
//!
//! ## Sanitized handle inheritance
//!
//! Both modes inherit ONLY the three stdio handles we resolve here. On
//! Windows we use `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` to whitelist exactly
//! the resolved handles. On Unix the spawned child runs a `pre_exec` closure
//! that walks `/proc/self/fd` (or `/dev/fd`) and closes every fd > 2.
//!
//! Motivation: when a process tree has a pipe-redirected ancestor (Python
//! `subprocess.Popen(stdout=PIPE)`, IDE language-server hosts, CI runners,
//! etc.), every intermediate `CreateProcessW(bInheritHandles=TRUE)` on
//! Windows — and every `fork`+`exec` of a non-`O_CLOEXEC` fd on Unix —
//! duplicates that orphaned pipe write-end into the new child. The original
//! reader at the top never sees EOF.
//!
//! Issue: <https://github.com/zackees/running-process/issues/110>.

#[cfg(unix)]
use std::os::fd::BorrowedFd;
#[cfg(windows)]
use std::os::windows::io::BorrowedHandle;
use std::process::Command;
use std::time::Duration;

/// Selects the base environment used for a newly spawned process.
///
/// Explicit mutations added through [`Command::env`], [`Command::envs`], or
/// [`Command::env_remove`] are applied after the selected base and therefore
/// always win.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EnvironmentPolicy {
    /// Choose from the process lifetime: contained subprocesses inherit,
    /// while detached daemons start from the logged-in user's baseline.
    #[default]
    Auto,
    /// Inherit the spawning process's environment.
    Inherit,
    /// Start from the logged-in user's machine + user environment, discarding
    /// the spawning process's ambient environment except for the documented
    /// Unix locale, time-zone, and temporary-directory allowlist.
    ///
    /// Windows implements this with `CreateEnvironmentBlock`. Unix
    /// reconstructs a clean login environment from the user's identity
    /// (`getpwuid_r` → `USER`/`LOGNAME`/`HOME`/`SHELL`, platform default
    /// `PATH`, carried-over locale/`TZ`/`TMPDIR`), falling back to inheritance
    /// only when the passwd entry cannot be resolved.
    ///
    /// Consumers that need values such as `CARGO_HOME`, `RUSTUP_HOME`,
    /// `SOLDR_*`, credentials, or runner-specific paths must pass them
    /// explicitly on the [`Command`].
    UserBaseline,
    /// Start from an empty environment.
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnLifetime {
    Contained,
    Daemon,
}

impl EnvironmentPolicy {
    pub(crate) fn resolve(self, lifetime: SpawnLifetime) -> Self {
        match (self, lifetime) {
            (Self::Auto, SpawnLifetime::Contained) => Self::Inherit,
            (Self::Auto, SpawnLifetime::Daemon) => Self::UserBaseline,
            (explicit, _) => explicit,
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Caller-supplied stdio bindings for [`spawn`].
///
/// Each of `stdin`, `stdout`, `stderr` is independently a [`StdioSource`].
/// `drain_timeout` bounds the post-mortem wait the watcher thread applies
/// before force-closing any wrapper-held pipe ends so the parent observes
/// EOF after the child exits. `None` means the wrapper never auto-closes;
/// the parent is responsible for closing the pipes when it's done reading.
///
/// `show_console` (Windows-only effect) controls whether the child gets a
/// console window. Default is `false` — `CREATE_NO_WINDOW` is set, so the
/// child has no console regardless of how the parent was launched. Set this
/// to `true` only when you actually want the child to inherit / allocate a
/// console (interactive subprocess that should be visible to the user).
pub struct SpawnStdio<'a> {
    /// Source connected to the child's standard input.
    pub stdin: StdioSource<'a>,
    /// Source connected to the child's standard output.
    pub stdout: StdioSource<'a>,
    /// Source connected to the child's standard error.
    pub stderr: StdioSource<'a>,
    /// Maximum time the watcher waits before closing wrapper-held pipe ends.
    pub drain_timeout: Option<Duration>,
    /// Whether Windows children may inherit or allocate a visible console.
    pub show_console: bool,
}

/// Creation policy for [`spawn_tokio`].
///
/// This compatibility entrypoint lets async daemons keep Tokio's pipe and
/// wait APIs while making `running-process` the sole owner of child-creation
/// policy. It defaults to contained, console-less children.
#[cfg(feature = "client-async")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokioSpawnOptions {
    /// Terminate the child when Tokio's child handle is dropped.
    pub kill_on_drop: bool,
    /// Whether Windows children may inherit or allocate a visible console.
    pub show_console: bool,
}

#[cfg(feature = "client-async")]
impl Default for TokioSpawnOptions {
    fn default() -> Self {
        Self {
            kill_on_drop: true,
            show_console: false,
        }
    }
}

impl Default for SpawnStdio<'_> {
    fn default() -> Self {
        Self {
            stdin: StdioSource::Null,
            stdout: StdioSource::Parent,
            stderr: StdioSource::Parent,
            drain_timeout: Some(Duration::from_secs(2)),
            show_console: false,
        }
    }
}

/// Caller-supplied output bindings for a detached daemon.
///
/// Detached children may write only to the platform null device or to
/// caller-owned file handles. Parent stdio and anonymous pipes are
/// intentionally unavailable: either can retain the launching process's
/// lifetime or fail after that process exits. The child always receives a
/// fresh inheritable duplicate, and the caller retains its original handle.
pub struct DaemonStdio<'a> {
    /// Source connected to the daemon's standard output.
    pub stdout: DaemonStdioSource<'a>,
    /// Source connected to the daemon's standard error.
    pub stderr: DaemonStdioSource<'a>,
}

impl Default for DaemonStdio<'_> {
    fn default() -> Self {
        Self {
            stdout: DaemonStdioSource::Null,
            stderr: DaemonStdioSource::Null,
        }
    }
}

/// Safe output source for a detached daemon.
pub enum DaemonStdioSource<'a> {
    /// Connect this slot to the platform null device (`NUL` / `/dev/null`).
    Null,
    /// Bind this slot to a caller-owned OS handle. The wrapper duplicates the
    /// handle into an inheritable copy for the child.
    #[cfg(windows)]
    Handle(BorrowedHandle<'a>),
    /// Bind this slot to a caller-owned file descriptor. Equivalent to
    /// `DaemonStdioSource::Handle` on Windows.
    #[cfg(unix)]
    Fd(BorrowedFd<'a>),
    #[doc(hidden)]
    _Phantom(std::marker::PhantomData<&'a ()>),
}

/// Per-slot source describing what the child should inherit for one of
/// stdin / stdout / stderr.
pub enum StdioSource<'a> {
    /// Connect this slot to the platform null device (`NUL` / `/dev/null`).
    Null,
    /// Inherit the parent's corresponding standard handle. The kernel
    /// receives a fresh inheritable duplicate; the parent's original slot
    /// is untouched.
    Parent,
    /// Bind this slot to a caller-owned OS handle. The wrapper duplicates
    /// the handle into an inheritable copy for the child; the caller
    /// retains its own handle and is responsible for closing it.
    #[cfg(windows)]
    Handle(BorrowedHandle<'a>),
    /// Bind this slot to a caller-owned file descriptor. Equivalent to
    /// `StdioSource::Handle` on Unix.
    #[cfg(unix)]
    Fd(BorrowedFd<'a>),
    /// Create a fresh anonymous pipe. The child gets one end; the parent
    /// gets the other via [`SpawnedChild`]'s `stdin` / `stdout` / `stderr`
    /// fields.
    Pipe,
    #[doc(hidden)]
    _Phantom(std::marker::PhantomData<&'a ()>),
}

// _Phantom is uninhabitable from outside: PhantomData<&'a ()> is a private
// constructor in practice (the variant is doc(hidden) and not constructed
// anywhere in this crate). It's only here so the `'a` lifetime is always
// used regardless of which cfg branch is active.

/// Handle to a detached daemon spawned via [`spawn_daemon`].
///
/// The daemon child always has stdin connected to the platform null device.
/// Stdout and stderr also default to null, but [`spawn_daemon_with_stdio`]
/// can bind them to caller-owned files. A detached process can never inherit
/// parent stdio or caller pipes through this API. Dropping `DaemonChild` does
/// NOT terminate the daemon; it only closes the OS handle the wrapper held.
/// Call [`DaemonChild::kill`] to terminate.
pub struct DaemonChild {
    pid: u32,
    #[cfg(windows)]
    handle: imp::OwnedHandle,
    #[cfg(unix)]
    child: std::process::Child,
}

impl DaemonChild {
    /// Process ID.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Forcibly terminate the child. Best-effort.
    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            imp::terminate(&self.handle)
        }
        #[cfg(unix)]
        {
            self.child.kill()
        }
    }

    /// Block until the child exits and return its exit code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        #[cfg(windows)]
        {
            imp::wait(&self.handle)
        }
        #[cfg(unix)]
        {
            let status = self.child.wait()?;
            Ok(unix_exit_code(status))
        }
    }

    /// Non-blocking variant of [`Self::wait`].
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            imp::try_wait(&self.handle)
        }
        #[cfg(unix)]
        {
            Ok(self.child.try_wait()?.map(unix_exit_code))
        }
    }
}

/// Handle to a contained child spawned via [`spawn`].
///
/// On Drop, `SpawnedChild` synchronously kills the child:
///   * Windows: closes the Job Object handle; `KILL_ON_JOB_CLOSE` causes the
///     kernel to terminate every process in the job (the child and its
///     descendants).
///   * Unix: `killpg(pgid, SIGKILL)` and `waitpid` to reap.
///
/// The optional `stdin` / `stdout` / `stderr` fields are present when the
/// corresponding [`StdioSource`] was [`StdioSource::Pipe`]; otherwise they
/// are `None`.
pub struct SpawnedChild {
    /// Parent-side pipe for writing to child stdin when requested.
    pub stdin: Option<std::process::ChildStdin>,
    /// Parent-side pipe for reading child stdout when requested.
    pub stdout: Option<std::process::ChildStdout>,
    /// Parent-side pipe for reading child stderr when requested.
    pub stderr: Option<std::process::ChildStderr>,
    pid: u32,
    #[cfg(windows)]
    inner: imp::SpawnedInner,
    #[cfg(unix)]
    inner: unix_impl::SpawnedInner,
}

impl SpawnedChild {
    /// Process ID of the spawned child.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Forcibly terminate the child. Best-effort.
    pub fn kill(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            self.inner.kill()
        }
        #[cfg(unix)]
        {
            self.inner.kill()
        }
    }

    /// Block until the child exits and return its exit code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        #[cfg(windows)]
        {
            self.inner.wait()
        }
        #[cfg(unix)]
        {
            self.inner.wait()
        }
    }

    /// Non-blocking variant of [`Self::wait`].
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        #[cfg(windows)]
        {
            self.inner.try_wait()
        }
        #[cfg(unix)]
        {
            self.inner.try_wait()
        }
    }
}

impl Drop for SpawnedChild {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.inner.shutdown();
        }
        #[cfg(unix)]
        {
            self.inner.shutdown();
        }
    }
}

/// Set on every child spawned through the daemon path, so a process can be
/// recognized as a *declared daemon* rather than inferred to be one.
///
/// # Why a positive marker
///
/// Reapers previously had to infer daemon-ness from the **absence** of
/// [`crate::ORIGINATOR_ENV_VAR`], which `spawn_daemon` strips. But absence is
/// overloaded: it means both "this process deliberately detached itself" and
/// "something in the chain clobbered the environment" — and those are
/// byte-identical at the observation point, so no amount of process-lineage
/// tracking can separate them. See zackees/clud#522, where an
/// ancestry-fallback proposal and a daemon exemption read the same signal and
/// drew opposite conclusions.
///
/// A positive declaration removes the ambiguity: only a process that actually
/// went through the daemon path carries this.
///
/// # Caveat
///
/// This is still an environment variable, so a chain that strips
/// `RUNNING_PROCESS_ORIGINATOR` strips this too. It narrows the ambiguous case
/// rather than eliminating it; a durable answer would need the daemon's
/// supervisor to register the PID somewhere the reaper can read.
///
/// Distinct from `RUNNING_PROCESS_DAEMON_SCOPE`, which names a broker scope
/// and is unrelated.
pub const DAEMON_MARKER_ENV_VAR: &str = "RUNNING_PROCESS_IS_DAEMON";

/// Spawn `command` as a detached daemon. NUL stdio, sanitized handles,
/// no console window, ignores parent's Ctrl-C / SIGINT (Windows:
/// `CREATE_NEW_PROCESS_GROUP` + `DETACHED_PROCESS`; Unix: `setsid` puts the
/// daemon in a new session so it's not in the parent's foreground group).
///
/// Use [`spawn_daemon_with_stdio`] when the daemon must write to stable
/// caller-owned files. Parent stdio and anonymous pipes remain unavailable
/// for detached children.
pub fn spawn_daemon(command: &mut Command) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(
        command,
        DaemonStdio::default(),
        EnvironmentPolicy::Auto,
        false,
    )
}

/// Spawn a detached daemon with file-or-NUL stdout and stderr.
///
/// Stdin remains connected to null. The supplied handles are duplicated into
/// the sanitized child handle list, so the caller can close its files after
/// this function returns without affecting the daemon.
pub fn spawn_daemon_with_stdio(
    command: &mut Command,
    stdio: DaemonStdio<'_>,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_with_stdio_and_env_policy(command, stdio, EnvironmentPolicy::Auto)
}

/// [`spawn_daemon_with_stdio`] with an explicit environment policy.
pub fn spawn_daemon_with_stdio_and_env_policy(
    command: &mut Command,
    stdio: DaemonStdio<'_>,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(command, stdio, policy, false)
}

/// Like [`spawn_daemon`] but with explicit control over whether the
/// daemon's inherited env is passed through to the child.
///
/// `clear_env = false` uses [`EnvironmentPolicy::Auto`], matching
/// [`spawn_daemon`].
///
/// `clear_env = true`: child sees ONLY the explicit `command.env(...)`
/// entries. Mirrors `command.env_clear()` semantics for callers using
/// the manual `CreateProcessW` path (Rust stdlib's `env_clear` flag
/// isn't observable through `Command::get_envs`, so our sanitized
/// spawn machinery can't otherwise honour it).
pub fn spawn_daemon_with_clear_env(
    command: &mut Command,
    clear_env: bool,
) -> std::io::Result<DaemonChild> {
    let policy = if clear_env {
        EnvironmentPolicy::Clear
    } else {
        EnvironmentPolicy::Auto
    };
    spawn_daemon_inner(command, DaemonStdio::default(), policy, false)
}

/// Spawn a detached daemon using an explicit environment policy.
///
/// [`EnvironmentPolicy::Auto`] resolves to
/// [`EnvironmentPolicy::UserBaseline`] for daemons, excluding unlisted
/// ambient variables. Use [`EnvironmentPolicy::Inherit`] as the explicit
/// escape hatch for trusted callers that require the full parent environment.
/// In every mode, explicit command environment additions, overrides, and
/// removals are applied last.
pub fn spawn_daemon_with_env_policy(
    command: &mut Command,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(command, DaemonStdio::default(), policy, false)
}

/// Like [`spawn_daemon`], but the child also **breaks away from any Job
/// Object the spawner belongs to** (Windows; a no-op elsewhere).
///
/// Use this for a daemon that must outlive the process tree that happened to
/// start it — a build cache server, a language server, anything discovered
/// and reused by later, unrelated invocations.
///
/// # Why this is separate from [`spawn_daemon`]
///
/// "Detached lifetime" and "escapes my caller's containment" are different
/// properties, and callers genuinely want them independently. Job Object
/// membership is inherited by every descendant at any depth, and jobs created
/// by this crate carry `KILL_ON_JOB_CLOSE` — so without breakaway the kernel
/// terminates such a daemon the moment the spawner's job handle drops, no
/// matter how detached the daemon made itself.
///
/// But making that unconditional breaks the opposite use: a child spawned as
/// a daemon purely to obtain a sanitized handle list must stay inside the
/// caller's job. `testbins/src/bin/spawner.rs` does exactly this, and
/// `containment_test::test_contained_group_kills_grandchildren` fails if its
/// sleepers escape.
///
/// # Refusal is not silent
///
/// `CREATE_BREAKAWAY_FROM_JOB` is *refused*, not ignored, when the spawner
/// sits inside a job that lacks `JOB_OBJECT_LIMIT_BREAKAWAY_OK`:
/// `CreateProcessW` fails with `ERROR_ACCESS_DENIED`. Outer jobs we do not
/// control are common (CI runners, container supervisors, debuggers), so the
/// spawn retries once with the flag cleared — a daemon that stays contained
/// beats a daemon that fails to start.
pub fn spawn_daemon_breaking_away_from_job(command: &mut Command) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(
        command,
        DaemonStdio::default(),
        EnvironmentPolicy::Auto,
        true,
    )
}

/// [`spawn_daemon_breaking_away_from_job`] with an explicit env policy.
pub fn spawn_daemon_breaking_away_with_env_policy(
    command: &mut Command,
    policy: EnvironmentPolicy,
) -> std::io::Result<DaemonChild> {
    spawn_daemon_inner(command, DaemonStdio::default(), policy, true)
}

/// Apply the daemon self-declaration to `command`. Split out from
/// [`spawn_daemon_inner`] so the policy is unit-testable without spawning a
/// real detached process.
pub(crate) fn mark_as_daemon(command: &mut Command) {
    command.env(DAEMON_MARKER_ENV_VAR, "1");
}

fn spawn_daemon_inner(
    command: &mut Command,
    stdio: DaemonStdio<'_>,
    policy: EnvironmentPolicy,
    breakaway: bool,
) -> std::io::Result<DaemonChild> {
    // Every daemon-spawn variant funnels through here, so this is the one
    // place that can mark them all — including the free functions consumers
    // like zccache call directly.
    mark_as_daemon(command);
    let policy = policy.resolve(SpawnLifetime::Daemon);
    #[cfg(windows)]
    {
        imp::spawn_daemon(command, stdio, policy, breakaway)
    }
    #[cfg(unix)]
    {
        // Unix has no Job Object; `setsid` already detaches the daemon from
        // the parent's session and process group, so breakaway is moot.
        let _ = breakaway;
        unix_impl::spawn_daemon(command, stdio, policy)
    }
}

/// Spawn `command` as a contained child with caller-controlled stdio.
/// Sanitized handles, and no console (`DETACHED_PROCESS` on Windows). Child
/// dies when the returned
/// [`SpawnedChild`] is dropped.
pub fn spawn(command: &mut Command, stdio: SpawnStdio<'_>) -> std::io::Result<SpawnedChild> {
    spawn_with_env_policy(command, stdio, EnvironmentPolicy::Auto)
}

/// Spawn a contained child using an explicit environment policy.
pub fn spawn_with_env_policy(
    command: &mut Command,
    stdio: SpawnStdio<'_>,
    policy: EnvironmentPolicy,
) -> std::io::Result<SpawnedChild> {
    let policy = policy.resolve(SpawnLifetime::Contained);
    #[cfg(windows)]
    {
        imp::spawn(command, stdio, policy)
    }
    #[cfg(unix)]
    {
        unix_impl::spawn(command, stdio, policy)
    }
}

/// Spawn a Tokio child through the centralized process-creation boundary.
///
/// Callers retain Tokio's async stdin/stdout/stderr and wait APIs, but may not
/// apply platform creation flags themselves. On Windows, console suppression
/// is owned here. Use [`spawn`] when the stronger sanitized-handle-list and
/// kill-on-close Job Object contract is required.
#[cfg(feature = "client-async")]
pub fn spawn_tokio(
    command: &mut tokio::process::Command,
    options: TokioSpawnOptions,
) -> std::io::Result<tokio::process::Child> {
    command.kill_on_drop(options.kill_on_drop);
    #[cfg(windows)]
    command.creation_flags(tokio_creation_flags(options.show_console));
    #[cfg(not(windows))]
    let _ = options.show_console;
    command.spawn()
}

#[cfg(all(feature = "client-async", windows))]
fn tokio_creation_flags(show_console: bool) -> u32 {
    if show_console {
        0
    } else {
        // CREATE_NO_WINDOW. Keep this policy private so consumers cannot
        // duplicate or partially apply Windows creation flags.
        0x0800_0000
    }
}

#[cfg(unix)]
fn unix_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(1))
}

// ── Windows implementation ──────────────────────────────────────────────────

#[cfg(windows)]
#[path = "spawn_imp_windows.rs"]
mod imp;

#[cfg(unix)]
#[path = "spawn_imp_unix.rs"]
mod unix_impl;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_stdio_default_has_sane_values() {
        let s = SpawnStdio::default();
        assert!(matches!(s.stdin, StdioSource::Null));
        assert!(matches!(s.stdout, StdioSource::Parent));
        assert!(matches!(s.stderr, StdioSource::Parent));
        assert_eq!(s.drain_timeout, Some(Duration::from_secs(2)));
        // No console window by default — opt-in only.
        assert!(!s.show_console);
    }

    #[test]
    fn daemon_stdio_default_is_null() {
        let stdio = DaemonStdio::default();
        assert!(matches!(stdio.stdout, DaemonStdioSource::Null));
        assert!(matches!(stdio.stderr, DaemonStdioSource::Null));
    }

    #[test]
    fn auto_environment_policy_depends_on_lifetime() {
        assert_eq!(
            EnvironmentPolicy::Auto.resolve(SpawnLifetime::Contained),
            EnvironmentPolicy::Inherit
        );
        assert_eq!(
            EnvironmentPolicy::Auto.resolve(SpawnLifetime::Daemon),
            EnvironmentPolicy::UserBaseline
        );
    }

    #[test]
    fn explicit_environment_policy_is_not_rewritten() {
        for policy in [
            EnvironmentPolicy::Inherit,
            EnvironmentPolicy::UserBaseline,
            EnvironmentPolicy::Clear,
        ] {
            assert_eq!(policy.resolve(SpawnLifetime::Contained), policy);
            assert_eq!(policy.resolve(SpawnLifetime::Daemon), policy);
        }
    }

    #[cfg(feature = "client-async")]
    #[test]
    fn tokio_spawn_defaults_to_contained_consoleless_children() {
        assert_eq!(
            TokioSpawnOptions::default(),
            TokioSpawnOptions {
                kill_on_drop: true,
                show_console: false,
            }
        );
    }

    #[cfg(all(feature = "client-async", windows))]
    #[test]
    fn tokio_spawn_owns_console_creation_flags() {
        assert_eq!(tokio_creation_flags(false), 0x0800_0000);
        assert_eq!(tokio_creation_flags(true), 0);
    }
}
