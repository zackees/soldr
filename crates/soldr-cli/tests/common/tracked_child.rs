//! A spawned fixture child whose deadline is real on every OS (soldr#3128).
//!
//! The shape this replaces, written out because it looks correct and is not:
//!
//! ```ignore
//! let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
//! if child.wait_timeout(budget)?.is_none() {
//!     let _ = child.kill();
//!     let output = child.wait_with_output()?;   // <-- can block forever
//!     panic!("timed out\nstdout:\n{}", ...);    // <-- therefore never runs
//! }
//! ```
//!
//! Two independent defects, both observed on the Windows target-run lanes in
//! soldr#3128, where the fixture's own 100 s panic never printed and nextest
//! reported a bare 300 s `TIMEOUT` with empty captured output:
//!
//! 1. **`kill()` is one process, the deadline covers a tree.** On Windows
//!    `Child::kill` is `TerminateProcess` on the direct child only; on Unix it
//!    is `SIGKILL` to one pid. A `soldr cargo build` fixture has a cargo child
//!    and rustc/shim grandchildren, and every one of them inherited the piped
//!    stdout/stderr handles. Killing the root leaves the pipe *open*, because
//!    a pipe reaches EOF only when the last writer closes it.
//! 2. **Collection is unbounded.** `wait_with_output()` reads both pipes to
//!    EOF, so with a surviving grandchild holding a write handle it never
//!    returns -- and the panic that carries the diagnostic is on the line
//!    after it. The normal-exit path has the same exposure: a build that
//!    exits 0 while a lingering descendant holds the pipe hangs there too.
//!
//! So this module fixes both halves at once:
//!
//! * pipes are drained by threads started at spawn time into shared buffers,
//!   so the child can never stall on a full pipe and a *snapshot* of whatever
//!   arrived is available at any instant without blocking on EOF;
//! * the timeout path terminates the whole process tree through
//!   [`soldr_platform::process::terminate::terminate_tree`] -- the same verified
//!   leaves-first walk (Windows) / process-group signal (Unix) the cargo front
//!   door uses for its own timeouts -- and then waits only a bounded grace for
//!   the drains to settle before returning what it has.
//!
//! On Unix the group signal only reaches descendants if the root is a group
//! leader, so [`spawn_tracked`] applies
//! [`soldr_platform::process::command::configure_process_group`]. A fixture
//! spawning `soldr cargo ...` should additionally set
//! `SOLDR_INTERNAL_INHERIT_PROCESS_GROUP=1`
//! (`cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV`) so the front door
//! keeps its cargo descendant in that same group instead of opening a new one
//! of its own -- see `cli_cargo_doc_routes.rs`, which does the same.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

/// How long to keep waiting for the drain threads to see EOF after the child
/// (or its tree) is gone. Only a survivor still holding a write handle can
/// exceed this, which is exactly the condition being bounded -- so expiring
/// here is reported, not waited out.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the direct child to be reapable after the tree kill.
const TREE_KILL_REAP_BUDGET: Duration = Duration::from_secs(10);

/// One pipe being drained by a background thread into a shared buffer.
struct DrainedPipe {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl DrainedPipe {
    fn spawn<R: Read + Send + 'static>(mut reader: R) -> Self {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let sink = Arc::clone(&buf);
        let finished = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let mut guard = sink.lock().unwrap_or_else(|e| e.into_inner());
                        guard.extend_from_slice(&chunk[..read]);
                    }
                }
            }
            finished.store(true, Ordering::Release);
        });
        Self { buf, done }
    }

    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Copy out what has arrived so far. Never blocks on the child.
    fn snapshot(&self) -> Vec<u8> {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What a tracked child produced, and how it ended.
pub(crate) struct TrackedOutput {
    /// `None` only when the direct child could not be reaped even after the
    /// tree kill -- itself a finding worth printing.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The deadline expired and the tree was terminated.
    pub timed_out: bool,
    /// Human-readable outcome of the tree kill, present iff `timed_out`.
    /// Distinguishes "the whole tree was killed" from "only the root was" --
    /// soldr#2605's weaker outcome, which must not read as a clean kill.
    pub tree_kill: Option<&'static str>,
    /// Whether both drains reached EOF within [`PIPE_DRAIN_GRACE`]. `false`
    /// means a descendant still holds a pipe write handle, so the captured
    /// output below is a partial snapshot.
    pub pipes_closed: bool,
}

impl TrackedOutput {
    pub(crate) fn stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    pub(crate) fn stderr_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }

    /// A one-line description of how the wait ended, for panic messages.
    pub(crate) fn disposition(&self) -> String {
        let status = match self.status {
            Some(status) => format!("{status}"),
            None => "not reapable".to_string(),
        };
        format!(
            "timed_out={} status={status} tree_kill={} pipes_closed={}",
            self.timed_out,
            self.tree_kill.unwrap_or("n/a"),
            self.pipes_closed,
        )
    }

    /// The `std::process::Output` shape callers already assert on. Only valid
    /// where a status exists; a timed-out child has no meaningful exit code.
    pub(crate) fn into_output(self) -> std::process::Output {
        std::process::Output {
            status: self.status.expect("tracked child must have been reaped"),
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

/// A spawned child with both pipes already draining.
pub(crate) struct TrackedChild {
    child: Child,
    stdout: DrainedPipe,
    stderr: DrainedPipe,
}

/// Spawn `command` with piped stdio, both pipes draining from the first
/// instant, and (on Unix) its own process group so the deadline path can
/// signal the whole tree.
///
/// The caller keeps ownership of every other `Command` setting; only stdio and
/// the process group are imposed here.
pub(crate) fn spawn_tracked(command: &mut Command) -> std::io::Result<TrackedChild> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    soldr_platform::process::command::configure_process_group(command);
    let mut child = super::spawn_staged(command)?;
    let stdout = DrainedPipe::spawn(child.stdout.take().expect("piped stdout"));
    let stderr = DrainedPipe::spawn(child.stderr.take().expect("piped stderr"));
    Ok(TrackedChild {
        child,
        stdout,
        stderr,
    })
}

impl TrackedChild {
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Wait up to `timeout` for the child, then return regardless.
    ///
    /// This call is bounded by construction: at worst it costs
    /// `timeout + TREE_KILL_REAP_BUDGET + PIPE_DRAIN_GRACE`, whatever any
    /// descendant does. That property is the whole point -- the caller's
    /// diagnostic panic has to be reachable.
    pub(crate) fn wait_bounded(mut self, timeout: Duration) -> TrackedOutput {
        let settled = self
            .child
            .wait_timeout(timeout)
            .expect("wait for tracked child");
        if let Some(status) = settled {
            let pipes_closed = self.await_pipes(PIPE_DRAIN_GRACE);
            return TrackedOutput {
                status: Some(status),
                stdout: self.stdout.snapshot(),
                stderr: self.stderr.snapshot(),
                timed_out: false,
                tree_kill: None,
                pipes_closed,
            };
        }

        let tree_kill = match soldr_platform::process::terminate::terminate_tree(&mut self.child) {
            Ok(soldr_platform::process::terminate::TreeKill::TreeKilled) => "tree killed",
            Ok(soldr_platform::process::terminate::TreeKill::ProcessKilled) => {
                "root killed (descendants may have survived)"
            }
            Err(_) => "tree kill failed",
        };
        let status = self
            .child
            .wait_timeout(TREE_KILL_REAP_BUDGET)
            .ok()
            .flatten();
        let pipes_closed = self.await_pipes(PIPE_DRAIN_GRACE);
        TrackedOutput {
            status,
            stdout: self.stdout.snapshot(),
            stderr: self.stderr.snapshot(),
            timed_out: true,
            tree_kill: Some(tree_kill),
            pipes_closed,
        }
    }

    /// Give both drains up to `grace` to reach EOF so the snapshot below is
    /// complete when nothing survives. Returns whether both closed.
    fn await_pipes(&self, grace: Duration) -> bool {
        let deadline = Instant::now() + grace;
        loop {
            if self.stdout.finished() && self.stderr.finished() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
