//! The dead-recorded-owner arm of the root-ownership diagnostic (soldr#3581).
//!
//! That arm used to hand the operator `root-owner.lock` and ask them to
//! identify its holder from the file. The file records only the (dead)
//! recorded PID, so the instruction dead-ended: nothing in the diagnostic
//! could answer "who holds it now". The issue shows what that cost — the
//! operator re-derived the answer by hand with a `/proc/*/fd` loop on every
//! failure, while the named PID stayed stale across runs and looked like one
//! stuck process rather than a moving holder.
//!
//! The arm now enumerates the live holders itself, through the neutral
//! `crate::platform::process::inspect::holders_of_file` facade, and names
//! each one: pid, executable, parentage, and whether it is clearly an orphan.
//! Hosts that cannot enumerate say so explicitly instead of implying the
//! lock file can answer the question.
//!
//! The renderer takes the scan as an argument ([`describe_with_scan`]), so
//! all three branches — holders found, none found, platform cannot
//! enumerate — are reachable from a test on every host.

use crate::core::SoldrPaths;
use crate::daemon::other_generations::{recorded_generation_owners, GenerationOwner};
use crate::platform::process::inspect::{holders_of_file, FileHolder, FileHolderScan};
use std::path::Path;

/// The line every branch ends with.
///
/// soldr#2316's `pkill` advice killed sibling daemons that were legitimately
/// serving other roots, so the "one process only" rule is unconditional here
/// — even on branches that could not name a process yet.
const CLOSING: &str =
    "soldr: terminate only that process; other daemon routes can coexist and must keep serving.";

/// Explain a busy root-owner lock whose recorded owner is already dead
/// (soldr#2316), naming the live process that actually holds it (soldr#3581).
pub(crate) fn describe_dead_owner_busy_lock(paths: &SoldrPaths, dead_pid: u32) -> String {
    let lock = crate::daemon::generation_key::generation_state_dir(paths)
        .join(super::root_ownership::ROOT_OWNER_LOCK_NAME);
    describe_with_scan(
        &paths.root,
        dead_pid,
        &lock,
        holders_of_file(&lock),
        &recorded_generation_owners(paths),
    )
}

/// Render the message for a given enumeration result.
///
/// Pure by construction: no probe runs here, which is what lets every branch
/// be pinned by a test on any host — including the two a Linux test host
/// cannot otherwise reach (`Unsupported`, and `Enumerated` with an arbitrary
/// holder shape).
///
/// `dead_pid` is the arm's precondition: the route claim it read names a
/// process that is gone. That is why a childless holder can be called an
/// orphan with a stale route claim, and why a holder some *other* generation
/// claims is the sibling-serving case worth flagging instead.
pub(crate) fn describe_with_scan(
    root: &Path,
    dead_pid: u32,
    lock: &Path,
    scan: FileHolderScan,
    generation_owners: &[GenerationOwner],
) -> String {
    let mut message = format!(
        "soldr root ownership is busy: {} -- recorded owner PID {} is dead, \
         but this route's lock {} is still held (soldr#2316).",
        root.display(),
        dead_pid,
        lock.display()
    );
    match scan {
        FileHolderScan::Unsupported => message.push_str(&cannot_enumerate_branch(lock)),
        FileHolderScan::Enumerated(holders) if holders.is_empty() => {
            message.push_str(&no_holder_branch(lock));
        }
        FileHolderScan::Enumerated(holders) => {
            message.push_str(&holders_branch(lock, &holders, generation_owners));
        }
    }
    message.push('\n');
    message.push_str(CLOSING);
    message
}

/// The branch for a host with no supported enumeration: say so, rather than
/// pointing at a lock file as if inspecting it would name its holder.
fn cannot_enumerate_branch(lock: &Path) -> String {
    format!(
        "\nsoldr: this host cannot enumerate holders of an open file, so it cannot name who holds {}.\n\
         soldr: identify the holder with a system tool that lists open file handles.",
        lock.display()
    )
}

/// The branch where enumeration ran and found nobody: the holder exited
/// between checks, or lives where this scan is not allowed to look.
fn no_holder_branch(lock: &Path) -> String {
    format!(
        "\nsoldr: enumeration found no holder of {} -- the holder exited between checks, \
         or is not visible to this scan (for example another user's process).\n\
         soldr: re-run while the lock stays busy to enumerate again.",
        lock.display()
    )
}

/// The branch that answers the question: name each holder, then say which
/// of them is safe to act on.
fn holders_branch(lock: &Path, holders: &[FileHolder], owners: &[GenerationOwner]) -> String {
    let all_childless = holders.iter().all(|holder| holder.children.is_empty());
    let self_pid = std::process::id();
    let mut text = format!("\nsoldr: live holders of {}:", lock.display());
    for holder in holders {
        text.push_str("\nsoldr:   ");
        text.push_str(&holder_line(holder, owners, all_childless, self_pid));
    }
    text.push_str(&terminate_line(holders, owners, self_pid));
    text
}

/// One holder: identity first, then the safety verdict the operator needs
/// before signalling anything.
fn holder_line(
    holder: &FileHolder,
    owners: &[GenerationOwner],
    all_childless: bool,
    self_pid: u32,
) -> String {
    let exe = holder
        .exe
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "exe unknown".to_string());
    let parent = holder
        .parent_pid
        .map(|pid| format!("parent PID {pid}"))
        .unwrap_or_else(|| "parent unknown".to_string());
    let mut line = format!("PID {} ({exe}, {parent})", holder.pid);
    if holder.pid == self_pid {
        // Naming ourselves as a recovery target would be the same defect as
        // naming a dead PID: an instruction the operator cannot carry out.
        line.push_str(", this process, not a terminate target");
    } else if let Some(generation) = recorded_generation(holder.pid, owners) {
        line.push_str(&format!(
            ", recorded owner of generation {generation}: legitimately serving, do not terminate"
        ));
    } else if all_childless {
        // The route claim is dead by this arm's precondition, so a holder
        // with no children of its own is the orphan shape the report asks
        // the message to distinguish from a sibling that is still serving.
        line.push_str(", orphaned (no children, stale route claim), safe to terminate");
    }
    line
}

/// What to actually signal: only holders no generation claims and that are
/// not this process. Never a blanket sweep.
fn terminate_line(holders: &[FileHolder], owners: &[GenerationOwner], self_pid: u32) -> String {
    let named: Vec<String> = holders
        .iter()
        .filter(|holder| {
            holder.pid != self_pid && recorded_generation(holder.pid, owners).is_none()
        })
        .map(|holder| format!("PID {}", holder.pid))
        .collect();
    if named.is_empty() {
        return "\nsoldr: every named holder is a recorded daemon of another generation or this \
                process itself, so there is no unrecorded holder here to terminate."
            .to_string();
    }
    format!("\nsoldr: terminate {} to recover.", named.join(", "))
}

/// The generation whose recorded claim names `pid`, if any.
///
/// Conservative by design: a pid match alone withholds the "safe to
/// terminate" label, because the two errors are not symmetric. Leaving an
/// orphan unnamed costs the operator one manual inspection; naming a
/// sibling daemon that is legitimately serving another route invites
/// exactly the blanket recovery soldr#2316's advice caused. A pid recycled
/// onto a stranger therefore reads as "claimed", not as "orphan".
fn recorded_generation(pid: u32, owners: &[GenerationOwner]) -> Option<&str> {
    owners
        .iter()
        .find(|owner| owner.pid == pid)
        .map(|owner| owner.generation.as_str())
}
