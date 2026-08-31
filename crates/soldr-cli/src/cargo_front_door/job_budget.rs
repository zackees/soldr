//! Memory-aware Cargo orchestration budget (soldr#2878).
//!
//! The embedded compiler gate cannot protect work Cargo performs before a
//! rustc wrapper is invoked: fingerprinting, directory walks, build-script
//! planning, and the jobserver itself all consume host resources first.  The
//! original failure happened at `CARGO_BUILD_JOBS=8` in an 8 GiB container;
//! the same cold graph completed at two jobs.  This module turns that measured
//! boundary into a *memory* budget rather than baking the number two into the
//! front door, so a larger host can retain more parallelism.

use std::path::Path;

const GIB: u64 = 1024 * 1024 * 1024;

/// Leave room for Cargo itself, the embedded daemon, the kernel, and unrelated
/// runner services before assigning memory to jobserver slots.
const HOST_HEADROOM_BYTES: u64 = GIB;

/// Conservative per-slot allowance derived from the soldr#2878 cold-graph
/// measurement: seven GiB of cgroup headroom safely carried two jobs, while
/// eight jobs failed before compiler admission.  Three GiB per slot plus the
/// fixed one-GiB host reserve reproduces that safe boundary and scales to four
/// jobs on a 16 GiB hosted runner.  It is deliberately a byte budget, not a
/// fixed job cap.
const BYTES_PER_CARGO_JOB: u64 = 3 * GIB;

const PROC_MEMINFO: &str = "/proc/meminfo";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CargoMemoryTelemetry {
    pub(super) cgroup_current_bytes: Option<u64>,
    pub(super) cgroup_peak_bytes: Option<u64>,
    pub(super) cgroup_limit_bytes: Option<u64>,
    pub(super) cgroup_limit_unbounded: bool,
    pub(super) cgroup_swap_current_bytes: Option<u64>,
    pub(super) cgroup_swap_limit_bytes: Option<u64>,
    pub(super) cgroup_swap_limit_unbounded: bool,
    pub(super) cgroup_oom_kills: Option<u64>,
    pub(super) cgroup_pids_current: Option<u64>,
    pub(super) cgroup_pids_limit: Option<u64>,
    pub(super) cgroup_pids_limit_unbounded: bool,
    pub(super) system_available_bytes: Option<u64>,
}

impl CargoMemoryTelemetry {
    pub(super) fn capture() -> Self {
        let meminfo = Path::new(PROC_MEMINFO);
        soldr_platform::host::resources::cgroup_v2_dir().map_or_else(
            || Self {
                system_available_bytes: read_text(meminfo).as_deref().and_then(mem_available),
                ..Self::default()
            },
            |cgroup_dir| Self::read_at(&cgroup_dir, meminfo),
        )
    }

    fn read_at(cgroup_dir: &Path, meminfo_path: &Path) -> Self {
        let events = read_text(cgroup_dir.join("memory.events"));
        let (cgroup_limit_bytes, cgroup_limit_unbounded) =
            read_limit(cgroup_dir.join("memory.max"));
        let (cgroup_swap_limit_bytes, cgroup_swap_limit_unbounded) =
            read_limit(cgroup_dir.join("memory.swap.max"));
        let (cgroup_pids_limit, cgroup_pids_limit_unbounded) =
            read_limit(cgroup_dir.join("pids.max"));
        Self {
            cgroup_current_bytes: read_u64(cgroup_dir.join("memory.current")),
            cgroup_peak_bytes: read_u64(cgroup_dir.join("memory.peak")),
            cgroup_limit_bytes,
            cgroup_limit_unbounded,
            cgroup_swap_current_bytes: read_u64(cgroup_dir.join("memory.swap.current")),
            cgroup_swap_limit_bytes,
            cgroup_swap_limit_unbounded,
            cgroup_oom_kills: events.as_deref().and_then(total_oom_kills),
            cgroup_pids_current: read_u64(cgroup_dir.join("pids.current")),
            cgroup_pids_limit,
            cgroup_pids_limit_unbounded,
            system_available_bytes: read_text(meminfo_path).as_deref().and_then(mem_available),
        }
    }

    fn cgroup_headroom(&self) -> Option<u64> {
        Some(
            self.cgroup_limit_bytes?
                .saturating_sub(self.cgroup_current_bytes?),
        )
    }

    fn available_for_budget(&self) -> Option<(u64, CargoJobBudgetSource)> {
        self.cgroup_headroom()
            .map(|bytes| (bytes, CargoJobBudgetSource::CgroupHeadroom))
            .or_else(|| {
                self.system_available_bytes
                    .map(|bytes| (bytes, CargoJobBudgetSource::SystemAvailable))
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CargoJobBudgetSource {
    ExplicitEnvironment,
    ExplicitArgument,
    CgroupHeadroom,
    SystemAvailable,
    CpuOnly,
}

impl CargoJobBudgetSource {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::ExplicitEnvironment => "explicit CARGO_BUILD_JOBS",
            Self::ExplicitArgument => "explicit Cargo jobs argument",
            Self::CgroupHeadroom => "finite cgroup memory headroom",
            Self::SystemAvailable => "MemAvailable fallback",
            Self::CpuOnly => "CPU-only fallback (memory telemetry unavailable)",
        }
    }
}

/// One parent-level decision for several Cargo processes that would run at
/// the same time.
///
/// Each Cargo invocation owns an independent jobserver.  Therefore stamping
/// `CARGO_BUILD_JOBS=2` on two children exposes four orchestration slots before
/// either child reaches the daemon's compiler semaphore.  `soldr ci-test`
/// uses this value to decide whether its two compiler-bearing branches may
/// overlap; it never rewrites an explicit per-process value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SharedCargoProcessBudget {
    pub(crate) requested_slots: Option<usize>,
    pub(crate) available_slots: usize,
    pub(crate) source_description: &'static str,
    pub(crate) may_overlap: bool,
}

/// Resolve the live aggregate budget for `processes` independent Cargo
/// jobservers, each configured with `per_process_jobs`.
pub(crate) fn shared_cargo_process_budget(
    per_process_jobs: &str,
    processes: usize,
) -> SharedCargoProcessBudget {
    let telemetry = CargoMemoryTelemetry::capture();
    let logical_jobs = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    shared_cargo_process_budget_from(per_process_jobs, processes, logical_jobs, &telemetry)
}

fn shared_cargo_process_budget_from(
    per_process_jobs: &str,
    processes: usize,
    logical_jobs: usize,
    telemetry: &CargoMemoryTelemetry,
) -> SharedCargoProcessBudget {
    let capacity = resolve_cargo_job_budget_from(None, None, logical_jobs, telemetry);
    let requested_slots =
        parse_positive(per_process_jobs).and_then(|jobs| jobs.checked_mul(processes.max(1)));
    SharedCargoProcessBudget {
        requested_slots,
        available_slots: capacity.effective_jobs,
        source_description: capacity.source.describe(),
        may_overlap: requested_slots.is_some_and(|slots| slots <= capacity.effective_jobs),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CargoJobBudget {
    pub(super) requested_jobs: usize,
    pub(super) effective_jobs: usize,
    pub(super) source: CargoJobBudgetSource,
}

impl CargoJobBudget {
    fn is_explicit(&self) -> bool {
        matches!(
            self.source,
            CargoJobBudgetSource::ExplicitEnvironment | CargoJobBudgetSource::ExplicitArgument
        )
    }

    fn decision(&self, telemetry: &CargoMemoryTelemetry) -> String {
        format!(
            "soldr: Cargo orchestration budget: requested jobs={}, effective jobs={} ({}, fixed reserve={}, per-job reserve={}, memory.current={}, memory.peak={}, memory.max={}, MemAvailable={})",
            self.requested_jobs,
            self.effective_jobs,
            self.source.describe(),
            format_bytes(Some(HOST_HEADROOM_BYTES)),
            format_bytes(Some(BYTES_PER_CARGO_JOB)),
            format_bytes(telemetry.cgroup_current_bytes),
            format_bytes(telemetry.cgroup_peak_bytes),
            format_limit(
                telemetry.cgroup_limit_bytes,
                telemetry.cgroup_limit_unbounded,
            ),
            format_bytes(telemetry.system_available_bytes),
        )
    }
}

/// Captured once immediately before Cargo starts.  Keeping the policy and the
/// inputs together makes a later failure diagnostic explain the exact decision
/// applied to this invocation rather than re-resolving against changed state.
#[derive(Debug, Clone)]
pub(super) struct AppliedCargoJobBudget {
    pub(super) budget: CargoJobBudget,
    pub(super) before: CargoMemoryTelemetry,
}

impl AppliedCargoJobBudget {
    pub(super) fn diagnose_failure(&self, captured_stderr: &str) -> Option<String> {
        let after = CargoMemoryTelemetry::capture();
        precompiler_exhaustion_diagnostic_from(captured_stderr, &self.budget, &self.before, &after)
    }
}

/// Resolve and apply the automatic budget.  Explicit user configuration is
/// observed but never rewritten: Cargo remains responsible for validating it.
pub(super) fn apply(args: &[String], command: &mut std::process::Command) -> AppliedCargoJobBudget {
    let before = CargoMemoryTelemetry::capture();
    let logical_jobs = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    let explicit_env = std::env::var("CARGO_BUILD_JOBS").ok();
    let explicit_arg = explicit_jobs_argument(args);
    let budget = resolve_cargo_job_budget_from(
        explicit_env.as_deref(),
        explicit_arg.flatten(),
        logical_jobs,
        &before,
    );

    // Presence matters independently of parsing.  An invalid explicit value
    // must still reach Cargo and produce Cargo's own stable error rather than
    // being silently replaced by an automatic value.
    let has_explicit_env = explicit_env.is_some();
    let has_explicit_arg = explicit_arg.is_some();
    if !has_explicit_env && !has_explicit_arg {
        command.env("CARGO_BUILD_JOBS", budget.effective_jobs.to_string());
    }

    // Normal unconstrained builds stay quiet.  A cap is user-visible because
    // it changes Cargo's default, while --debug provides the full decision on
    // any host for performance investigations.
    if budget.effective_jobs < budget.requested_jobs || super::debug_trace::enabled() {
        eprintln!("{}", budget.decision(&before));
    }

    AppliedCargoJobBudget { budget, before }
}

pub(super) fn resolve_cargo_job_budget_from(
    explicit_env: Option<&str>,
    explicit_arg: Option<usize>,
    logical_jobs: usize,
    telemetry: &CargoMemoryTelemetry,
) -> CargoJobBudget {
    let logical_jobs = logical_jobs.max(1);
    if let Some(raw) = explicit_env {
        let jobs = parse_positive(raw).unwrap_or(logical_jobs);
        return CargoJobBudget {
            requested_jobs: jobs,
            effective_jobs: jobs,
            source: CargoJobBudgetSource::ExplicitEnvironment,
        };
    }
    if let Some(jobs) = explicit_arg.filter(|jobs| *jobs > 0) {
        return CargoJobBudget {
            requested_jobs: jobs,
            effective_jobs: jobs,
            source: CargoJobBudgetSource::ExplicitArgument,
        };
    }

    let Some((available_bytes, source)) = telemetry.available_for_budget() else {
        return CargoJobBudget {
            requested_jobs: logical_jobs,
            effective_jobs: logical_jobs,
            source: CargoJobBudgetSource::CpuOnly,
        };
    };
    let memory_jobs = available_bytes
        .saturating_sub(HOST_HEADROOM_BYTES)
        .checked_div(BYTES_PER_CARGO_JOB)
        .unwrap_or(0)
        .max(1) as usize;
    CargoJobBudget {
        requested_jobs: logical_jobs,
        effective_jobs: logical_jobs.min(memory_jobs).max(1),
        source,
    }
}

/// Return `Some(Some(n))` for a valid explicit Cargo jobs argument,
/// `Some(None)` when a jobs flag was present but malformed (Cargo will report
/// it), and `None` when Cargo should use the automatic budget.
fn explicit_jobs_argument(args: &[String]) -> Option<Option<usize>> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            break;
        }
        if arg == "-j" || arg == "--jobs" {
            return Some(args.get(index + 1).and_then(|value| parse_positive(value)));
        }
        if let Some(value) = arg.strip_prefix("--jobs=") {
            return Some(parse_positive(value));
        }
        if let Some(value) = arg.strip_prefix("-j") {
            if !value.is_empty() {
                return Some(parse_positive(value));
            }
        }
        index += 1;
    }
    None
}

pub(super) fn precompiler_exhaustion_diagnostic(
    stderr: &str,
    budget: &CargoJobBudget,
    telemetry: &CargoMemoryTelemetry,
) -> Option<String> {
    precompiler_exhaustion_diagnostic_from(stderr, budget, telemetry, telemetry)
}

fn precompiler_exhaustion_diagnostic_from(
    stderr: &str,
    budget: &CargoJobBudget,
    before: &CargoMemoryTelemetry,
    after: &CargoMemoryTelemetry,
) -> Option<String> {
    let lower = stderr.to_ascii_lowercase();
    let is_enomem = lower.contains("cannot allocate memory") || lower.contains("os error 12");
    let is_cargo_orchestration = [
        "failed to determine package fingerprint",
        "failed to determine list of files",
        "could not obtain directory entry",
        "failed to read directory",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if !is_enomem || !is_cargo_orchestration {
        return None;
    }

    let override_note = if budget.is_explicit() {
        "The explicit override was retained; lower CARGO_BUILD_JOBS or Cargo -j for this host."
    } else {
        "The automatic budget was applied; set CARGO_BUILD_JOBS to a lower value if other workloads share this cgroup."
    };
    let oom_kills = after
        .cgroup_oom_kills
        .map(|value| {
            let delta = before
                .cgroup_oom_kills
                .map(|start| value.saturating_sub(start));
            delta.map_or_else(
                || value.to_string(),
                |delta| format!("{value} (build delta={delta})"),
            )
        })
        .unwrap_or_else(|| "unknown".to_string());
    Some(format!(
        "soldr: Cargo orchestration exhausted host resources before compiler admission; this failure occurred during Cargo fingerprint/directory scanning, so rustc/zccache exclusive admission could not protect it.\n{}\n\
         soldr: post-failure cgroup evidence: memory.current={}, memory.peak={}, memory.max={}, memory.swap.current={}, memory.swap.max={}, memory.events oom_kill={}, pids.current={}, pids.max={}.\n\
         soldr: {override_note}",
        budget.decision(before),
        format_bytes(after.cgroup_current_bytes),
        format_bytes(after.cgroup_peak_bytes),
        format_limit(after.cgroup_limit_bytes, after.cgroup_limit_unbounded),
        format_bytes(after.cgroup_swap_current_bytes),
        format_limit(
            after.cgroup_swap_limit_bytes,
            after.cgroup_swap_limit_unbounded,
        ),
        oom_kills,
        after
            .cgroup_pids_current
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        format_count_limit(
            after.cgroup_pids_limit,
            after.cgroup_pids_limit_unbounded,
        ),
    ))
}

fn read_text(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_text(path)?.trim().parse().ok()
}

fn read_limit(path: impl AsRef<Path>) -> (Option<u64>, bool) {
    let Some(raw) = read_text(path) else {
        return (None, false);
    };
    let raw = raw.trim();
    if raw == "max" {
        (None, true)
    } else {
        (raw.parse().ok(), false)
    }
}

fn parse_positive(raw: &str) -> Option<usize> {
    raw.trim().parse().ok().filter(|value| *value > 0)
}

fn mem_available(raw: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let value = line.strip_prefix("MemAvailable:")?;
        let kib = value.split_whitespace().next()?.parse::<u64>().ok()?;
        kib.checked_mul(1024)
    })
}

fn total_oom_kills(raw: &str) -> Option<u64> {
    let mut total = None;
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name != "oom_kill" && name != "oom_group_kill" {
            continue;
        }
        if let Ok(value) = value.parse::<u64>() {
            total = Some(total.unwrap_or(0u64).saturating_add(value));
        }
    }
    total
}

fn format_bytes(bytes: Option<u64>) -> String {
    bytes
        .map(|bytes| format!("{:.2} GiB", bytes as f64 / GIB as f64))
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_limit(bytes: Option<u64>, unbounded: bool) -> String {
    if unbounded {
        "max".to_string()
    } else {
        format_bytes(bytes)
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

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn constrained_cgroup_caps_automatic_fanout_but_keeps_two_jobs() {
        let telemetry = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(GIB),
            cgroup_limit_bytes: Some(8 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let budget = resolve_cargo_job_budget_from(None, None, 8, &telemetry);

        assert_eq!(budget.effective_jobs, 2);
        assert_eq!(budget.requested_jobs, 8);
        assert_eq!(budget.source, CargoJobBudgetSource::CgroupHeadroom);
    }

    #[test]
    fn shared_budget_counts_every_independent_cargo_jobserver() {
        let two_slot_host = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(GIB),
            cgroup_limit_bytes: Some(8 * GIB),
            ..CargoMemoryTelemetry::default()
        };
        let four_slot_host = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(GIB),
            cgroup_limit_bytes: Some(16 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let one_each = shared_cargo_process_budget_from("1", 2, 8, &two_slot_host);
        assert_eq!(one_each.requested_slots, Some(2));
        assert_eq!(one_each.available_slots, 2);
        assert!(one_each.may_overlap);

        let two_each = shared_cargo_process_budget_from("2", 2, 8, &two_slot_host);
        assert_eq!(two_each.requested_slots, Some(4));
        assert!(!two_each.may_overlap);

        let roomy = shared_cargo_process_budget_from("2", 2, 8, &four_slot_host);
        assert_eq!(roomy.available_slots, 4);
        assert!(roomy.may_overlap);
    }

    #[test]
    fn malformed_explicit_jobs_serializes_without_rewriting_the_value() {
        let telemetry = CargoMemoryTelemetry {
            system_available_bytes: Some(16 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let budget = shared_cargo_process_budget_from("not-a-number", 2, 8, &telemetry);

        assert_eq!(budget.requested_slots, None);
        assert!(!budget.may_overlap);
    }

    #[test]
    fn explicit_cargo_build_jobs_is_never_silently_rewritten() {
        let telemetry = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(3 * GIB),
            cgroup_limit_bytes: Some(4 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let budget = resolve_cargo_job_budget_from(Some("8"), None, 8, &telemetry);

        assert_eq!(budget.effective_jobs, 8);
        assert_eq!(budget.source, CargoJobBudgetSource::ExplicitEnvironment);
    }

    #[test]
    fn cargo_short_jobs_flag_is_an_explicit_override() {
        let telemetry = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(3 * GIB),
            cgroup_limit_bytes: Some(4 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let budget = resolve_cargo_job_budget_from(None, Some(6), 8, &telemetry);

        assert_eq!(budget.effective_jobs, 6);
        assert_eq!(budget.source, CargoJobBudgetSource::ExplicitArgument);
    }

    #[test]
    fn precompiler_enomem_diagnostic_names_policy_and_kernel_evidence() {
        let telemetry = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(3 * GIB),
            cgroup_peak_bytes: Some(4 * GIB),
            cgroup_limit_bytes: Some(4 * GIB),
            cgroup_oom_kills: Some(1),
            cgroup_pids_current: Some(42),
            cgroup_pids_limit: Some(512),
            ..CargoMemoryTelemetry::default()
        };
        let budget = resolve_cargo_job_budget_from(None, None, 8, &telemetry);
        let stderr = "failed to determine package fingerprint\nCaused by:\n  Cannot allocate memory (os error 12)";

        let diagnostic = precompiler_exhaustion_diagnostic(stderr, &budget, &telemetry)
            .expect("directory-scan ENOMEM must be diagnosed");

        assert!(diagnostic.contains("Cargo orchestration"), "{diagnostic}");
        assert!(
            diagnostic.contains("before compiler admission"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("effective jobs=1"), "{diagnostic}");
        assert!(diagnostic.contains("memory.peak=4.00 GiB"), "{diagnostic}");
        assert!(diagnostic.contains("oom_kill=1"), "{diagnostic}");
        assert!(diagnostic.contains("pids.current=42"), "{diagnostic}");
        assert!(diagnostic.contains("pids.max=512"), "{diagnostic}");
        assert!(
            diagnostic.contains("per-job reserve=3.00 GiB"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("CARGO_BUILD_JOBS"), "{diagnostic}");
    }

    #[test]
    fn larger_measured_headroom_scales_above_two_without_exceeding_cpu() {
        let telemetry = CargoMemoryTelemetry {
            cgroup_current_bytes: Some(GIB),
            cgroup_limit_bytes: Some(16 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let budget = resolve_cargo_job_budget_from(None, None, 4, &telemetry);

        assert_eq!(budget.effective_jobs, 4);
        assert_eq!(budget.source, CargoJobBudgetSource::CgroupHeadroom);
    }

    #[test]
    fn memavailable_is_used_only_when_the_cgroup_limit_is_unbounded() {
        let telemetry = CargoMemoryTelemetry {
            system_available_bytes: Some(7 * GIB),
            ..CargoMemoryTelemetry::default()
        };

        let budget = resolve_cargo_job_budget_from(None, None, 8, &telemetry);

        assert_eq!(budget.effective_jobs, 2);
        assert_eq!(budget.source, CargoJobBudgetSource::SystemAvailable);
    }

    #[test]
    fn unreadable_memory_telemetry_falls_back_to_cpu_without_guessing() {
        let budget = resolve_cargo_job_budget_from(None, None, 6, &CargoMemoryTelemetry::default());

        assert_eq!(budget.effective_jobs, 6);
        assert_eq!(budget.source, CargoJobBudgetSource::CpuOnly);
    }

    #[test]
    fn cgroup_fixture_captures_peak_swap_and_oom_counters() {
        let dir = tempfile::tempdir().expect("cgroup fixture");
        std::fs::write(dir.path().join("memory.current"), "1073741824\n").unwrap();
        std::fs::write(dir.path().join("memory.peak"), "4294967296\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "8589934592\n").unwrap();
        std::fs::write(dir.path().join("memory.swap.current"), "536870912\n").unwrap();
        std::fs::write(dir.path().join("memory.swap.max"), "2147483648\n").unwrap();
        std::fs::write(dir.path().join("pids.current"), "37\n").unwrap();
        std::fs::write(dir.path().join("pids.max"), "512\n").unwrap();
        std::fs::write(
            dir.path().join("memory.events"),
            "low 0\nhigh 2\nmax 3\noom 1\noom_kill 1\noom_group_kill 2\n",
        )
        .unwrap();
        let meminfo = dir.path().join("meminfo");
        std::fs::write(
            &meminfo,
            "MemTotal: 16000000 kB\nMemAvailable: 7000000 kB\n",
        )
        .unwrap();

        let telemetry = CargoMemoryTelemetry::read_at(dir.path(), &meminfo);

        assert_eq!(telemetry.cgroup_current_bytes, Some(GIB));
        assert_eq!(telemetry.cgroup_peak_bytes, Some(4 * GIB));
        assert_eq!(telemetry.cgroup_limit_bytes, Some(8 * GIB));
        assert_eq!(telemetry.cgroup_swap_current_bytes, Some(GIB / 2));
        assert_eq!(telemetry.cgroup_swap_limit_bytes, Some(2 * GIB));
        assert_eq!(telemetry.cgroup_oom_kills, Some(3));
        assert_eq!(telemetry.cgroup_pids_current, Some(37));
        assert_eq!(telemetry.cgroup_pids_limit, Some(512));
        assert_eq!(telemetry.system_available_bytes, Some(7_000_000 * 1024));
    }

    #[test]
    fn cgroup_max_uses_memavailable_fallback() {
        let dir = tempfile::tempdir().expect("cgroup fixture");
        std::fs::write(dir.path().join("memory.current"), "1073741824\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        let meminfo = dir.path().join("meminfo");
        std::fs::write(&meminfo, "MemAvailable: 7340032 kB\n").unwrap();

        let telemetry = CargoMemoryTelemetry::read_at(dir.path(), &meminfo);
        let budget = resolve_cargo_job_budget_from(None, None, 8, &telemetry);

        assert_eq!(telemetry.cgroup_limit_bytes, None);
        assert!(telemetry.cgroup_limit_unbounded);
        assert_eq!(budget.effective_jobs, 2);
        assert_eq!(budget.source, CargoJobBudgetSource::SystemAvailable);
    }

    #[test]
    fn jobs_arguments_stop_at_cargo_double_dash() {
        assert_eq!(
            explicit_jobs_argument(&["build".into(), "-j6".into()]),
            Some(Some(6))
        );
        assert_eq!(
            explicit_jobs_argument(&["build".into(), "--jobs".into(), "3".into()]),
            Some(Some(3))
        );
        assert_eq!(
            explicit_jobs_argument(&["build".into(), "--".into(), "-j8".into()]),
            None,
            "arguments after Cargo's separator belong to the compiled target"
        );
    }

    #[test]
    fn unrelated_enomem_is_not_misattributed_to_cargo_orchestration() {
        let budget = resolve_cargo_job_budget_from(None, None, 4, &CargoMemoryTelemetry::default());
        assert_eq!(
            precompiler_exhaustion_diagnostic(
                "rustc LLVM ERROR: out of memory; Cannot allocate memory (os error 12)",
                &budget,
                &CargoMemoryTelemetry::default(),
            ),
            None
        );
    }
}
