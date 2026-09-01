//! Canonical host-resource evidence attached to abnormal compiler exits.

use soldr_platform::host::resources::HostResourceSnapshot;

const GIB: f64 = (1024 * 1024 * 1024) as f64;

pub(crate) fn read() -> HostResourceSnapshot {
    HostResourceSnapshot::capture()
}

/// Render the same cgroup fields used by Cargo-front-door diagnostics.
/// Unknown snapshots add nothing, which keeps non-Linux failures concise.
pub(crate) fn describe(snapshot: &HostResourceSnapshot) -> String {
    let has_cgroup_evidence = snapshot.cgroup_current_bytes.is_some()
        || snapshot.cgroup_peak_bytes.is_some()
        || snapshot.cgroup_limit_bytes.is_some()
        || snapshot.cgroup_limit_unbounded
        || snapshot.cgroup_swap_current_bytes.is_some()
        || snapshot.cgroup_swap_limit_bytes.is_some()
        || snapshot.cgroup_swap_limit_unbounded
        || snapshot.cgroup_oom_kills.is_some()
        || snapshot.cgroup_pids_current.is_some()
        || snapshot.cgroup_pids_limit.is_some()
        || snapshot.cgroup_pids_limit_unbounded;
    if !has_cgroup_evidence {
        return String::new();
    }

    let conclusion = match snapshot.cgroup_oom_kills {
        Some(0) => concat!(
            "memory.events oom_kill+oom_group_kill=0 rules out a kernel cgroup OOM kill for this signal; ",
            "the abnormal exit remains a Soldr scheduling/admission defect to diagnose"
        )
        .to_string(),
        Some(count) => format!(
            "memory.events oom_kill+oom_group_kill={count} is cumulative evidence of a possible OOM kill; any OOM is a Soldr scheduling/admission bug, never a reason to lower global build concurrency"
        ),
        None => concat!(
            "memory.events oom_kill+oom_group_kill is unreadable, so the kernel OOM cause is unknown; ",
            "the abnormal exit remains a Soldr scheduling/admission defect to diagnose"
        )
        .to_string(),
    };

    format!(
        "soldr: compiler-exit cgroup evidence: memory.current={}, memory.peak={}, memory.max={}, memory.swap.current={}, memory.swap.max={}, memory.events oom_kill+oom_group_kill={}, pids.current={}, pids.max={}; {conclusion} (soldr#3031).\n",
        format_bytes(snapshot.cgroup_current_bytes),
        format_bytes(snapshot.cgroup_peak_bytes),
        format_limit(snapshot.cgroup_limit_bytes, snapshot.cgroup_limit_unbounded),
        format_bytes(snapshot.cgroup_swap_current_bytes),
        format_limit(
            snapshot.cgroup_swap_limit_bytes,
            snapshot.cgroup_swap_limit_unbounded,
        ),
        snapshot
            .cgroup_oom_kills
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        snapshot
            .cgroup_pids_current
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        format_count_limit(
            snapshot.cgroup_pids_limit,
            snapshot.cgroup_pids_limit_unbounded,
        ),
    )
}

fn format_bytes(value: Option<u64>) -> String {
    value
        .map(|bytes| format!("{:.2} GiB", bytes as f64 / GIB))
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_limit(value: Option<u64>, unbounded: bool) -> String {
    if unbounded {
        "max".to_string()
    } else {
        format_bytes(value)
    }
}

fn format_count_limit(value: Option<u64>, unbounded: bool) -> String {
    if unbounded {
        "max".to_string()
    } else {
        value
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_evidence_has_the_full_canonical_snapshot() {
        let snapshot = HostResourceSnapshot {
            cgroup_current_bytes: Some(1 << 30),
            cgroup_peak_bytes: Some(4 << 30),
            cgroup_limit_bytes: Some(8 << 30),
            cgroup_swap_current_bytes: Some(1 << 29),
            cgroup_swap_limit_bytes: Some(2 << 30),
            cgroup_oom_kills: Some(1),
            cgroup_pids_current: Some(37),
            cgroup_pids_limit: Some(512),
            ..HostResourceSnapshot::default()
        };
        let text = describe(&snapshot);
        for field in [
            "memory.current=1.00 GiB",
            "memory.peak=4.00 GiB",
            "memory.max=8.00 GiB",
            "memory.swap.current=0.50 GiB",
            "memory.swap.max=2.00 GiB",
            "oom_kill+oom_group_kill=1",
            "pids.current=37",
            "pids.max=512",
        ] {
            assert!(text.contains(field), "missing {field}: {text}");
        }
        assert!(text.contains("Soldr scheduling/admission bug"), "{text}");
        assert!(!text.contains("lower the job"), "{text}");
    }

    #[test]
    fn zero_oom_is_decisive_without_prescribing_a_blanket_throttle() {
        let snapshot = HostResourceSnapshot {
            cgroup_oom_kills: Some(0),
            ..HostResourceSnapshot::default()
        };
        let text = describe(&snapshot);
        assert!(
            text.contains("rules out a kernel cgroup OOM kill"),
            "{text}"
        );
        assert!(text.contains("scheduling/admission defect"), "{text}");
        assert!(!text.contains("lowering"), "{text}");
    }

    #[test]
    fn wholly_unreadable_snapshot_says_nothing() {
        assert_eq!(describe(&HostResourceSnapshot::default()), "");
    }
}
