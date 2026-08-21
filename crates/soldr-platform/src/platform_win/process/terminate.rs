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
        let survivors = surviving_descendants(root);
        if survivors.is_empty() {
            return Ok(TreeKill::TreeKilled);
        }
        if std::time::Instant::now() >= deadline {
            // Out of budget with survivors still live. Say so: the whole point
            // of soldr#2605 is that this case used to report success.
            return Ok(TreeKill::ProcessKilled);
        }
        for pid in survivors.iter().rev() {
            terminate_descendant(*pid, root);
        }
        std::thread::sleep(VERIFY_POLL);
    }
}

/// How long `terminate_tree` keeps re-killing before admitting descendants
/// survived. Bounded so a wedged descendant cannot hang the caller.
const VERIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Gap between verification sweeps.
const VERIFY_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Descendants of `root` that are still alive, parents before children.
///
/// Private on purpose: the cross-platform facade exports only the four
/// termination entry points, and adding a fifth would oblige Linux and macOS
/// to grow an equivalent for a diagnostic only Windows needs today.
fn surviving_descendants(root: u32) -> Vec<u32> {
    descendants_of(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| crate::process::inspect::is_alive(*pid))
        .collect()
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
mod tests {
    use super::*;

    /// Liveness by exit code, not handle existence: a terminated
    /// process stays openable while any handle survives, so a
    /// handle-existence check would report a successful kill as failed.
    fn is_alive(pid: u32) -> bool {
        crate::process::inspect::is_alive(pid)
    }

    #[test]
    fn descendants_are_ordered_parents_before_children() {
        // 100 -> 200 -> 300, plus an unrelated 400.
        let edges = [(200, 100), (300, 200), (400, 1)];
        assert_eq!(collect_descendants(&edges, 100), vec![200, 300]);
    }

    #[test]
    fn unrelated_processes_are_never_collected() {
        let edges = [(200, 100), (400, 1), (500, 400)];
        assert_eq!(collect_descendants(&edges, 100), vec![200]);
    }

    #[test]
    fn a_leaf_root_has_no_descendants() {
        let edges = [(200, 100), (300, 200)];
        assert!(collect_descendants(&edges, 300).is_empty());
    }

    #[test]
    fn a_self_parenting_process_does_not_loop() {
        // Recycled pids can produce a self-edge; the walk must still finish.
        let edges = [(100, 100), (200, 100)];
        assert_eq!(collect_descendants(&edges, 100), vec![200]);
    }

    #[test]
    fn a_cycle_back_to_the_root_does_not_loop() {
        // 100 -> 200 -> 300 -> 100. The root is pre-seeded as seen, so the edge
        // closing the cycle is ignored rather than revisited forever.
        let edges = [(200, 100), (300, 200), (100, 300)];
        assert_eq!(collect_descendants(&edges, 100), vec![200, 300]);
    }

    /// Spawn `cmd -> ping` and return `(child, grandchild pids)` once the
    /// grandchild exists. Killing before `ping` spawns would prove nothing.
    ///
    /// `zombie_processes` cannot follow a `Child` handed back to a caller, so
    /// it reads the success path as a leak. It is not: ownership transfers,
    /// and both callers reap (one via `child.kill()` + `wait()`, the other via
    /// `terminate_tree` + `wait()`). The timeout path reaps here before it
    /// panics, so no path leaves the pair running.
    #[allow(clippy::zombie_processes)]
    fn spawn_cmd_with_ping_grandchild() -> (std::process::Child, Vec<u32>) {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 > nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd wrapper");
        let root = child.id();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = descendants_of(root).expect("snapshot");
            if !found.is_empty() {
                return (child, found);
            }
            if Instant::now() >= deadline {
                // Reap before unwinding: a panic here would otherwise leave a
                // 30s `cmd`/`ping` pair behind for every run of the suite.
                let _ = child.kill();
                let _ = child.wait();
                panic!("ping grandchild never appeared under cmd pid {root}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Pins the negative result that shaped this module.
    ///
    /// The obvious explanation for soldr#2605 is that a grandchild becomes
    /// unreachable once its parent dies, so a kill-time tree walk cannot find
    /// it. That is false on Windows, and this test is here so nobody (including
    /// a future reader of the commit that introduced it) re-derives the wrong
    /// mechanism: a process entry keeps its `th32ParentProcessID` after the
    /// parent exits, so the walk still resolves.
    ///
    /// Which is why `terminate_tree` does not rest on enumeration order alone
    /// -- it verifies. If this test ever starts failing, the walk has become
    /// order-sensitive after all and the snapshot-first ordering becomes
    /// load-bearing rather than merely tidy.
    #[test]
    fn a_dead_root_still_exposes_its_grandchildren() {
        use std::time::{Duration, Instant};

        let (mut child, grandchildren) = spawn_cmd_with_ping_grandchild();
        let root = child.id();

        // Kill ONLY the root, the way the old fallback path did.
        child.kill().expect("kill root");
        let _ = child.wait();

        let survivors: Vec<u32> = grandchildren
            .iter()
            .copied()
            .filter(|pid| is_alive(*pid))
            .collect();
        assert!(
            !survivors.is_empty(),
            "fixture invalid: the grandchild died with the root, so there is              nothing for a post-kill walk to find"
        );

        let after = descendants_of(root).expect("snapshot");
        assert!(
            after.iter().any(|pid| survivors.contains(pid)),
            "a walk rooted at the dead pid {root} lost the still-live              grandchildren {survivors:?} (walk returned {after:?}). If this              fails, kill-time enumeration IS unsound on this host and the              snapshot-first ordering is what saves us."
        );

        // Clean up the orphan the fixture deliberately created.
        for pid in survivors {
            let _ = signal_pid(pid, true);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while grandchildren.iter().any(|pid| is_alive(*pid)) {
            assert!(Instant::now() < deadline, "orphaned ping outlived cleanup");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_real_grandchild_tree_is_killed_whole() {
        // The soldr#2605 shape: soldr -> cmd.exe -> ping.exe. The old
        // `taskkill /T` walk resolved parent links only after the root was
        // already dying, so the grandchild escaped and kept the inherited
        // stderr pipe open. Assert the grandchild is gone, not just the root.
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 > nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd wrapper");
        let root = child.id();

        // Wait for the grandchild to exist -- killing before `ping` spawns
        // would pass trivially and prove nothing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let grandchildren = loop {
            let found = descendants_of(root).expect("snapshot");
            if !found.is_empty() {
                break found;
            }
            assert!(
                Instant::now() < deadline,
                "ping grandchild never appeared under cmd pid {root}"
            );
            std::thread::sleep(Duration::from_millis(50));
        };

        assert_eq!(
            terminate_tree(&mut child).expect("terminate tree"),
            TreeKill::TreeKilled
        );
        let _ = child.wait();

        // Poll: TerminateProcess is asynchronous, so a pid can linger briefly.
        // `is_alive` reads the exit code rather than merely opening a handle --
        // a terminated process stays openable while any handle survives, so a
        // handle-existence check would report the kill as failed.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let survivors: Vec<u32> = grandchildren
                .iter()
                .copied()
                .filter(|pid| is_alive(*pid))
                .collect();
            if survivors.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendants {survivors:?} of cmd pid {root} outlived the tree kill"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
