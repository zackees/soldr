//! Explain OS-level process-creation failures that are not build errors.
//!
//! soldr#1974. When the host cannot supply the resources to start a
//! process, Windows refuses to initialize its DLLs and the child dies
//! before `main` with `STATUS_DLL_INIT_FAILED`. Nothing was wrong with the
//! code, the toolchain, or the cache -- but every layer above reports it as
//! though something was:
//!
//! - cargo prints `process didn't exit successfully: ... (exit code:
//!   0xc0000142, STATUS_DLL_INIT_FAILED)`, which reads as a crash
//! - when the victim is `link.exe`, rustc appends *"the Visual Studio build
//!   tools may need to be repaired"* -- actively misleading advice that
//!   sends people to reinstall a toolchain that is fine
//!
//! The condition is transient and host-wide: it strikes whichever process
//! happens to start next, so the victim rotates between runs and an
//! identical retry usually succeeds. That combination -- rotating victim,
//! passes on retry, plausible-looking toolchain error -- is what makes it
//! expensive to diagnose.
//!
//! This module recognizes the one exit code that is unambiguously this
//! condition and says so.

use std::io::Write;

/// `STATUS_DLL_INIT_FAILED` (`0xC0000142`) as it appears through
/// [`std::process::ExitStatus::code`].
///
/// Windows surfaces the NTSTATUS as the process exit code, and Rust widens
/// it to `i32`, so the comparison is against the sign-reinterpreted value
/// (`-1073741502`) rather than the literal.
///
/// The platform crate owns the classification: a Unix `ExitStatus::code`
/// is a 0-255 wait status and can never collide with this value, so the
/// check is harmless there and the tests run on every platform.
pub(crate) const STATUS_DLL_INIT_FAILED: i32 = 0xC000_0142_u32 as i32;

/// True when `exit_code` is the process-initialization failure above.
pub(crate) fn is_process_init_failure(exit_code: i32) -> bool {
    crate::platform::process::exit::is_init_failure(exit_code)
}

/// The advisory printed when a compiler dies at process init.
///
/// Kept separate from the writer so tests can assert the wording without
/// capturing stderr.
///
/// # Why this names no single cause
///
/// The first version of this message asserted handle / paged-pool
/// exhaustion. That was wrong, and shipped: `0xC0000142` was then observed
/// on a host with **205,910 total handles and a 8,356-handle top holder** --
/// entirely healthy. Several distinct resources produce this same code:
///
/// - **desktop heap** (`SharedSection` in the `SubSystems` registry value) --
///   a fixed shared section consumed per-process and *unrelated* to the
///   handle table. A build spawning many short-lived compilers exhausts it
///   while handles stay flat, which is exactly the observed case.
/// - **handle / paged-pool exhaustion** -- typically one leaking process
///   holding hundreds of thousands of handles.
/// - a genuinely missing or incompatible DLL, which is *not* a resource
///   condition at all and would not pass on retry.
///
/// So the note reports what is certain -- the OS refused to start the
/// process, and the "repair your build tools" advice above it is a false
/// lead -- and then lists what to check. Asserting a cause the tool has not
/// measured sends people to fix the wrong thing, which is the exact failure
/// this module exists to prevent.
pub(crate) fn process_init_failure_note(tool: &str) -> String {
    format!(
        "soldr: {tool} exited 0x{:08X} (STATUS_DLL_INIT_FAILED) -- the OS refused to \
         initialize the process, so it died before running.\n\
         soldr: this is a host process-creation failure, not a build, cache, or toolchain \
         error. Any \"repair your build tools\" advice above is a false lead.\n\
         soldr: it is usually a resource the host could not supply. Check, in order:\n\
         soldr:   1. desktop heap -- often the cause when many compilers run at once, and\n\
         soldr:      independent of handle count. Lower build parallelism to test:\n\
         soldr:        CARGO_BUILD_JOBS=4 soldr cargo ...\n\
         soldr:   2. handle exhaustion -- look for one process holding a very large count:\n\
         soldr:        Get-Process | Sort-Object HandleCount -Descending | Select-Object -First 5 Name,Id,HandleCount\n\
         soldr: the victim rotates between runs, so an identical retry often succeeds -- \
         which means a passing retry does not mean the condition is gone.",
        STATUS_DLL_INIT_FAILED as u32
    )
}

/// Print [`process_init_failure_note`] to `sink` when `exit_code` warrants it.
///
/// Returns whether the note fired, so callers can record it. Write failures
/// are ignored: this is advisory output on an already-failing path and must
/// never mask the original exit code.
pub(crate) fn report_process_init_failure(
    sink: &mut impl Write,
    tool: &str,
    exit_code: i32,
) -> bool {
    if !is_process_init_failure(exit_code) {
        return false;
    }
    let _ = writeln!(sink, "{}", process_init_failure_note(tool));
    true
}

/// Convenience wrapper over [`report_process_init_failure`] targeting stderr.
///
/// soldr#2021: when the note fires, this also captures and prints a **live**
/// host-pressure snapshot (process counts, commit charge, parallelism)
/// immediately -- soldr is the only observer present at the instant of a
/// `STATUS_DLL_INIT_FAILED`, and every prior investigation sampled the host
/// *after* the fact, blind to the transient peak. The snapshot is best-effort:
/// every probe yields `None` on failure and nothing here can change the
/// already-set exit code.
pub(crate) fn report_process_init_failure_to_stderr(tool: &str, exit_code: i32) -> bool {
    // soldr#2024: both direct-exec paths call this immediately after the
    // child ran with inherited stdio, which makes it the one place that
    // sees "a tool spoke for this invocation" without either caller
    // growing a line. The child owns the explanation from here.
    crate::exit_guard::mark_spoke();
    let fired = report_process_init_failure(&mut std::io::stderr(), tool, exit_code);
    if fired {
        // Sample DURING the failure, not after (soldr#2021 step #2).
        let snapshot = capture_snapshot();
        let _ = writeln!(std::io::stderr(), "{}", format_snapshot(&snapshot, tool));
    }
    fired
}

/// A best-effort, point-in-time capture of host process/memory pressure taken
/// at the instant a compiler dies at process init (soldr#2021).
///
/// Every field is `Option` because any individual probe may fail, and on an
/// already-failing build path a probe failure must degrade to "unknown"
/// rather than panic or mask the original exit code. On non-Windows hosts only
/// [`jobs`](Self::jobs) is populated -- the Win32 counters have no analogue and
/// stay `None` so the module still compiles and unit-tests on the Linux dev
/// harness.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct HostPressureSnapshot {
    /// Build parallelism: `CARGO_BUILD_JOBS` if set and parseable, else
    /// [`std::thread::available_parallelism`].
    pub(crate) jobs: Option<usize>,
    /// Total number of running processes on the host (Windows only).
    pub(crate) total_processes: Option<u32>,
    /// Of those, how many are compiler/toolchain-ish images
    /// (see [`is_compiler_image`]) (Windows only).
    pub(crate) compiler_processes: Option<u32>,
    /// System commit charge currently in use, in MiB (Windows only).
    pub(crate) commit_used_mb: Option<u64>,
    /// System commit limit, in MiB (Windows only).
    pub(crate) commit_limit_mb: Option<u64>,
}

/// Resolve build parallelism the same way cargo/soldr would perceive it:
/// an explicit `CARGO_BUILD_JOBS` wins, otherwise the detected core count.
///
/// Pure except for the two reads (env + core count); both degrade to `None`.
fn resolve_jobs() -> Option<usize> {
    if let Ok(raw) = std::env::var("CARGO_BUILD_JOBS") {
        if let Ok(n) = raw.trim().parse::<usize>() {
            return Some(n);
        }
    }
    std::thread::available_parallelism().ok().map(|n| n.get())
}

/// True when an image name is a compiler/linker/toolchain process worth
/// counting separately in the snapshot.
///
/// Case-insensitive; matches the exact images the issue calls out plus the
/// `cc1*` / `clang*` prefixes that appear on GNU/LLVM toolchains. Pure, so it
/// is unit-tested on every platform.
///
/// Only the Windows probe consumes it in production, so a non-Windows,
/// non-test build sees it as unused -- that is expected, not dead code.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_compiler_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const EXACT: &[&str] = &["rustc.exe", "link.exe", "soldr.exe", "soldr-daemon.exe"];
    if EXACT.contains(&lower.as_str()) {
        return true;
    }
    // Prefix families: cc1, cc1plus, clang, clang-cl, clang++ ...
    lower.starts_with("cc1") || lower.starts_with("clang")
}

/// Render the snapshot as `soldr: ...` advisory lines.
///
/// Pure -- takes an already-captured struct and does no probing -- so it is
/// unit-tested on all platforms with synthetic inputs. Unknown fields render
/// as `unknown` rather than being omitted, so the reader can tell "we looked
/// and could not tell" from "we did not look".
fn format_snapshot(snap: &HostPressureSnapshot, tool: &str) -> String {
    fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|x| x.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
    let mut out = String::new();
    out.push_str(&format!(
        "soldr: host-pressure snapshot at {tool} process-init failure (soldr#2021, \
         sampled live at the moment of failure):\n"
    ));
    out.push_str(&format!(
        "soldr:   build parallelism (jobs): {}\n",
        opt(snap.jobs)
    ));
    out.push_str(&format!(
        "soldr:   running processes (total): {}\n",
        opt(snap.total_processes)
    ));
    out.push_str(&format!(
        "soldr:   compiler/linker processes (rustc/link/soldr/clang/cc1): {}\n",
        opt(snap.compiler_processes)
    ));
    out.push_str(&format!(
        "soldr:   commit charge: {} MiB used of {} MiB limit",
        opt(snap.commit_used_mb),
        opt(snap.commit_limit_mb)
    ));
    out
}

/// Capture the live host-pressure snapshot.
///
/// The platform crate owns the probes (a ToolHelp process-table walk and
/// the `GlobalMemoryStatusEx` commit charge on Windows; `None` on hosts
/// without a Win32 analogue). Best-effort: each probe is isolated so a
/// failure yields `None` for that field only. No retries, sleeps, or
/// network -- the walk is bounded and fast, which matters because this
/// runs on an already-failing build path and must never hang or change
/// the exit code.
fn capture_snapshot() -> HostPressureSnapshot {
    let (total_processes, compiler_processes) =
        match crate::platform::host::resources::process_table() {
            Some(rows) => {
                let total = rows.len() as u32;
                let compilerish = rows
                    .iter()
                    .filter(|(_, name)| is_compiler_image(name))
                    .count() as u32;
                (Some(total), Some(compilerish))
            }
            None => (None, None),
        };
    let (commit_used_mb, commit_limit_mb) =
        match crate::platform::host::resources::commit_charge_mb() {
            Some((used, limit)) => (Some(used), Some(limit)),
            None => (None, None),
        };
    HostPressureSnapshot {
        jobs: resolve_jobs(),
        total_processes,
        compiler_processes,
        commit_used_mb,
        commit_limit_mb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The whole check hinges on this constant matching what
    // `ExitStatus::code()` actually returns, which is the sign-reinterpreted
    // NTSTATUS rather than the literal 0xC0000142.
    crate::timed_test!(status_constant_is_the_sign_reinterpreted_ntstatus, {
        assert_eq!(STATUS_DLL_INIT_FAILED, -1_073_741_502);
        assert_eq!(STATUS_DLL_INIT_FAILED as u32, 0xC000_0142);
    });

    crate::timed_test!(recognizes_only_the_process_init_failure_code, {
        assert!(is_process_init_failure(STATUS_DLL_INIT_FAILED));
        // Ordinary compiler failures must not be explained away as a host
        // condition -- that would turn a real build error into a "just retry"
        // message and hide the bug.
        for code in [0, 1, 2, 101, 255, -1] {
            assert!(
                !is_process_init_failure(code),
                "exit code {code} must not be treated as process-init failure"
            );
        }
    });

    crate::timed_test!(note_names_the_tool_and_contradicts_the_toolchain_advice, {
        let note = process_init_failure_note("link.exe");
        assert!(note.contains("link.exe"), "{note}");
        assert!(note.contains("STATUS_DLL_INIT_FAILED"), "{note}");
        assert!(note.contains("0xC0000142"), "{note}");
        // The rustc-emitted "repair your build tools" line is the specific
        // false lead this exists to counter, so the rebuttal must be explicit.
        assert!(note.contains("false lead"), "{note}");
    });

    // Regression guard for the correction in this module's docs: the first
    // shipped wording asserted handle exhaustion, and 0xC0000142 was then
    // observed on a host with entirely healthy handles. Naming one unmeasured
    // cause sends people to fix the wrong thing -- the precise failure this
    // module exists to prevent -- so the note must offer the alternatives.
    crate::timed_test!(note_does_not_assert_a_single_unmeasured_cause, {
        let note = process_init_failure_note("rustc.exe");
        assert!(
            note.contains("desktop heap"),
            "desktop heap is the cause observed with healthy handles; it must be listed: {note}"
        );
        assert!(
            note.contains("HandleCount"),
            "handle exhaustion must remain one of the candidates: {note}"
        );
        assert!(
            !note.contains("(handle / paged-pool exhaustion)"),
            "must not assert a cause soldr has not measured: {note}"
        );
        // A passing retry is the trap: it looks like resolution and is not.
        assert!(
            note.contains("does not mean the condition is gone"),
            "{note}"
        );
    });

    crate::timed_test!(reports_only_on_the_matching_exit_code, {
        let mut sink = Vec::new();
        assert!(!report_process_init_failure(&mut sink, "rustc.exe", 1));
        assert!(sink.is_empty(), "a plain exit 1 must stay silent");

        let mut sink = Vec::new();
        assert!(report_process_init_failure(
            &mut sink,
            "rustc.exe",
            STATUS_DLL_INIT_FAILED
        ));
        let out = String::from_utf8(sink).expect("utf8");
        assert!(out.contains("rustc.exe"), "{out}");
    });

    // --- soldr#2021: live host-pressure snapshot -------------------------

    // The image classifier drives the compiler-process count, so it must
    // match exactly the images the issue names (case-insensitively) and the
    // cc1*/clang* prefix families, and nothing else.
    crate::timed_test!(compiler_image_matches_toolchain_processes_only, {
        for yes in [
            "rustc.exe",
            "RUSTC.EXE",
            "link.exe",
            "soldr.exe",
            "soldr-daemon.exe",
            "cc1.exe",
            "cc1plus.exe",
            "clang.exe",
            "clang-cl.exe",
            "clang++",
        ] {
            assert!(is_compiler_image(yes), "should classify as compiler: {yes}");
        }
        for no in [
            "cargo.exe",
            "explorer.exe",
            "svchost.exe",
            "notepad.exe",
            "",
        ] {
            assert!(
                !is_compiler_image(no),
                "should NOT classify as compiler: {no}"
            );
        }
    });

    // The formatter is pure: given a fully-populated struct it must surface
    // every field, the tool name, and the issue reference.
    crate::timed_test!(format_snapshot_renders_all_known_fields, {
        let snap = HostPressureSnapshot {
            jobs: Some(16),
            total_processes: Some(412),
            compiler_processes: Some(9),
            commit_used_mb: Some(56_000),
            commit_limit_mb: Some(270_000),
        };
        let out = format_snapshot(&snap, "link.exe");
        assert!(out.contains("link.exe"), "{out}");
        assert!(out.contains("2021"), "{out}");
        assert!(out.contains("16"), "jobs: {out}");
        assert!(out.contains("412"), "total procs: {out}");
        assert!(out.contains("9"), "compiler procs: {out}");
        assert!(out.contains("56000"), "commit used: {out}");
        assert!(out.contains("270000"), "commit limit: {out}");
    });

    // Every probe may fail on the already-failing path, so an all-None struct
    // must still render -- as "unknown", so a missed probe is distinguishable
    // from a genuine zero.
    crate::timed_test!(format_snapshot_renders_unknown_for_missing_probes, {
        let snap = HostPressureSnapshot::default();
        let out = format_snapshot(&snap, "rustc.exe");
        assert!(out.contains("rustc.exe"), "{out}");
        assert!(
            out.contains("unknown"),
            "missing probes must read 'unknown': {out}"
        );
        // No panic and no empty output is the real contract here.
        assert!(!out.is_empty(), "{out}");
    });

    // The capture entry point must never panic and must always populate at
    // least `jobs` (available on every platform). This exercises the real
    // probes on Windows and the stub elsewhere.
    crate::timed_test!(capture_snapshot_is_infallible_and_reports_jobs, {
        let snap = capture_snapshot();
        assert!(
            snap.jobs.is_some(),
            "jobs is derivable on every platform: {snap:?}"
        );
        // Formatting the real capture must also never panic.
        let _ = format_snapshot(&snap, "rustc.exe");
    });

    // CARGO_BUILD_JOBS, when set and parseable, wins over the detected core
    // count -- that is the knob the note tells users to turn.
    crate::timed_test!(resolve_jobs_prefers_cargo_build_jobs, {
        // NOTE: single-threaded env mutation within one test body.
        let prev = std::env::var("CARGO_BUILD_JOBS").ok();
        std::env::set_var("CARGO_BUILD_JOBS", "3");
        assert_eq!(resolve_jobs(), Some(3));
        std::env::set_var("CARGO_BUILD_JOBS", "not-a-number");
        // Unparseable falls back to the detected count (some Some value).
        assert!(resolve_jobs().is_some());
        match prev {
            Some(v) => std::env::set_var("CARGO_BUILD_JOBS", v),
            None => std::env::remove_var("CARGO_BUILD_JOBS"),
        }
    });
}
