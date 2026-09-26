//! Memory-aware Nextest admission for `soldr ci-test` (soldr#2885).
//!
//! `ci-test` used to freeze Nextest at one test process whenever
//! `NEXTEST_TEST_THREADS` was unset: a safe floor, but a permanent one, and
//! blind to the machine. The Bosn report behind soldr#2885 hit `ENOMEM` at
//! Nextest's host default of four threads on a 4-CPU / 8 GiB Docker Desktop
//! VM whose container reported `memory.max=max` -- while other containers in
//! the same VM held the memory. So the admission is now *measured*:
//!
//! * **Initial concurrency** (this module, at plan time): the smaller of the
//!   logical CPU count and how many per-test budgets fit in the memory that is
//!   available right now after a fixed reserve. Available memory is the
//!   *tighter* of host `MemAvailable` and finite cgroup headroom -- a
//!   `memory.max` of `max` never means unlimited, and a finite cgroup limit
//!   never hides a VM that is short. When memory cannot be observed at all,
//!   the historical one-test fallback is kept.
//! * **Run-time pressure** (`test_pressure`): hysteresis marks derived from
//!   the same budget pause and resume new test admissions while Nextest runs.
//! * **Per-test ceiling**: frozen here, enforced by the Nextest wrapper
//!   (`.github/scripts/nextest_memory_guard.py`) on Unix.
//!
//! An explicit `NEXTEST_TEST_THREADS` stays authoritative and is frozen
//! verbatim. When the measurement says it cannot fit, planning warns (stderr
//! and the `--explain-plan` JSON) instead of rewriting the operator's value.
//!
//! None of this touches `CARGO_BUILD_JOBS` or `SOLDR_JOBS`: test processes are
//! outside compiler admission, and compiler work keeps the daemon's canonical
//! shared/exclusive gate.

use crate::platform::host::resources::HostResourceSnapshot;
use serde::Serialize;

pub(crate) const NEXTEST_TEST_THREADS_ENV: &str = "NEXTEST_TEST_THREADS";
/// Replaces the probed available memory, in MiB -- for a container that can
/// only see its own cgroup while the VM around it is short, and for tests.
pub(crate) const MEMORY_AVAILABLE_MIB_ENV: &str = "SOLDR_CI_TEST_MEMORY_AVAILABLE_MIB";
/// Replaces the logical CPU count used for admission.
pub(crate) const LOGICAL_CPUS_ENV: &str = "SOLDR_CI_TEST_LOGICAL_CPUS";
/// Memory one ordinary test process tree is budgeted, in MiB.
pub(crate) const PER_TEST_MEMORY_MIB_ENV: &str = "SOLDR_CI_TEST_PER_TEST_MEMORY_MIB";
/// Memory held back for the daemon, the overlapping Dylint branch, and the OS.
pub(crate) const MEMORY_RESERVE_MIB_ENV: &str = "SOLDR_CI_TEST_MEMORY_RESERVE_MIB";
/// Per-test process-tree ceiling, in MiB; `0` disables it.
pub(crate) const TEST_MEMORY_CEILING_MIB_ENV: &str = "SOLDR_CI_TEST_TEST_MEMORY_CEILING_MIB";

const MIB: u64 = 1024 * 1024;
/// The #2885 cook container peaked near 3.08 GiB with four tests resident,
/// about 0.77 GiB each; 1 GiB per test keeps headroom above that.
const DEFAULT_PER_TEST_MIB: u64 = 1024;
/// Nextest execution overlaps the Dylint UI-test compiles and a live daemon.
const DEFAULT_RESERVE_MIB: u64 = 2048;
/// Far above any ordinary test tree; low enough that one runaway test fails
/// alone instead of exhausting a 16 GiB runner.
const DEFAULT_CEILING_MIB: u64 = 4096;
/// Used only when memory cannot be observed at all.
const FALLBACK_TEST_THREADS: u64 = 1;

/// Where the available-memory figure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemorySource {
    Override,
    CgroupHeadroom,
    MemAvailable,
    OsAvailablePhysical,
    Unavailable,
}

impl MemorySource {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Override => MEMORY_AVAILABLE_MIB_ENV,
            Self::CgroupHeadroom => "cgroup headroom (memory.max - memory.current)",
            Self::MemAvailable => "MemAvailable",
            Self::OsAvailablePhysical => "OS available physical memory",
            Self::Unavailable => "unavailable",
        }
    }
}

/// One available-memory reading, with the cgroup context that shaped it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MemoryReading {
    pub(crate) available_bytes: Option<u64>,
    pub(crate) source: MemorySource,
    pub(crate) limit_bytes: Option<u64>,
    pub(crate) limit_unbounded: bool,
    pub(crate) pids_current: Option<u64>,
    pub(crate) pids_limit: Option<u64>,
}

/// The tighter of finite cgroup headroom and host-available memory.
///
/// Deliberately not the daemon's "cgroup headroom, else `MemAvailable`"
/// preference (`history_admission::available_bytes`): test processes are not
/// charged to a compiler, and the soldr#2885 failure was a VM that ran short
/// while the container's own cgroup had room.
pub(crate) fn tightest_available(
    snapshot: &HostResourceSnapshot,
    os_available: Option<u64>,
) -> (Option<u64>, MemorySource) {
    let host = snapshot
        .system_available_bytes
        .map(|bytes| (bytes, MemorySource::MemAvailable))
        .or_else(|| os_available.map(|bytes| (bytes, MemorySource::OsAvailablePhysical)));
    let cgroup = snapshot
        .cgroup_headroom()
        .map(|bytes| (bytes, MemorySource::CgroupHeadroom));
    match (host, cgroup) {
        (Some(host), Some(cgroup)) => {
            let tighter = if cgroup.0 < host.0 { cgroup } else { host };
            (Some(tighter.0), tighter.1)
        }
        (Some((bytes, source)), None) | (None, Some((bytes, source))) => (Some(bytes), source),
        (None, None) => (None, MemorySource::Unavailable),
    }
}

/// Probe this host now. `override_mib` replaces only the available figure.
pub(crate) fn read_memory(override_mib: Option<u64>) -> MemoryReading {
    let snapshot = HostResourceSnapshot::capture();
    let (available_bytes, source) = match override_mib {
        Some(mib) => (Some(mib.saturating_mul(MIB)), MemorySource::Override),
        None => tightest_available(
            &snapshot,
            crate::platform::host::resources::available_physical_memory_bytes(),
        ),
    };
    MemoryReading {
        available_bytes,
        source,
        limit_bytes: snapshot.cgroup_limit_bytes,
        limit_unbounded: snapshot.cgroup_limit_unbounded,
        pids_current: snapshot.cgroup_pids_current,
        pids_limit: snapshot.cgroup_pids_limit,
    }
}

/// Everything the admission decision depends on, captured once.
#[derive(Clone, Debug)]
pub(crate) struct AdmissionInputs {
    pub(crate) explicit: Option<String>,
    pub(crate) logical_cpus: u64,
    pub(crate) memory: MemoryReading,
    pub(crate) per_test_budget_bytes: u64,
    pub(crate) reserve_bytes: u64,
    pub(crate) ceiling_bytes: Option<u64>,
}

impl AdmissionInputs {
    pub(crate) fn from_process() -> Self {
        let logical_cpus = env_mib_or_count(LOGICAL_CPUS_ENV).unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(1, |value| value.get() as u64)
        });
        let ceiling_mib = env_u64(TEST_MEMORY_CEILING_MIB_ENV).unwrap_or(DEFAULT_CEILING_MIB);
        Self {
            // An empty value is "unset" to Nextest too; anything else is the
            // operator's and is frozen verbatim.
            explicit: std::env::var(NEXTEST_TEST_THREADS_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
            logical_cpus: logical_cpus.max(1),
            memory: read_memory(env_mib_or_count(MEMORY_AVAILABLE_MIB_ENV)),
            per_test_budget_bytes: env_mib_or_count(PER_TEST_MEMORY_MIB_ENV)
                .unwrap_or(DEFAULT_PER_TEST_MIB)
                .saturating_mul(MIB),
            reserve_bytes: env_u64(MEMORY_RESERVE_MIB_ENV)
                .unwrap_or(DEFAULT_RESERVE_MIB)
                .saturating_mul(MIB),
            ceiling_bytes: (ceiling_mib > 0).then(|| ceiling_mib.saturating_mul(MIB)),
        }
    }
}

/// A non-negative integer knob; malformed values keep the default (and say so).
fn env_u64(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(value) => Some(value),
        Err(_) => {
            eprintln!("warning: soldr ci-test: ignoring {name}={raw:?}; expected a whole number");
            None
        }
    }
}

/// Like [`env_u64`], but zero is meaningless (no CPUs, no memory, no budget).
fn env_mib_or_count(name: &str) -> Option<u64> {
    env_u64(name).filter(|value| *value > 0)
}

/// The frozen admission decision, rendered verbatim by `--explain-plan`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TestAdmission {
    /// The explicit `NEXTEST_TEST_THREADS`, or null when ci-test chose.
    pub(crate) requested_test_threads: Option<String>,
    /// The value handed to `--test-threads`.
    pub(crate) effective_test_threads: String,
    /// `explicit`, `measured`, or `fallback` (memory unobservable).
    pub(crate) source: &'static str,
    pub(crate) logical_cpus: u64,
    pub(crate) memory_source: &'static str,
    pub(crate) memory_available_bytes: Option<u64>,
    pub(crate) memory_limit_bytes: Option<u64>,
    pub(crate) memory_limit_unbounded: bool,
    pub(crate) pids_current: Option<u64>,
    pub(crate) pids_limit: Option<u64>,
    pub(crate) per_test_budget_bytes: u64,
    pub(crate) reserve_bytes: u64,
    /// How many per-test budgets fit after the reserve; null when unobservable.
    pub(crate) memory_capacity_tests: Option<u64>,
    /// Run-time controller marks: pause new tests below the first, resume at
    /// or above the second. The band is one per-test budget wide.
    pub(crate) pause_below_available_bytes: u64,
    pub(crate) resume_at_available_bytes: u64,
    /// Per-test process-tree ceiling; null when disabled.
    pub(crate) per_test_ceiling_bytes: Option<u64>,
    pub(crate) warning: Option<String>,
}

impl TestAdmission {
    /// One human line the wrapper quotes in every memory diagnostic.
    pub(crate) fn summary(&self) -> String {
        format!(
            "requested NEXTEST_TEST_THREADS={}, effective={} ({}); {} logical CPUs; \
             {} available via {} at plan time, memory.max={}; per-test budget {}, reserve {}",
            self.requested_test_threads.as_deref().unwrap_or("unset"),
            self.effective_test_threads,
            self.source,
            self.logical_cpus,
            format_bytes(self.memory_available_bytes),
            self.memory_source,
            if self.memory_limit_unbounded {
                "max".to_string()
            } else {
                format_bytes(self.memory_limit_bytes)
            },
            format_bytes(Some(self.per_test_budget_bytes)),
            format_bytes(Some(self.reserve_bytes)),
        )
    }
}

pub(crate) fn format_bytes(value: Option<u64>) -> String {
    const GIB: u64 = 1024 * MIB;
    match value {
        None => "unknown".into(),
        Some(bytes) if bytes >= GIB => format!("{:.2} GiB", bytes as f64 / GIB as f64),
        Some(bytes) => format!("{:.1} MiB", bytes as f64 / MIB as f64),
    }
}

/// Resolve the effective test concurrency. Pure: every input is captured.
pub(crate) fn resolve(inputs: &AdmissionInputs) -> TestAdmission {
    let budget = inputs.per_test_budget_bytes.max(1);
    let capacity = inputs
        .memory
        .available_bytes
        .map(|available| available.saturating_sub(inputs.reserve_bytes) / budget);
    let (effective, source, warning) = match &inputs.explicit {
        Some(explicit) => (
            explicit.clone(),
            "explicit",
            capacity.and_then(|capacity| unsafe_explicit_warning(explicit, inputs, capacity)),
        ),
        None => match capacity {
            Some(capacity) => (
                capacity.clamp(1, inputs.logical_cpus).to_string(),
                "measured",
                None,
            ),
            None => (FALLBACK_TEST_THREADS.to_string(), "fallback", None),
        },
    };
    TestAdmission {
        requested_test_threads: inputs.explicit.clone(),
        effective_test_threads: effective,
        source,
        logical_cpus: inputs.logical_cpus,
        memory_source: inputs.memory.source.describe(),
        memory_available_bytes: inputs.memory.available_bytes,
        memory_limit_bytes: inputs.memory.limit_bytes,
        memory_limit_unbounded: inputs.memory.limit_unbounded,
        pids_current: inputs.memory.pids_current,
        pids_limit: inputs.memory.pids_limit,
        per_test_budget_bytes: budget,
        reserve_bytes: inputs.reserve_bytes,
        memory_capacity_tests: capacity,
        pause_below_available_bytes: budget,
        resume_at_available_bytes: budget.saturating_mul(2),
        per_test_ceiling_bytes: inputs.ceiling_bytes,
        warning,
    }
}

/// Nextest's own spellings: a positive count, `num-cpus`, or `-N` (CPUs - N).
fn explicit_count(explicit: &str, logical_cpus: u64) -> Option<u64> {
    let value = explicit.trim();
    if value == "num-cpus" {
        return Some(logical_cpus);
    }
    let parsed: i64 = value.parse().ok()?;
    if parsed > 0 {
        return Some(parsed as u64);
    }
    (parsed < 0).then(|| logical_cpus.saturating_sub(parsed.unsigned_abs()).max(1))
}

fn unsafe_explicit_warning(
    explicit: &str,
    inputs: &AdmissionInputs,
    capacity: u64,
) -> Option<String> {
    let requested = explicit_count(explicit, inputs.logical_cpus)?;
    let safe = capacity.max(1);
    (requested > safe).then(|| {
        format!(
            "soldr ci-test: NEXTEST_TEST_THREADS={explicit} asks for {requested} concurrent tests, \
             but measured memory holds at most {safe}: {} available via {}, minus a {} reserve, \
             at {} per test. Keeping the explicit value; unset NEXTEST_TEST_THREADS to let ci-test \
             choose, or set it to {safe} or fewer (soldr#2885)",
            format_bytes(inputs.memory.available_bytes),
            inputs.memory.source.describe(),
            format_bytes(Some(inputs.reserve_bytes)),
            format_bytes(Some(inputs.per_test_budget_bytes)),
        )
    })
}

#[cfg(test)]
#[path = "test_admission_tests.rs"]
mod tests;
