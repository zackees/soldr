use super::*;

const GIB: u64 = 1024 * MIB;

fn inputs(explicit: Option<&str>, cpus: u64, available: Option<u64>) -> AdmissionInputs {
    AdmissionInputs {
        explicit: explicit.map(str::to_owned),
        logical_cpus: cpus,
        memory: MemoryReading {
            available_bytes: available,
            source: if available.is_some() {
                MemorySource::MemAvailable
            } else {
                MemorySource::Unavailable
            },
            limit_bytes: None,
            limit_unbounded: true,
            pids_current: Some(40),
            pids_limit: None,
        },
        per_test_budget_bytes: DEFAULT_PER_TEST_MIB * MIB,
        reserve_bytes: DEFAULT_RESERVE_MIB * MIB,
        ceiling_bytes: Some(DEFAULT_CEILING_MIB * MIB),
    }
}

#[test]
fn idle_four_cpu_eight_gib_docker_runs_four_tests() {
    // soldr#2885 acceptance: 4 CPUs and 7.756 GiB visible, memory.max=max.
    let admission = resolve(&inputs(None, 4, Some(7_942 * MIB)));
    assert_eq!(admission.effective_test_threads, "4");
    assert_eq!(admission.source, "measured");
    assert_eq!(admission.memory_capacity_tests, Some(5));
    assert!(admission.warning.is_none());
}

#[test]
fn memory_not_cpus_bounds_a_short_machine() {
    assert_eq!(
        resolve(&inputs(None, 16, Some(5 * GIB))).effective_test_threads,
        "3"
    );
    // Less than the reserve still runs one test: admission never reaches zero.
    let starved = resolve(&inputs(None, 16, Some(GIB)));
    assert_eq!(starved.effective_test_threads, "1");
    assert_eq!(starved.memory_capacity_tests, Some(0));
}

#[test]
fn unobservable_memory_keeps_the_single_test_fallback() {
    let admission = resolve(&inputs(None, 32, None));
    assert_eq!(admission.effective_test_threads, "1");
    assert_eq!(admission.source, "fallback");
    assert_eq!(admission.memory_capacity_tests, None);
}

#[test]
fn explicit_values_are_frozen_verbatim_even_when_memory_disagrees() {
    for explicit in ["8", "num-cpus", "-1", "04"] {
        let admission = resolve(&inputs(Some(explicit), 8, Some(4 * GIB)));
        assert_eq!(admission.effective_test_threads, explicit);
        assert_eq!(admission.requested_test_threads.as_deref(), Some(explicit));
        assert_eq!(admission.source, "explicit");
        let warning = admission
            .warning
            .expect("2 tests fit; each value asks for more");
        assert!(warning.contains(&format!("NEXTEST_TEST_THREADS={explicit}")));
        assert!(warning.contains("at most 2"), "{warning}");
        assert!(warning.contains("unset NEXTEST_TEST_THREADS"), "{warning}");
    }
}

#[test]
fn safe_or_uninterpretable_explicit_values_do_not_warn() {
    assert!(resolve(&inputs(Some("2"), 8, Some(4 * GIB)))
        .warning
        .is_none());
    // Nextest owns validation of values it does not accept.
    assert!(resolve(&inputs(Some("lots"), 8, Some(4 * GIB)))
        .warning
        .is_none());
    // No measurement, no basis for a warning.
    assert!(resolve(&inputs(Some("64"), 8, None)).warning.is_none());
}

#[test]
fn pressure_marks_span_one_budget_above_a_one_budget_floor() {
    let admission = resolve(&inputs(None, 4, Some(8 * GIB)));
    assert_eq!(admission.pause_below_available_bytes, GIB);
    assert_eq!(admission.resume_at_available_bytes, 2 * GIB);
}

#[test]
fn a_finite_cgroup_and_a_short_vm_both_bound_available_memory() {
    // Docker Desktop: the container's memory.max is `max`; only the VM-wide
    // MemAvailable shows the neighbours' pressure.
    let unbounded = HostResourceSnapshot {
        cgroup_limit_unbounded: true,
        cgroup_current_bytes: Some(GIB),
        system_available_bytes: Some(3 * GIB),
        ..Default::default()
    };
    assert_eq!(
        tightest_available(&unbounded, None),
        (Some(3 * GIB), MemorySource::MemAvailable)
    );
    // A finite limit tighter than the host wins...
    let bounded = HostResourceSnapshot {
        cgroup_limit_bytes: Some(4 * GIB),
        cgroup_current_bytes: Some(3 * GIB),
        system_available_bytes: Some(60 * GIB),
        ..Default::default()
    };
    assert_eq!(
        tightest_available(&bounded, None),
        (Some(GIB), MemorySource::CgroupHeadroom)
    );
    // ...and a short host wins over a roomy cgroup.
    let short_host = HostResourceSnapshot {
        system_available_bytes: Some(GIB / 2),
        ..bounded
    };
    assert_eq!(
        tightest_available(&short_host, None),
        (Some(GIB / 2), MemorySource::MemAvailable)
    );
    // Hosts without procfs fall back to the OS API reading.
    assert_eq!(
        tightest_available(&HostResourceSnapshot::default(), Some(5 * GIB)),
        (Some(5 * GIB), MemorySource::OsAvailablePhysical)
    );
    assert_eq!(
        tightest_available(&HostResourceSnapshot::default(), None),
        (None, MemorySource::Unavailable)
    );
}

#[test]
fn the_wrapper_summary_names_request_effective_and_memory_source() {
    let summary = resolve(&inputs(Some("8"), 8, Some(4 * GIB))).summary();
    assert!(
        summary.contains("requested NEXTEST_TEST_THREADS=8"),
        "{summary}"
    );
    assert!(summary.contains("effective=8 (explicit)"), "{summary}");
    assert!(
        summary.contains("4.00 GiB available via MemAvailable"),
        "{summary}"
    );
    assert!(summary.contains("memory.max=max"), "{summary}");
    let summary = resolve(&inputs(None, 4, Some(8 * GIB))).summary();
    assert!(
        summary.contains("requested NEXTEST_TEST_THREADS=unset"),
        "{summary}"
    );
}

#[test]
fn the_live_probe_reports_a_source_consistent_with_its_value() {
    let reading = read_memory(None);
    assert_eq!(
        reading.available_bytes.is_some(),
        reading.source != MemorySource::Unavailable
    );
    let pinned = read_memory(Some(123));
    assert_eq!(pinned.available_bytes, Some(123 * MIB));
    assert_eq!(pinned.source, MemorySource::Override);
}
