//! On-CPU/off-CPU profile and task-inventory artifacts for a breach dump
//! (soldr#3053 remaining scope). The cgroup.json + in-flight compile list
//! half of #3053 already landed in `rss_ceiling.rs`
//! (`cgroup_and_inflight_json`, soldr#3281); this module carries the rest of
//! that issue's acceptance criteria: an on-CPU profile artifact (or a
//! manifest naming why one could not be produced), an off-CPU
//! approximation (or a manifest recording the kernel-capability finding),
//! a tokio task inventory (or a manifest reason), and — via
//! [`ArtifactOutcome`] itself — a uniform "pending kill decision" shape a
//! caller can serialize alongside `summary.json` without inventing a new
//! ad hoc `Option<PathBuf>` per artifact.
//!
//! ## Design decisions
//!
//! - **On-CPU sampling shells out to `perf`, rather than vendoring
//!   `perf_event_open`.** A hand-rolled `perf_event_open` + ring-buffer
//!   reader is a second, competing implementation of exactly what `perf
//!   record` already does, and it would need its own privilege probing,
//!   symbolization, and buffer management. `perf record -g` is the same
//!   capability soldr's own tracing docs point operators at manually, so
//!   shelling out keeps this module's job to invocation, timeout
//!   enforcement, and turning `perf`'s own failure modes (missing binary,
//!   `perf_event_paranoid`, timeout) into a legible [`ArtifactOutcome`].
//! - **Off-CPU tracing is approximated, not measured.** True off-CPU
//!   (`sched_switch`) tracing needs eBPF or ftrace, both of which require
//!   `CAP_PERFMON`/`CAP_SYS_ADMIN` or a readable tracefs mount — capabilities
//!   an unprivileged `soldr-daemon` does not have and must not assume it can
//!   acquire. [`capture_off_cpu_approximation`] substitutes a windowed delta
//!   of each thread's own `/proc/self/task/<tid>/schedstat` (on-CPU run time
//!   and run-queue wait time), from which "was this thread neither running
//!   nor runnable" is inferred as the remainder of the window. This is a
//!   strictly weaker signal than a real `sched_switch` trace — it cannot say
//!   *why* a thread was off-CPU (I/O wait vs. lock contention vs. voluntary
//!   sleep), only that it was — but it needs no elevated capability and is
//!   always available to an unprivileged daemon. The kernel-capability gap
//!   is recorded verbatim in the output JSON so a reader never mistakes the
//!   approximation for the real thing.
//! - **Task inventory has two tiers.** `tokio::runtime::Handle::metrics()`
//!   is stable, needs no feature flag, and always answers "how many workers,
//!   how many alive tasks, how deep is the global queue" when this process
//!   is inside a tokio runtime. Per-task detail (which task, how long
//!   blocked, its poll history) is what `tokio-console` records, and that
//!   requires the daemon to have been built with the (non-default)
//!   `tokio-console` feature and a `console-subscriber` recording already in
//!   progress — see `server_runtime.rs`'s `TOKIO_CONSOLE_ENV_VAR` /
//!   `TOKIO_CONSOLE_RECORD_PATH_ENV_VAR`. When that recording is not
//!   available this function still writes the runtime-wide counters and
//!   returns an `unavailable` outcome naming the missing per-task detail,
//!   rather than silently reporting "written" for a strictly smaller
//!   artifact than the caller asked for.
//! - **No `#[cfg(target_os = ...)]` anywhere.** Every platform gap
//!   (`/proc` absence, missing `perf`, an unreadable tracefs mount) is a
//!   runtime probe, matching `rss_ceiling.rs`'s `copy_proc_snapshot`
//!   convention and `docs/CI_MODES.md`'s platform-cfg-boundary rule — a
//!   Linux-only artifact degrades to a named `unavailable_reason` on any
//!   other host rather than failing to compile there.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Wall-clock bound applied to every shelled-out `perf` invocation. Chosen
/// to comfortably exceed the `sleep 1` sampling window `perf record` itself
/// runs for, while still failing fast if `perf` hangs (e.g. waiting on a
/// permission prompt that will never come).
const PERF_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll interval used while waiting out [`PERF_TIMEOUT`].
const PERF_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Window over which the off-CPU schedstat approximation is measured. Long
/// enough that a live thread accumulates a measurable delta, short enough
/// that this stays a cheap best-effort probe rather than a real profiling
/// session.
const OFF_CPU_WINDOW: Duration = Duration::from_millis(200);

/// Outcome of one forensics artifact capture: exactly one of `path` /
/// `unavailable_reason` is `Some`. Use [`ArtifactOutcome::written`] /
/// [`ArtifactOutcome::unavailable`] to construct one rather than the struct
/// literal, so that invariant cannot be violated by a call site forgetting
/// to clear the other field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactOutcome {
    pub path: Option<PathBuf>,
    pub unavailable_reason: Option<String>,
}

impl ArtifactOutcome {
    pub fn written(path: PathBuf) -> Self {
        ArtifactOutcome {
            path: Some(path),
            unavailable_reason: None,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        ArtifactOutcome {
            path: None,
            unavailable_reason: Some(reason.into()),
        }
    }
}

/// Best-effort on-CPU sampling profile of this process via `perf record`.
/// See module docs for why this shells out rather than vendoring
/// `perf_event_open`.
pub(crate) fn capture_on_cpu_profile(dir: &Path) -> ArtifactOutcome {
    capture_on_cpu_profile_with(dir, find_perf_on_path())
}

/// Core implementation with the `perf` binary path injectable, so tests can
/// exercise both "not found" and "found but fails" without depending on the
/// host's actual PATH.
fn capture_on_cpu_profile_with(dir: &Path, perf_bin: Option<PathBuf>) -> ArtifactOutcome {
    if !Path::new("/proc/self").is_dir() {
        return ArtifactOutcome::unavailable(
            "no /proc: on-CPU sampling via perf is Linux-only on this build",
        );
    }
    let Some(perf_bin) = perf_bin else {
        return ArtifactOutcome::unavailable("perf not found on PATH");
    };

    let paranoid = read_perf_event_paranoid();
    let data_path = dir.join("on-cpu.perf.data");
    let record = Command::new(&perf_bin)
        .arg("record")
        .arg("-F")
        .arg("99")
        .arg("-g")
        .arg("-p")
        .arg(std::process::id().to_string())
        .arg("-o")
        .arg(&data_path)
        .arg("--")
        .arg("sleep")
        .arg("1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match record {
        Ok(child) => child,
        Err(error) => {
            return ArtifactOutcome::unavailable(format!("failed to spawn perf record: {error}"));
        }
    };

    let (status, stderr) = match wait_with_timeout(&mut child, PERF_TIMEOUT) {
        WaitOutcome::Exited(status) => {
            let stderr = read_stderr(&mut child);
            (Some(status), stderr)
        }
        WaitOutcome::TimedOut => {
            let _ = child.kill();
            let _ = child.wait();
            return ArtifactOutcome::unavailable("perf record timed out");
        }
        WaitOutcome::WaitError(error) => {
            return ArtifactOutcome::unavailable(format!("perf record wait failed: {error}"));
        }
    };

    let status = match status {
        Some(status) => status,
        None => return ArtifactOutcome::unavailable("perf record produced no exit status"),
    };

    if !status.success() {
        let paranoid_display = paranoid
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unreadable".to_string());
        let truncated = truncate_bytes(&stderr, 512);
        return ArtifactOutcome::unavailable(format!(
            "perf record failed (exit {code}, perf_event_paranoid={paranoid}): {stderr}",
            code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            paranoid = paranoid_display,
            stderr = truncated,
        ));
    }

    // Best-effort: turn the raw perf.data into a readable script. Failure
    // here does not invalidate the capture -- the raw perf.data is still a
    // valid, if less legible, artifact.
    let script_path = dir.join("on-cpu.perf-script.txt");
    if run_perf_script(&perf_bin, &data_path, &script_path) {
        ArtifactOutcome::written(script_path)
    } else {
        ArtifactOutcome::written(data_path)
    }
}

/// Run `perf script -i <data_path>`, writing stdout to `dest`. Returns
/// whether it succeeded within [`PERF_TIMEOUT`].
fn run_perf_script(perf_bin: &Path, data_path: &Path, dest: &Path) -> bool {
    let spawn = Command::new(perf_bin)
        .arg("script")
        .arg("-i")
        .arg(data_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(_) => return false,
    };
    match wait_with_timeout(&mut child, PERF_TIMEOUT) {
        WaitOutcome::Exited(status) if status.success() => {
            let mut stdout_buf = Vec::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_end(&mut stdout_buf);
            }
            std::fs::write(dest, stdout_buf).is_ok()
        }
        WaitOutcome::Exited(_) => false,
        WaitOutcome::TimedOut => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
        WaitOutcome::WaitError(_) => false,
    }
}

enum WaitOutcome {
    Exited(std::process::ExitStatus),
    TimedOut,
    WaitError(std::io::Error),
}

/// Poll `child` with `try_wait` until it exits or `timeout` elapses.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> WaitOutcome {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return WaitOutcome::Exited(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return WaitOutcome::TimedOut;
                }
                std::thread::sleep(PERF_POLL_INTERVAL);
            }
            Err(error) => return WaitOutcome::WaitError(error),
        }
    }
}

fn read_stderr(child: &mut Child) -> String {
    let mut buf = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max).collect();
        truncated.push_str("...(truncated)");
        truncated
    }
}

fn read_perf_event_paranoid() -> Option<i64> {
    std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// Locate an executable file named `perf` on `PATH`.
fn find_perf_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("perf");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// One thread's schedstat delta over the sampling window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OffCpuThreadSample {
    tid: u32,
    comm: String,
    run_ns_delta: u64,
    wait_ns_delta: u64,
    off_cpu_ns_estimate: u64,
}

/// Best-effort approximation of off-CPU time per thread, via a windowed
/// `/proc/self/task/<tid>/schedstat` delta. See module docs for why this is
/// an approximation rather than a real `sched_switch` trace.
pub(crate) fn capture_off_cpu_approximation(dir: &Path) -> ArtifactOutcome {
    let task_dir = Path::new("/proc/self/task");
    if !task_dir.is_dir() {
        return ArtifactOutcome::unavailable(
            "no /proc/self/task: off-CPU approximation is Linux-only",
        );
    }

    let before = read_all_schedstat(task_dir);
    std::thread::sleep(OFF_CPU_WINDOW);
    let after = read_all_schedstat(task_dir);

    let window_ns = OFF_CPU_WINDOW.as_nanos() as u64;
    let mut threads = Vec::new();
    for (tid, (run_after, wait_after, _)) in after {
        let Some((run_before, wait_before, _)) = before.get(&tid).copied() else {
            continue;
        };
        let run_delta = run_after.saturating_sub(run_before);
        let wait_delta = wait_after.saturating_sub(wait_before);
        let off_cpu_estimate = window_ns
            .saturating_sub(run_delta)
            .saturating_sub(wait_delta);
        let comm = read_comm(task_dir, tid).unwrap_or_else(|| "unknown".to_string());
        threads.push(OffCpuThreadSample {
            tid,
            comm,
            run_ns_delta: run_delta,
            wait_ns_delta: wait_delta,
            off_cpu_ns_estimate: off_cpu_estimate,
        });
    }

    let tracefs_readable = Path::new("/sys/kernel/tracing/events/sched/sched_switch").is_file()
        && std::fs::File::open("/sys/kernel/tracing/events/sched/sched_switch").is_ok();

    let payload = serde_json::json!({
        "method": "per-tid /proc/<tid>/schedstat delta",
        "kernel_capability": "sched_switch tracing requires CAP_PERFMON/CAP_SYS_ADMIN or \
             readable tracefs; unprivileged daemon uses schedstat deltas instead",
        "tracefs_readable": tracefs_readable,
        "window_ms": OFF_CPU_WINDOW.as_millis() as u64,
        "threads": threads,
    });

    let dest = dir.join("off-cpu-schedstat.json");
    match serde_json::to_vec_pretty(&payload) {
        Ok(body) => match std::fs::write(&dest, body) {
            Ok(()) => ArtifactOutcome::written(dest),
            Err(error) => ArtifactOutcome::unavailable(format!(
                "failed to write off-cpu-schedstat.json: {error}"
            )),
        },
        Err(error) => ArtifactOutcome::unavailable(format!(
            "failed to serialize off-cpu-schedstat.json: {error}"
        )),
    }
}

/// Read every readable `/proc/self/task/<tid>/schedstat` into
/// `tid -> (run_ns, wait_ns, timeslices)`. Unreadable individual entries
/// (a thread that exited mid-scan) are skipped rather than failing the
/// whole read.
fn read_all_schedstat(task_dir: &Path) -> std::collections::HashMap<u32, (u64, u64, u64)> {
    let mut out = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir(task_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(tid) = file_name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let schedstat_path = entry.path().join("schedstat");
        let Ok(body) = std::fs::read_to_string(&schedstat_path) else {
            continue;
        };
        if let Some(fields) = parse_schedstat(&body) {
            out.insert(tid, fields);
        }
    }
    out
}

/// Parse `/proc/<pid>/task/<tid>/schedstat`'s three whitespace-separated
/// fields: run_ns, wait_ns, timeslices. `None` on anything else.
fn parse_schedstat(s: &str) -> Option<(u64, u64, u64)> {
    let mut fields = s.split_whitespace();
    let run_ns = fields.next()?.parse::<u64>().ok()?;
    let wait_ns = fields.next()?.parse::<u64>().ok()?;
    let timeslices = fields.next()?.parse::<u64>().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((run_ns, wait_ns, timeslices))
}

fn read_comm(task_dir: &Path, tid: u32) -> Option<String> {
    std::fs::read_to_string(task_dir.join(tid.to_string()).join("comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Runtime-wide tokio task metrics, when this process is inside a tokio
/// runtime. Kept independent of the `tokio-console` feature -- these
/// counters are stable, always-available tokio APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TokioRuntimeCounters {
    num_workers: usize,
    num_alive_tasks: usize,
    global_queue_depth: usize,
}

/// Tokio task inventory: runtime-wide counters always, per-task detail only
/// when the `tokio-console` feature is enabled and a recording path is
/// available. See module docs for the two-tier rationale.
#[cfg(not(feature = "tokio-console"))]
pub(crate) fn capture_task_inventory(dir: &Path) -> ArtifactOutcome {
    capture_task_inventory_impl(dir, None)
}

#[cfg(feature = "tokio-console")]
pub(crate) fn capture_task_inventory(
    dir: &Path,
    console_record_path: Option<&Path>,
) -> ArtifactOutcome {
    capture_task_inventory_impl(dir, console_record_path)
}

fn capture_task_inventory_impl(dir: &Path, console_record_path: Option<&Path>) -> ArtifactOutcome {
    let runtime_json_path = dir.join("tokio-runtime.json");
    let runtime_written = write_tokio_runtime_json(&runtime_json_path);

    #[cfg(feature = "tokio-console")]
    {
        let Some(record_path) = console_record_path else {
            let reason = if runtime_written {
                "tokio-console enabled but no recording path configured (set \
                 SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH); tokio-runtime.json was written with \
                 runtime-wide counters"
                    .to_string()
            } else {
                "tokio-console enabled but no recording path configured (set \
                 SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH)"
                    .to_string()
            };
            return ArtifactOutcome::unavailable(reason);
        };
        if !record_path.is_file() {
            return ArtifactOutcome::unavailable(format!(
                "tokio-console recording path {} is not a file",
                record_path.display()
            ));
        }
        let dest = dir.join("tokio-console-record.bin");
        return match std::fs::copy(record_path, &dest) {
            Ok(_) => ArtifactOutcome::written(dest),
            Err(error) => ArtifactOutcome::unavailable(format!(
                "failed to copy tokio-console recording: {error}"
            )),
        };
    }

    #[cfg(not(feature = "tokio-console"))]
    {
        let _ = console_record_path;
        let reason = if runtime_written {
            "soldr-daemon built without the tokio-console feature; per-task inventory \
             unavailable (runtime counters in tokio-runtime.json)"
                .to_string()
        } else {
            "soldr-daemon built without the tokio-console feature; per-task inventory \
             unavailable"
                .to_string()
        };
        ArtifactOutcome::unavailable(reason)
    }
}

/// Best-effort write of `tokio-runtime.json`. Returns whether it was
/// written (either because a tokio runtime was current, or because the
/// write of an "unavailable" placeholder still counts as informative).
fn write_tokio_runtime_json(dest: &Path) -> bool {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    let metrics = handle.metrics();
    let counters = TokioRuntimeCounters {
        num_workers: metrics.num_workers(),
        num_alive_tasks: metrics.num_alive_tasks(),
        global_queue_depth: metrics.global_queue_depth(),
    };
    match serde_json::to_vec_pretty(&counters) {
        Ok(body) => std::fs::write(dest, body).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_exactly_one_some(outcome: &ArtifactOutcome) {
        assert_eq!(
            outcome.path.is_some(),
            outcome.unavailable_reason.is_none(),
            "exactly one of path/unavailable_reason must be Some: {outcome:?}"
        );
    }

    #[test]
    fn artifact_outcome_written_sets_only_path() {
        let outcome = ArtifactOutcome::written(PathBuf::from("/tmp/example"));
        assert_exactly_one_some(&outcome);
        assert!(outcome.path.is_some());
    }

    #[test]
    fn artifact_outcome_unavailable_sets_only_reason() {
        let outcome = ArtifactOutcome::unavailable("nope");
        assert_exactly_one_some(&outcome);
        assert_eq!(outcome.unavailable_reason.as_deref(), Some("nope"));
    }

    #[test]
    fn parse_schedstat_valid() {
        assert_eq!(parse_schedstat("123 456 7"), Some((123, 456, 7)));
        assert_eq!(parse_schedstat("  1   2   3  \n"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_schedstat_garbage() {
        assert_eq!(parse_schedstat(""), None);
        assert_eq!(parse_schedstat("1 2"), None);
        assert_eq!(parse_schedstat("1 2 3 4"), None);
        assert_eq!(parse_schedstat("a b c"), None);
    }

    #[test]
    fn on_cpu_profile_without_perf_binary_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = capture_on_cpu_profile_with(dir.path(), None);
        assert_exactly_one_some(&outcome);
        if Path::new("/proc/self").is_dir() {
            assert!(
                outcome
                    .unavailable_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("perf not found"),
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn on_cpu_profile_with_nonexistent_perf_bin_does_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = capture_on_cpu_profile_with(
            dir.path(),
            Some(PathBuf::from("/nonexistent/path/to/perf-binary-xyz")),
        );
        assert_exactly_one_some(&outcome);
        assert!(outcome.unavailable_reason.is_some());
    }

    #[test]
    fn off_cpu_approximation_writes_thread_samples_when_proc_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = capture_off_cpu_approximation(dir.path());
        if !Path::new("/proc/self/task").is_dir() {
            assert_exactly_one_some(&outcome);
            assert!(outcome.unavailable_reason.is_some());
            return;
        }
        assert_exactly_one_some(&outcome);
        let path = outcome.path.expect("written path");
        let body = std::fs::read_to_string(&path).expect("read off-cpu-schedstat.json");
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("off-cpu-schedstat.json must be valid JSON");
        let threads = json["threads"]
            .as_array()
            .expect("threads must be an array");
        assert!(
            !threads.is_empty(),
            "threads array must be non-empty on a host with /proc/self/task: {json}"
        );
        assert!(json["kernel_capability"].is_string());
        assert!(json["tracefs_readable"].is_boolean());
    }

    #[test]
    fn task_inventory_without_feature_or_record_path_is_unavailable_with_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        #[cfg(not(feature = "tokio-console"))]
        let outcome = capture_task_inventory(dir.path());
        #[cfg(feature = "tokio-console")]
        let outcome = capture_task_inventory(dir.path(), None);
        assert_exactly_one_some(&outcome);
        let reason = outcome
            .unavailable_reason
            .expect("must be unavailable with no feature/record path");
        assert!(!reason.is_empty());
    }
}
