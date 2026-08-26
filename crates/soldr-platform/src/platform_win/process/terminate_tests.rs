//! Tests for Windows process-tree termination.
//!
//! A sibling file rather than an inline `#[cfg(test)]` block: these tests
//! spawn real `cmd`/`ping` trees, and `.github/scripts/spawn_path_guard.py`
//! counts `.spawn()` per FILE without stripping cfg(test). Inline, the test
//! spawns would have to be allowlisted as if they were sanctioned production
//! spawn sites -- exactly the misrepresentation the guard exists to prevent.
//! The guard already skips `*_tests.rs` siblings for this reason.

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
///
/// **Scope, narrowed by soldr#2806 follow-up.** What this pins is that a dead
/// *root* still exposes its children: the fixture is `cmd -> ping`, so the
/// grandchild's parent is the root itself, and edges are matched by parent pid
/// rather than walked through parent nodes. It does not extend to a dead
/// *intermediate* one level further down, which really is unreachable --
/// see `the_walk_cannot_bridge_a_dead_intermediate`.
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
    // The soldr#2605 shape: soldr -> cmd.exe -> ping.exe. Assert the
    // grandchild is gone, not merely the root -- a kill that reaches only the
    // direct child is precisely the outcome that left `ping` holding the
    // inherited stderr pipe for its full sleep.
    use std::time::{Duration, Instant};

    let (mut child, grandchildren) = spawn_cmd_with_ping_grandchild();
    let root = child.id();

    // `ProcessKilled` is the truthful result when the production verification
    // budget expires with a live descendant. This test's invariant is stronger
    // and different: the descendant must eventually be gone. Keep the
    // production two-second bound strict, then give only this real-process
    // fixture time to observe asynchronous Windows reaping.
    terminate_tree(&mut child).expect("terminate tree");
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

/// A sweep that could not look must not read as a sweep that found nothing
/// (soldr#2806).
///
/// `surviving_descendants` used to map a failed `CreateToolhelp32Snapshot` to
/// an empty vector, and an empty vector is the success condition -- so a
/// snapshot that could not be taken returned `TreeKilled`: a *verified* kill
/// that verified nothing. `terminate_tree`'s initial enumeration already
/// refuses to do this and says why, so the verification loop doing the opposite
/// was an asymmetry rather than a judgement call.
///
/// Pure, because the failure it guards is a transient Windows API error this
/// host does not reproduce -- `CreateToolhelp32Snapshot` returns
/// `ERROR_BAD_LENGTH` when the process list shifts under it, which is likelier
/// on the loaded runner where soldr#2806 was seen.
#[test]
fn a_failed_snapshot_is_not_an_empty_tree() {
    assert_eq!(classify_sweep(None), Sweep::Unknown);
    assert_eq!(classify_sweep(Some(Vec::new())), Sweep::Clear);
    assert_ne!(
        classify_sweep(None),
        classify_sweep(Some(Vec::new())),
        "could-not-enumerate and nothing-survived must stay distinct: only the          second may end the verification loop with TreeKilled"
    );
}

#[test]
fn a_sweep_that_finds_survivors_names_them() {
    assert_eq!(
        classify_sweep(Some(vec![200, 300])),
        Sweep::Survivors(vec![200, 300])
    );
}

// ---- soldr#2806: the orphaned grandchild ----------------------------------

/// The gap this fix closes, and the exact boundary of the negative result
/// pinned by `a_dead_root_still_exposes_its_grandchildren` above.
///
/// That test proves the walk survives a **dead root**, and it does: entries
/// keep their `th32ParentProcessID`, and `collect_descendants` matches edges by
/// parent pid, so `root` never has to be a node for its own children to be
/// found. Its fixture is `cmd -> ping` where `ping`'s parent *is* the root, so
/// "its parent dies" and "the root dies" are the same event there.
///
/// One level deeper they are not. With `root -> A -> B`, an `A` that has exited
/// is absent from the snapshot entirely: nothing has `ppid == root`, and `B`'s
/// edge names a pid that is not a node, so there is no way to bridge it. The
/// walk returns empty while `B` runs on.
///
/// This is **not** the failure recorded in soldr#2806 -- that one was a failed
/// snapshot reported as an empty tree, fixed in #2812. It is a second way the
/// same verification can conclude "clean" over a live descendant, found while
/// checking whether #2806 was fully closed.
#[test]
fn the_walk_cannot_bridge_a_dead_intermediate() {
    // root 100 -> A 200 -> B 300, with A exited and so absent from the snapshot.
    let orphaned = vec![(300u32, 200u32)];
    assert!(
        collect_descendants(&orphaned, 100).is_empty(),
        "nothing has ppid == root, and B's parent is not a node to walk through"
    );

    // A direct child of a dead root is still found -- the case the negative
    // result above pins, kept here so the boundary is visible in one place.
    let direct = vec![(300u32, 100u32)];
    assert_eq!(collect_descendants(&direct, 100), vec![300]);

    // With the intermediate alive the same walk finds both.
    let intact = vec![(200u32, 100u32), (300u32, 200u32)];
    assert_eq!(collect_descendants(&intact, 100), vec![200, 300]);
}

#[test]
fn remembered_start_times_are_looked_up_by_pid() {
    let known = vec![(200u32, 10u64), (300u32, 42u64)];
    assert_eq!(recorded_start(&known, 200), Some(10));
    assert_eq!(recorded_start(&known, 300), Some(42));
    assert_eq!(
        recorded_start(&known, 999),
        None,
        "a pid that was never remembered carries no identity to check"
    );
}

/// The behaviour the whole change rests on: a descendant the walk can no
/// longer reach is still checked, because it is still remembered.
///
/// Written against injected probes rather than a real tree -- the shape it
/// describes (a dead intermediate, an orphan still running) is the one that
/// takes a loaded CI host to produce naturally.
#[test]
fn a_remembered_pid_the_walk_lost_is_still_reported_alive() {
    let known = vec![(300u32, 10u64)];
    let survivors = live_descendants(
        // The walk reaches nothing: the intermediate has exited.
        &[],
        &known,
        |pid| pid == 300,
        |_| Some(10),
    );
    assert_eq!(
        survivors,
        vec![300],
        "an orphan is exactly what the verification sweep must not miss"
    );
}

#[test]
fn a_remembered_pid_that_has_died_is_not_a_survivor() {
    let known = vec![(300u32, 10u64)];
    assert!(live_descendants(&[], &known, |_| false, |_| Some(10)).is_empty());
}

/// A recycled pid must not keep the sweep from reporting `Clear`, and must not
/// be handed to `terminate_descendant`.
#[test]
fn a_remembered_pid_now_held_by_a_stranger_is_not_a_survivor() {
    let known = vec![(300u32, 10u64)];
    let survivors = live_descendants(
        &[],
        &known,
        // Something is alive at that pid...
        |pid| pid == 300,
        // ...but it started later, so it is not the process we tracked.
        |_| Some(99),
    );
    assert!(
        survivors.is_empty(),
        "a stranger holding a recycled pid is not our descendant"
    );
}

#[test]
fn reachable_and_remembered_are_both_checked_without_duplication() {
    let known = vec![(300u32, 10u64)];
    let survivors = live_descendants(&[200, 300], &known, |_| true, |_| Some(10));
    assert_eq!(survivors, vec![200, 300]);
}

/// The hole an earlier draft of this change had, kept as a regression test.
///
/// That draft treated an unreadable creation time as "same process", on the
/// reasoning `terminate_descendant` already applies to a *fresh* pid. It does
/// not carry over to a remembered one: a pid we can no longer identify has, as
/// far as we can prove, been handed to somebody else -- and a wrong answer here
/// terminates that somebody. Reporting `ProcessKilled` instead of `TreeKilled`
/// is the cheaper mistake, and is the claim this module already falls back to.
#[test]
fn a_remembered_pid_whose_identity_cannot_be_read_is_not_a_survivor() {
    let known = vec![(300u32, 10u64)];
    let survivors = live_descendants(
        &[],
        &known,
        // Something is alive at that pid...
        |pid| pid == 300,
        // ...but its creation time cannot be read, so it cannot be shown to be
        // the descendant we recorded.
        |_| None,
    );
    assert!(
        survivors.is_empty(),
        "an unprovable identity must not authorise a kill"
    );
}

/// A pid the walk still reaches needs no recorded identity: the snapshot it
/// came from is itself the evidence, which is why the two sources differ.
#[test]
fn a_reachable_pid_needs_no_recorded_identity() {
    let survivors = live_descendants(&[200], &[], |_| true, |_| None);
    assert_eq!(survivors, vec![200]);
}

#[test]
fn identity_is_pid_and_creation_time_together() {
    assert!(is_same_process(10, Some(10)));
    assert!(
        !is_same_process(10, Some(11)),
        "same pid, different start: a recycled pid, not our descendant"
    );
    assert!(
        !is_same_process(10, None),
        "unreadable start proves nothing, so it cannot prove sameness"
    );
}
