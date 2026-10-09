use super::*;

#[test]
fn ps_table_parser_builds_the_tree() {
    let table = parse_ps_table(
        "  10     1  2048\n  11    10  1024\n  12    11   512\n  13     1  4096\nbad line\n",
    );
    let sample = tree_from_table(10, &table).expect("root is in the table");
    assert_eq!(sample.rss_bytes, (2048 + 1024 + 512) * 1024);
    assert_eq!(sample.process_count, 3);
    assert_eq!(
        tree_from_table(99, &table).map(|sample| sample.process_count),
        None
    );
}

#[test]
fn ps_table_with_a_self_parented_root_terminates() {
    // macOS lists kernel_task as pid 0 with ppid 0.
    let table = parse_ps_table("0 0 100\n1 0 10\n");
    assert_eq!(
        tree_from_table(0, &table).map(|sample| sample.process_count),
        Some(2)
    );
}

#[test]
fn vm_stat_counts_reclaimable_pages() {
    let text = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                Pages free:                               10.\n\
                Pages active:                            999.\n\
                Pages inactive:                            20.\n\
                Pages speculative:                          3.\n\
                Pages purgeable:                            1.\n";
    assert_eq!(vm_stat_available(text), Some((10 + 20 + 3 + 1) * 16384));
    assert_eq!(vm_stat_available("garbage"), None);
}

#[test]
fn cgroup_ceiling_prepares_a_leaf_and_reports_peak_and_oom() {
    let root = tempfile::tempdir().expect("delegated root");
    std::fs::write(
        root.path().join("cgroup.subtree_control"),
        "cpu memory pids\n",
    )
    .expect("controllers");
    let ceiling = CgroupCeiling::create(root.path(), 256 * MIB, 4242).expect("leaf");
    let leaf = ceiling.path.clone();
    assert_eq!(leaf.parent(), Some(root.path()));
    assert_eq!(
        std::fs::read_to_string(leaf.join("memory.max")).expect("memory.max"),
        (256 * MIB).to_string()
    );
    assert_eq!(
        std::fs::read_to_string(leaf.join("memory.oom.group")).expect("oom.group"),
        "1"
    );
    // The kernel would populate these; the fixture plays the kernel.
    std::fs::write(leaf.join("memory.peak"), format!("{}\n", 300 * MIB)).expect("peak");
    std::fs::write(
        leaf.join("memory.events"),
        "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n",
    )
    .expect("events");
    let outcome = ceiling.outcome();
    assert_eq!(outcome.peak_bytes, Some(300 * MIB));
    assert_eq!(outcome.oom_kills, 1);
}

#[test]
fn cgroup_leaf_pins_swap_to_zero_when_swap_is_accounted() {
    // Hosted runners have swap: without this the overflow pages out unpunished.
    let leaf = tempfile::tempdir().expect("leaf");
    std::fs::write(leaf.path().join("memory.swap.max"), "max\n").expect("swap.max");
    configure_leaf(leaf.path(), 256 * MIB).expect("configure");
    assert_eq!(
        std::fs::read_to_string(leaf.path().join("memory.swap.max")).expect("swap.max"),
        "0"
    );
}

#[test]
fn cgroup_ceiling_declines_a_root_without_the_memory_controller() {
    let root = tempfile::tempdir().expect("undelegated root");
    std::fs::write(root.path().join("cgroup.subtree_control"), "cpu pids\n").expect("controllers");
    assert!(CgroupCeiling::create(root.path(), 256 * MIB, 1).is_none());
}

#[test]
fn tree_sampling_counts_descendants() {
    if !is_linux() && !is_macos() {
        return; // Windows never runs the wrapper.
    }
    let mut child = Command::new("/bin/sh")
        .args(["-c", "sleep 30 & sleep 30"])
        .spawn()
        .expect("spawn a two-process tree");
    let root = child.id();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut observed = None;
    while std::time::Instant::now() < deadline {
        observed = sample_tree(root);
        if observed
            .as_ref()
            .is_some_and(|sample| sample.process_count >= 2)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    kill_tree(root);
    let _ = child.kill();
    let _ = child.wait();
    let sample = observed.expect("the tree was sampled");
    assert!(sample.process_count >= 2, "{sample:?}");
    assert!(sample.rss_bytes > 0, "{sample:?}");
}
