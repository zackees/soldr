//! Physical-core detection (soldr#1761, final acceptance criterion).
//!
//! # Why this is not just `available_parallelism()`
//!
//! `std::thread::available_parallelism` reports *logical* CPUs. On an
//! SMT host that is double the physical core count, and sizing a
//! compile pool from it oversubscribes the machine: the reporting
//! Ryzen 3700X (8 cores / 16 threads) ran 15 concurrent rustc, which
//! saturates every hardware thread and leaves nothing for the
//! interactive session that started the build.
//!
//! [`super::jobs::default_compile_jobs`] previously deferred this,
//! noting that a blanket "assume SMT, halve it" rule would penalize a
//! genuinely non-SMT host — a 16-core machine with no SMT would be
//! capped for no reason. Reading the real topology removes the
//! guesswork, so the discount applies only where SMT actually exists.
//!
//! Returning `Option` is deliberate: every backend here can fail
//! (containers hide sysfs, an API can be unavailable), and the caller
//! must degrade to the logical-CPU behavior rather than invent a
//! number. A wrong core count is worse than no core count.

use std::sync::OnceLock;

/// Physical CPU cores on this machine, or `None` when the platform's
/// topology could not be read.
///
/// Memoized: the daemon asks once at startup, but the backends spawn a
/// subprocess (macOS) or walk sysfs (Linux), and neither cost should be
/// repeated if a caller ever moves this onto a warmer path.
pub fn physical_cores() -> Option<usize> {
    static CACHED: OnceLock<Option<usize>> = OnceLock::new();
    *CACHED.get_or_init(|| detect().filter(|cores| *cores > 0))
}

#[cfg(target_os = "linux")]
fn detect() -> Option<usize> {
    cores_from_sysfs(std::path::Path::new("/sys/devices/system/cpu"))
        .or_else(|| cores_from_cpuinfo(&std::fs::read_to_string("/proc/cpuinfo").ok()?))
}

// The two Linux parsers below are compiled everywhere, not gated on
// `target_os = "linux"`, so their tests run on every developer machine
// and every CI lane. Gating them would mean the first execution of this
// logic happened on a lane where a failure is expensive to diagnose;
// taking a `&Path` / `&str` instead of reading fixed paths costs
// nothing and makes them directly testable from a fixture.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Every hardware thread publishes the sibling set it belongs to, and
/// siblings of one physical core publish the *same* list. So the number
/// of distinct lists is the number of physical cores — no parsing of
/// the list contents required.
fn cores_from_sysfs(cpu_root: &std::path::Path) -> Option<usize> {
    use std::collections::HashSet;

    let mut sibling_sets: HashSet<String> = HashSet::new();
    for entry in std::fs::read_dir(cpu_root).ok()? {
        let path = entry.ok()?.path();
        let is_cpu_dir = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix("cpu").is_some_and(|rest| {
                    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                })
            });
        if !is_cpu_dir {
            continue;
        }
        // `thread_siblings_list` is the older name and `core_cpus_list`
        // the current one; kernels in the supported range expose one or
        // both, so try both before giving up on this CPU.
        let siblings = std::fs::read_to_string(path.join("topology/thread_siblings_list"))
            .or_else(|_| std::fs::read_to_string(path.join("topology/core_cpus_list")))
            .ok();
        if let Some(siblings) = siblings {
            sibling_sets.insert(siblings.trim().to_string());
        }
    }
    (!sibling_sets.is_empty()).then_some(sibling_sets.len())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Fallback for kernels or containers without the topology sysfs tree.
/// A physical core is identified by the `(physical id, core id)` pair;
/// `physical id` alone would collapse a multi-socket host to its socket
/// count.
fn cores_from_cpuinfo(text: &str) -> Option<usize> {
    use std::collections::HashSet;

    let mut cores: HashSet<(String, String)> = HashSet::new();
    let mut package: Option<String> = None;
    let mut core: Option<String> = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            // A blank line ends one processor block. Anything
            // incomplete is dropped rather than paired with the next
            // block's fields.
            package = None;
            core = None;
            continue;
        };
        match key.trim() {
            "physical id" => package = Some(value.trim().to_string()),
            "core id" => core = Some(value.trim().to_string()),
            _ => continue,
        }
        if let (Some(package), Some(core)) = (package.as_ref(), core.as_ref()) {
            cores.insert((package.clone(), core.clone()));
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

#[cfg(target_os = "macos")]
fn detect() -> Option<usize> {
    // `hw.physicalcpu` is the count for *this* process's allowed set,
    // which is what we want; `hw.physicalcpu_max` would ignore a
    // restricted CPU affinity.
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.physicalcpu"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// `GetLogicalProcessorInformationEx(RelationProcessorCore, ..)` returns
/// exactly one variable-length record per physical core, so counting
/// records is the answer.
#[cfg(windows)]
fn detect() -> Option<usize> {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore,
    };

    // Records are 8-byte aligned; backing the buffer with `u64` gives
    // that alignment for free, and the header fields are still read
    // unaligned so a driver reporting an odd `Size` cannot cause UB.
    const HEADER_BYTES: usize = 8; // Relationship: u32, Size: u32

    let mut len: u32 = 0;
    // The sizing call is *expected* to fail with
    // ERROR_INSUFFICIENT_BUFFER; only `len` matters here.
    unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
    };
    if (len as usize) < HEADER_BYTES {
        return None;
    }
    let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
    let ok = unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            buffer.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if ok == 0 {
        return None;
    }

    let base = buffer.as_ptr().cast::<u8>();
    let len = len as usize;
    let mut offset = 0usize;
    let mut cores = 0usize;
    while offset + HEADER_BYTES <= len {
        // SAFETY: `offset + HEADER_BYTES <= len` and `buffer` holds at
        // least `len` bytes, so both reads are in bounds. Unaligned
        // reads have no alignment requirement.
        let (relationship, size) = unsafe {
            let record = base.add(offset);
            (
                record.cast::<u32>().read_unaligned(),
                record.add(4).cast::<u32>().read_unaligned() as usize,
            )
        };
        // A zero or out-of-range `Size` would loop forever or walk off
        // the buffer. Stop and report what was counted so far.
        if size < HEADER_BYTES || offset + size > len {
            break;
        }
        if relationship == RelationProcessorCore as u32 {
            cores += 1;
        }
        offset += size;
    }
    (cores > 0).then_some(cores)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn detect() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(physical_cores_are_plausible_or_absent, {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        // Not asserting an exact number — this runs on unknown CI
        // hardware. The invariants that must hold anywhere are that a
        // reported count is positive and never exceeds the logical CPU
        // count, since every physical core carries at least one thread.
        if let Some(cores) = physical_cores() {
            assert!(cores > 0, "a reported core count must be positive");
            assert!(
                cores <= logical,
                "physical cores ({cores}) cannot exceed logical CPUs ({logical})"
            );
        }
    });

    crate::timed_test!(physical_cores_is_stable_across_calls, {
        assert_eq!(physical_cores(), physical_cores());
    });

    /// Build a fake `/sys/devices/system/cpu` for `cpu_count` hardware
    /// threads laid out `threads_per_core` to a core.
    fn fake_sysfs(cpu_count: usize, threads_per_core: usize, leaf: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        for cpu in 0..cpu_count {
            let topology = root.path().join(format!("cpu{cpu}")).join("topology");
            std::fs::create_dir_all(&topology).expect("create topology dir");
            let first = (cpu / threads_per_core) * threads_per_core;
            let siblings = (first..first + threads_per_core)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            std::fs::write(topology.join(leaf), format!("{siblings}\n")).expect("write siblings");
        }
        root
    }

    crate::timed_test!(sysfs_counts_distinct_sibling_sets, {
        // 16 threads, 2 per core -> 8 physical cores.
        let root = fake_sysfs(16, 2, "thread_siblings_list");
        assert_eq!(cores_from_sysfs(root.path()), Some(8));
    });

    crate::timed_test!(sysfs_reads_the_modern_leaf_name_too, {
        // Newer kernels expose `core_cpus_list` instead; both must work
        // or the detection silently degrades on one kernel generation.
        let root = fake_sysfs(8, 2, "core_cpus_list");
        assert_eq!(cores_from_sysfs(root.path()), Some(4));
    });

    crate::timed_test!(sysfs_handles_a_host_without_smt, {
        let root = fake_sysfs(8, 1, "thread_siblings_list");
        assert_eq!(cores_from_sysfs(root.path()), Some(8));
    });

    crate::timed_test!(sysfs_ignores_non_cpu_entries, {
        let root = fake_sysfs(4, 2, "thread_siblings_list");
        // Real sysfs has siblings like `cpufreq`, `cpuidle` and
        // `possible` next to the `cpuN` directories; counting those
        // would inflate the result.
        for noise in ["cpufreq", "cpuidle", "cpu_capacity", "power"] {
            std::fs::create_dir_all(root.path().join(noise).join("topology")).expect("noise dir");
            std::fs::write(
                root.path()
                    .join(noise)
                    .join("topology/thread_siblings_list"),
                "99\n",
            )
            .expect("noise file");
        }
        assert_eq!(cores_from_sysfs(root.path()), Some(2));
    });

    crate::timed_test!(sysfs_without_topology_reports_nothing, {
        // A container can expose `cpuN` directories with no `topology`
        // subtree. Reporting 0 would be read as "no cores"; `None` sends
        // the caller to the fallback instead.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("cpu0")).expect("cpu0");
        assert_eq!(cores_from_sysfs(root.path()), None);
    });

    crate::timed_test!(cpuinfo_pairs_package_with_core_id, {
        // Two sockets x 2 cores x 2 threads. `core id` alone would
        // report 2 by collapsing the sockets together.
        let mut text = String::new();
        for cpu in 0..8 {
            text.push_str(&format!(
                "processor\t: {cpu}\nphysical id\t: {}\ncore id\t\t: {}\n\n",
                cpu / 4,
                (cpu % 4) / 2
            ));
        }
        assert_eq!(cores_from_cpuinfo(&text), Some(4));
    });

    crate::timed_test!(cpuinfo_without_topology_fields_reports_nothing, {
        // Many ARM kernels omit `physical id` / `core id` entirely.
        let text = "processor\t: 0\nBogoMIPS\t: 50.00\n\nprocessor\t: 1\n";
        assert_eq!(cores_from_cpuinfo(text), None);
    });

    crate::timed_test!(cpuinfo_ignores_an_empty_input, {
        assert_eq!(cores_from_cpuinfo(""), None);
    });
}
