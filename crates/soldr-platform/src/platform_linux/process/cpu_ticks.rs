//! Linux exposes CPU ticks for a process and its descendants without adding
//! a heavyweight process-inspection dependency. Other platforms still use
//! live output, which rustup and cargo emit throughout normal work.

/// Sum of user+system ticks for `root_pid` and every descendant, read from
/// `/proc`. `None` when the root process has already vanished.
pub fn process_tree_cpu_ticks(root_pid: u32) -> Option<u64> {
    #[derive(Clone, Copy)]
    struct ProcessTicks {
        pid: u32,
        parent_pid: u32,
        ticks: u64,
    }

    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            // A process can vanish while `/proc` is being enumerated.
            continue;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = fields.split_ascii_whitespace().collect();
        // Fields after `comm`: state=0, ppid=1, ... utime=11, stime=12.
        let (Some(parent_pid), Some(user_ticks), Some(system_ticks)) = (
            fields.get(1).and_then(|field| field.parse().ok()),
            fields.get(11).and_then(|field| field.parse::<u64>().ok()),
            fields.get(12).and_then(|field| field.parse::<u64>().ok()),
        ) else {
            continue;
        };
        processes.push(ProcessTicks {
            pid,
            parent_pid,
            ticks: user_ticks.saturating_add(system_ticks),
        });
    }

    let mut pending = vec![root_pid];
    let mut total = 0_u64;
    while let Some(pid) = pending.pop() {
        let Some(process) = processes.iter().find(|process| process.pid == pid) else {
            continue;
        };
        total = total.saturating_add(process.ticks);
        pending.extend(
            processes
                .iter()
                .filter(|process| process.parent_pid == pid)
                .map(|process| process.pid),
        );
    }
    Some(total)
}
