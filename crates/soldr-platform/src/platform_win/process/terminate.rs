//! Windows process termination.

use std::io;
use std::process::Child;

/// How a tree kill was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKill {
    /// The whole tree was killed (the snapshotted descendant set, then the root).
    TreeKilled,
    /// The tree could not be enumerated; the direct child was killed instead.
    ProcessKilled,
}

/// Kill `child` and every descendant, then *verify* the tree is gone.
///
/// This replaces a `taskkill /PID <root> /T /F` shell-out. The motivation is
/// soldr#2605: on the Windows target-run lanes a cancelled `soldr lint deps`
/// sibling kept running -- its `cmd.exe` died while the `ping.exe` grandchild
/// slept its full 30s, still holding the inherited stderr pipe (the `1 leaky`
/// in the same nextest summary).
///
/// The exact escape route is NOT established, and this function does not claim
/// to have found it. A tempting explanation -- that a grandchild becomes
/// unreachable once its parent dies -- was tested and is false: parent links
/// keep resolving after the root exits (see
/// `a_dead_root_still_exposes_its_grandchildren`). What is established is that
/// `taskkill` gave us no way to tell:
///
///   * it is a one-shot request with no confirmation that anything died;
///   * a non-zero exit fell back to killing only the direct child, silently;
///   * and it can name no survivor, so a recurrence produced no evidence.
///
/// So the guarantee is strengthened where it can be: enumerate in-process,
/// kill leaves-first, then re-enumerate and re-kill until the tree is empty or
/// the budget expires. A verified kill beats an unverified one regardless of
/// which mechanism let the descendant escape, and `surviving_descendants` lets
/// callers name whatever is left instead of reporting a silent success.
pub fn terminate_tree(child: &mut Child) -> io::Result<TreeKill> {
    let root = child.id();

    // Safe to read parent links now: we hold the child handle un-reaped, so
    // Windows cannot recycle `root` out from under the walk.
    let enumerated = descendants_of(root);

    if let Some(descendants) = &enumerated {
        // Leaves first. Killing a parent before its child is exactly the
        // orphaning this function exists to avoid -- and while the snapshot
        // already pins the set, bottom-up also stops a shell from spawning one
        // more child in the window before its own termination lands.
        for pid in descendants.iter().rev() {
            terminate_descendant(*pid, root);
        }
    }

    child.kill()?;

    let Some(_) = enumerated else {
        // The snapshot failed, so descendants (if any) were never named. Report
        // the weaker guarantee rather than claiming a tree kill we did not do.
        return Ok(TreeKill::ProcessKilled);
    };

    // Verification sweep. TerminateProcess is asynchronous, and a shell can
    // spawn one more child in the window between the snapshot and its own
    // termination -- so a single pass cannot prove the tree is gone. Re-kill
    // whatever is still standing until nothing is, or the budget expires.
    let deadline = std::time::Instant::now() + VERIFY_BUDGET;
    loop {
        let sweep = classify_sweep(surviving_descendants(root));
        let survivors = match sweep {
            Sweep::Clear => return Ok(TreeKill::TreeKilled),
            // The snapshot failed, so this sweep saw nothing -- which is not the
            // same as nothing being there. Retry within the budget; on expiry
            // report the weaker guarantee, exactly as the enumeration above
            // does when it cannot name the set.
            Sweep::Unknown => Vec::new(),
            Sweep::Survivors(pids) => pids,
        };
        if std::time::Instant::now() >= deadline {
            // Out of budget with survivors still live, or never able to look.
            // Say so: the whole point of soldr#2605 is that this case used to
            // report success.
            return Ok(TreeKill::ProcessKilled);
        }
        for pid in survivors.iter().rev() {
            terminate_descendant(*pid, root);
        }
        std::thread::sleep(VERIFY_POLL);
    }
}

/// What one verification sweep established.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sweep {
    /// The tree was enumerated and nothing was left alive.
    Clear,
    /// The tree was enumerated and these are still alive.
    Survivors(Vec<u32>),
    /// The tree could not be enumerated, so this sweep proves nothing.
    Unknown,
}

/// Classify a sweep, keeping "could not look" distinct from "nothing there".
///
/// soldr#2806: these used to collapse. `surviving_descendants` mapped a failed
/// `CreateToolhelp32Snapshot` to an empty vector, and an empty vector is the
/// success condition -- so a snapshot that could not be taken returned
/// `TreeKilled`, a *verified* kill that verified nothing. The initial
/// enumeration in `terminate_tree` already refuses to do this and says why; the
/// verification loop did not, which is the asymmetry rather than a judgement
/// call.
///
/// `CreateToolhelp32Snapshot` fails transiently with `ERROR_BAD_LENGTH` when
/// the process list changes underneath it -- likelier on a loaded runner, which
/// is where soldr#2806 was seen and where this host never reproduces it.
fn classify_sweep(enumerated: Option<Vec<u32>>) -> Sweep {
    match enumerated {
        None => Sweep::Unknown,
        Some(pids) if pids.is_empty() => Sweep::Clear,
        Some(pids) => Sweep::Survivors(pids),
    }
}

/// How long `terminate_tree` keeps re-killing before admitting descendants
/// survived. Bounded so a wedged descendant cannot hang the caller.
const VERIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Gap between verification sweeps.
const VERIFY_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Descendants of `root` that are still alive, parents before children.
///
/// `None` when the process list could not be snapshotted -- distinct from
/// `Some(vec![])`, which means the tree really is empty (soldr#2806).
///
/// Private on purpose: the cross-platform facade exports only the four
/// termination entry points, and adding a fifth would oblige Linux and macOS
/// to grow an equivalent for a diagnostic only Windows needs today.
fn surviving_descendants(root: u32) -> Option<Vec<u32>> {
    Some(
        descendants_of(root)?
            .into_iter()
            .filter(|pid| crate::process::inspect::is_alive(*pid))
            .collect(),
    )
}

/// Terminate one descendant, refusing pids that cannot belong to the root tree.
///
/// Between the snapshot and this call a descendant may exit and Windows may
/// hand its pid to an unrelated process. Creation time settles it: a real
/// descendant cannot have started before its ancestor, so a candidate that
/// predates `root` is a recycled pid and is left alone. `taskkill` performed no
/// such check.
fn terminate_descendant(pid: u32, root: u32) {
    if pid == root || pid == 0 {
        return;
    }
    if let (Some(child_started), Some(root_started)) = (creation_time(pid), creation_time(root)) {
        if child_started < root_started {
            return;
        }
    }
    // A missing time means the process already exited or refuses the query.
    // Terminating is then either a no-op or impossible; neither harms.
    let _ = signal_pid(pid, true);
}

/// Every transitive descendant of `root`, ordered parents before children.
///
/// `None` means the process snapshot could not be taken, which is distinct from
/// `Some(vec![])` ("the root is a leaf") -- the caller reports a weaker kill for
/// the former and a complete one for the latter.
fn descendants_of(root: u32) -> Option<Vec<u32>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    // SAFETY: TH32CS_SNAPPROCESS with pid 0 snapshots all processes; the handle
    // is closed on every path out.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }

    // (pid, parent) for every live process, read in one pass so the whole graph
    // is consistent -- walking the snapshot repeatedly would reintroduce the
    // very inconsistency this avoids.
    let mut edges: Vec<(u32, u32)> = Vec::new();
    // SAFETY: PROCESSENTRY32W is plain-old-data; the API requires it zeroed
    // with `dwSize` set before the first call.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // SAFETY: `entry` is zeroed with dwSize set as the API requires.
    // Process32FirstW is skipped deliberately: entry 0 is the idle process,
    // which is never part of a tree we spawned.
    while unsafe { Process32NextW(snapshot, &mut entry) } != 0 {
        edges.push((entry.th32ProcessID, entry.th32ParentProcessID));
    }

    // SAFETY: `snapshot` came from CreateToolhelp32Snapshot and is not used
    // after this point.
    unsafe { CloseHandle(snapshot) };

    Some(collect_descendants(&edges, root))
}

/// Breadth-first walk of `edges` from `root`, parents before children.
///
/// Split out from the snapshot so the traversal -- including its cycle and
/// self-parent guards -- is unit-testable on any host.
fn collect_descendants(edges: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut ordered = Vec::new();
    let mut frontier = vec![root];
    // `seen` carries `root` from the start so a process reporting itself as its
    // own parent, or a recycled pid that closes a cycle back to the root, cannot
    // loop forever.
    let mut seen = vec![root];

    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in edges {
            if *ppid != parent || seen.contains(pid) {
                continue;
            }
            seen.push(*pid);
            ordered.push(*pid);
            frontier.push(*pid);
        }
    }
    ordered
}

/// Process creation time as a raw FILETIME, or `None` if it cannot be read.
fn creation_time(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: QUERY_LIMITED_INFORMATION is the least-privilege right that still
    // permits GetProcessTimes; a null return is checked.
    let handle: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    // SAFETY: FILETIME is plain-old-data.
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut exited: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is open and all four out-params are owned locals.
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    // SAFETY: `handle` came from OpenProcess above and is not used after.
    unsafe { CloseHandle(handle) };

    if ok == 0 {
        return None;
    }
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Terminate a single PID (`TerminateProcess` - the Windows equivalent of
/// SIGKILL; Windows has no graceful signal to offer).
pub fn terminate_pid(pid: u32) {
    let _ = signal_pid(pid, true);
}

/// Signal a single PID. Windows ignores `force`: `TerminateProcess` is the only
/// termination primitive available.
pub fn signal_pid(pid: u32, _force: bool) -> io::Result<()> {
    use std::os::windows::raw::HANDLE;
    // Win32 API spelling - clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const PROCESS_TERMINATE: DWORD = 0x0001;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn TerminateProcess(h: HANDLE, exit_code: DWORD) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }
    // SAFETY: OpenProcess on a pid the caller has already verified; a null
    // return means the process is gone or inaccessible, which is success for a
    // terminate request. TerminateProcess is SIGKILL-equivalent.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Ok(());
    }
    unsafe {
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
    Ok(())
}

#[cfg(test)]
#[path = "terminate_tests.rs"]
mod tests;
