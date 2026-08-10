//! External capture for unenrolled targets (S18 / #648).
//!
//! The cooperative path needs the target to have called `probe::install()`.
//! `--force` is for the process that did not — usually the one you most need
//! a stack from, because nobody expected it to misbehave.
//!
//! # What this deliberately will not do
//!
//! - **Never elevates.** No `sudo`, no privilege escalation, no re-exec as
//!   another user. If OS policy refuses, that is the answer.
//! - **Never writes `ptrace_scope`.** It is *read* to explain a refusal, and
//!   only ever read. A diagnostic tool that quietly widens a system-wide
//!   security control to get its job done has made a decision that was not
//!   its to make.
//! - **Never sets `PR_SET_PTRACER` for a target it does not own.** That call
//!   grants tracing of *the caller*, so it only helps when the caller is the
//!   process being traced. For an unrelated target it does nothing, and
//!   pretending otherwise would produce a confident failure.
//!
//! # PID reuse is checked on both sides of the capture
//!
//! A pid is not an identity — the OS recycles them, and a process that exits
//! mid-capture can have its number reused before the capture finishes. So the
//! target's start time is recorded before and re-checked after. A mismatch
//! means the artifact describes some other process, and an unnoticed dump of
//! the wrong process is worse than no dump: it sends the reader somewhere
//! confidently wrong.

use std::path::{Path, PathBuf};

/// What kind of target this is, which decides how to capture it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    /// A CPython process. Needs interpreter frames *and* native frames.
    Python,
    /// Anything else.
    Native,
}

/// How a target will be captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vehicle {
    /// `py-spy dump --pid` — interpreter frames, already symbolized.
    PySpy,
    /// `gdb -p` / `lldb -p` batch all-thread backtraces.
    NativeDebugger,
    /// ProcDump full minidump.
    ProcDump,
    /// `gcore` — a core file an external gdb can open post-mortem.
    Gcore,
}

/// Why a forced capture was refused.
///
/// A typed refusal rather than a generic error: the caller has to be able to
/// tell "this host forbids it" from "the tool is missing" from "you asked
/// about a process that no longer exists", because the remedies differ and
/// only one of them is the operator's to act on.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForceDenied {
    /// The OS refused to inspect the target.
    #[error("cannot capture pid {pid}: {reason}\n{remediation}")]
    PolicyDenied {
        /// Target process id.
        pid: u32,
        /// What the OS said, in its own terms.
        reason: String,
        /// What the operator can actually do about it.
        remediation: String,
    },
    /// The capture tool is not installed.
    #[error("cannot capture pid {pid}: {tool} is not installed\n{remediation}")]
    ToolMissing {
        /// Target process id.
        pid: u32,
        /// Tool that was looked for.
        tool: &'static str,
        /// How to get it.
        remediation: String,
    },
    /// The pid was reused between the start and end of the capture.
    #[error(
        "pid {pid} was reused during capture (start time {before} became {after}); \
         the artifact would describe a different process and has been discarded"
    )]
    PidReused {
        /// Target process id.
        pid: u32,
        /// Start time observed before capture.
        before: u64,
        /// Start time observed after.
        after: u64,
    },
    /// The target vanished.
    #[error("pid {pid} is not running")]
    NotRunning {
        /// Target process id.
        pid: u32,
    },
}

/// Choose the capture vehicles for a target.
///
/// Python targets get **two**: py-spy for interpreter frames and a native
/// debugger for the frames beneath them. Either alone is a half-answer — a
/// Python process wedged in a C extension shows nothing useful in py-spy, and
/// a native backtrace of CPython is a wall of `_PyEval_EvalFrameDefault`.
pub fn vehicles(runtime: Runtime, os: &str) -> Vec<Vehicle> {
    let native = if os == "windows" {
        Vehicle::ProcDump
    } else {
        Vehicle::NativeDebugger
    };
    match runtime {
        Runtime::Python => vec![Vehicle::PySpy, native],
        Runtime::Native => vec![native],
    }
}

/// The command that produces an artifact an external debugger can open.
///
/// Deliberately not a gdbserver. Embedding one would put a remote debugging
/// port on the host and make this tool responsible for its lifetime and its
/// access control; producing a file and telling the operator how to open it
/// keeps both where they already are.
pub fn openable_artifact_command(pid: u32, os: &str, out: &Path) -> Option<(String, Vec<String>)> {
    match os {
        "linux" => Some((
            "gcore".to_string(),
            vec!["-o".into(), out.display().to_string(), pid.to_string()],
        )),
        "windows" => Some((
            "procdump".to_string(),
            vec![
                "-accepteula".into(),
                "-ma".into(),
                pid.to_string(),
                out.display().to_string(),
            ],
        )),
        // macOS has no packaged equivalent that works without disabling SIP,
        // so there is nothing honest to offer here.
        _ => None,
    }
}

/// Build the debugger invocation for an all-thread backtrace.
pub fn debugger_command(tool: &str, pid: u32) -> (String, Vec<String>) {
    match tool {
        "lldb" => (
            "lldb".to_string(),
            vec![
                "-p".into(),
                pid.to_string(),
                "--batch".into(),
                "-o".into(),
                "thread backtrace all".into(),
                "-o".into(),
                "detach".into(),
                "-o".into(),
                "quit".into(),
            ],
        ),
        _ => (
            "gdb".to_string(),
            vec![
                "-p".into(),
                pid.to_string(),
                "-batch".into(),
                "-ex".into(),
                "thread apply all bt".into(),
            ],
        ),
    }
}

/// Read Linux's Yama `ptrace_scope`, if this host has one.
///
/// Read-only, always. The value explains why an attach was refused; changing
/// it is a system-wide security decision that belongs to whoever administers
/// the host, not to a tool that wants a stack trace.
pub fn yama_ptrace_scope() -> Option<u8> {
    std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Explain a refused attach in terms of the host's actual configuration.
pub fn yama_remediation(scope: Option<u8>) -> String {
    match scope {
        Some(0) => "Yama ptrace_scope is 0, so this refusal is not Yama — check that you own \
                    the target process and that no LSM (SELinux, AppArmor) is blocking it."
            .to_string(),
        Some(1) => "Yama ptrace_scope is 1: only a parent may attach to a process. Either run \
                    the target as a child of this tool, or have the target call \
                    prctl(PR_SET_PTRACER_ANY) itself. Raising the limit system-wide is an \
                    administrator's decision and this tool will not make it for you."
            .to_string(),
        Some(2) => "Yama ptrace_scope is 2: attaching requires CAP_SYS_PTRACE.".to_string(),
        Some(3) => "Yama ptrace_scope is 3: attaching is disabled entirely on this host and \
                    cannot be re-enabled without a reboot."
            .to_string(),
        Some(other) => format!("Yama ptrace_scope is {other}, an unrecognized value."),
        None => "This host has no Yama ptrace_scope; the refusal came from somewhere else \
                 (process ownership, a container's seccomp policy, or an LSM)."
            .to_string(),
    }
}

/// Copy-paste instructions for opening an artifact, or attaching live.
pub fn attach_instructions(
    pid: u32,
    os: &str,
    exe: Option<&Path>,
    artifact: Option<&Path>,
) -> String {
    let mut out = String::new();
    let exe_text = exe
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<executable>".to_string());

    if let Some(artifact) = artifact {
        out.push_str("Open the captured artifact:\n");
        match os {
            "windows" => out.push_str(&format!("  windbg -z {}\n", artifact.display())),
            _ => out.push_str(&format!("  gdb {} {}\n", exe_text, artifact.display())),
        }
        // Post-mortem first, deliberately: it needs no permission at all and
        // works after the target is gone, which live attach does not.
        out.push_str("\nThis works after the target has exited, and needs no attach permission.\n");
    }

    out.push_str("\nOr attach to the live process:\n");
    match os {
        "windows" => out.push_str(&format!("  windbg -p {pid}\n")),
        "macos" => out.push_str(&format!("  lldb -p {pid}\n")),
        _ => {
            out.push_str(&format!("  gdb -p {pid}\n"));
            out.push_str(&format!("\n{}\n", yama_remediation(yama_ptrace_scope())));
        }
    }
    out
}

/// Confirm the pid still refers to the same process instance.
///
/// Called with the start time captured *before* the dump. See the module docs
/// on why a mismatch discards the artifact rather than annotating it.
pub fn verify_not_reused(pid: u32, before: u64, after: Option<u64>) -> Result<(), ForceDenied> {
    match after {
        None => Err(ForceDenied::NotRunning { pid }),
        Some(after) if after != before => Err(ForceDenied::PidReused { pid, before, after }),
        Some(_) => Ok(()),
    }
}

/// Classify an OS error from a capture attempt.
pub fn classify_denial(pid: u32, error: &std::io::Error, os: &str) -> ForceDenied {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => ForceDenied::ToolMissing {
            pid,
            tool: "the capture tool",
            remediation: match os {
                "windows" => "Install ProcDump from Sysinternals and put it on PATH.".to_string(),
                "macos" => "Install the Xcode command line tools, which provide lldb.".to_string(),
                _ => "Install gdb (or lldb), and gcore for post-mortem artifacts.".to_string(),
            },
        },
        ErrorKind::PermissionDenied => ForceDenied::PolicyDenied {
            pid,
            reason: error.to_string(),
            remediation: if os == "linux" {
                yama_remediation(yama_ptrace_scope())
            } else {
                "The OS refused to inspect this process. This tool will not elevate; run it \
                 as the target's owner, or capture from a session that already has the \
                 rights."
                    .to_string()
            },
        },
        _ => ForceDenied::PolicyDenied {
            pid,
            reason: error.to_string(),
            remediation: "The capture tool failed for a reason this tool cannot classify; its \
                          own output above is the best guide."
                .to_string(),
        },
    }
}

/// Create the artifact directory with owner-only permissions.
///
/// A forced dump contains another process's memory. Leaving it
/// world-readable in a shared temp directory would hand out exactly what the
/// rest of this design spends its effort gating.
pub fn owner_private_dir(base: &Path) -> std::io::Result<PathBuf> {
    let dir = base.join(format!("rpprobe-force-{}", std::process::id()));
    running_process::broker::secure_dir::ensure_private_dir(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_python_target_gets_both_interpreter_and_native_vehicles() {
        // Either alone is a half-answer: py-spy shows nothing useful when the
        // process is wedged in a C extension, and a native backtrace of
        // CPython is a wall of `_PyEval_EvalFrameDefault`.
        assert_eq!(
            vehicles(Runtime::Python, "linux"),
            vec![Vehicle::PySpy, Vehicle::NativeDebugger]
        );
        assert_eq!(
            vehicles(Runtime::Python, "windows"),
            vec![Vehicle::PySpy, Vehicle::ProcDump]
        );
    }

    #[test]
    fn a_native_target_gets_the_platform_debugger_only() {
        assert_eq!(
            vehicles(Runtime::Native, "linux"),
            vec![Vehicle::NativeDebugger]
        );
        assert_eq!(
            vehicles(Runtime::Native, "macos"),
            vec![Vehicle::NativeDebugger]
        );
        assert_eq!(
            vehicles(Runtime::Native, "windows"),
            vec![Vehicle::ProcDump]
        );
    }

    #[test]
    fn pid_reuse_discards_the_capture() {
        // The artifact would describe a different process, and an unnoticed
        // dump of the wrong process sends the reader somewhere confidently
        // wrong — worse than no dump.
        let err = verify_not_reused(42, 1000, Some(2000)).expect_err("reuse must be caught");
        assert_eq!(
            err,
            ForceDenied::PidReused {
                pid: 42,
                before: 1000,
                after: 2000
            }
        );
        assert!(err.to_string().contains("discarded"));
    }

    #[test]
    fn an_unchanged_start_time_passes() {
        assert!(verify_not_reused(42, 1000, Some(1000)).is_ok());
    }

    #[test]
    fn a_vanished_target_is_distinguished_from_reuse() {
        // Different remedies: one means "it exited", the other means "your
        // artifact is about someone else".
        assert_eq!(
            verify_not_reused(42, 1000, None).expect_err("must fail"),
            ForceDenied::NotRunning { pid: 42 }
        );
    }

    #[test]
    fn every_yama_scope_gets_an_explanation_and_none_suggest_changing_it() {
        for scope in [None, Some(0), Some(1), Some(2), Some(3), Some(9)] {
            let text = yama_remediation(scope);
            assert!(!text.is_empty(), "scope {scope:?} has no explanation");
            // The invariant that matters: this tool never tells an operator to
            // widen a system-wide security control, and never does it itself.
            let lowered = text.to_lowercase();
            assert!(
                !lowered.contains("echo 0 >") && !lowered.contains("sysctl -w"),
                "scope {scope:?} suggests weakening ptrace_scope: {text}"
            );
            assert!(
                !lowered.contains("sudo"),
                "scope {scope:?} suggests sudo: {text}"
            );
        }
    }

    #[test]
    fn scope_one_explains_the_parent_rule_rather_than_how_to_disable_it() {
        let text = yama_remediation(Some(1));
        assert!(text.contains("only a parent may attach"));
        assert!(text.contains("PR_SET_PTRACER_ANY"));
        assert!(text.contains("administrator"));
    }

    #[test]
    fn attach_instructions_lead_with_the_post_mortem_route() {
        // It needs no permission and works after the target is gone, which
        // live attach does not.
        let artifact = PathBuf::from("/tmp/core.1234");
        let exe = PathBuf::from("/usr/bin/app");
        let text = attach_instructions(1234, "linux", Some(&exe), Some(&artifact));
        let open_at = text.find("gdb /usr/bin/app").expect("post-mortem command");
        let attach_at = text.find("gdb -p 1234").expect("live attach command");
        assert!(open_at < attach_at, "post-mortem must come first");
        assert!(text.contains("needs no attach permission"));
    }

    #[test]
    fn windows_instructions_use_windbg_not_gdb() {
        let text = attach_instructions(7, "windows", None, Some(Path::new("C:/dump.dmp")));
        assert!(text.contains("windbg -z C:/dump.dmp"));
        assert!(text.contains("windbg -p 7"));
        assert!(!text.contains("gdb"));
    }

    #[test]
    fn a_missing_tool_is_not_reported_as_a_policy_refusal() {
        // Different remedies: install something, versus you are not permitted.
        // Collapsing them sends the operator to argue with their sysadmin
        // about a missing package.
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        match classify_denial(9, &error, "windows") {
            ForceDenied::ToolMissing { remediation, .. } => {
                assert!(remediation.contains("ProcDump"));
            }
            other => panic!("expected ToolMissing, got {other:?}"),
        }
    }

    #[test]
    fn a_permission_error_carries_actionable_remediation_and_never_offers_elevation() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        match classify_denial(9, &error, "macos") {
            ForceDenied::PolicyDenied { remediation, .. } => {
                assert!(remediation.contains("will not elevate"));
                assert!(!remediation.to_lowercase().contains("sudo "));
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[test]
    fn the_openable_artifact_is_a_file_not_a_debug_server() {
        // Embedding a gdbserver would put a remote debugging port on the host
        // and make this tool responsible for its lifetime and access control.
        let out = PathBuf::from("/tmp/art");
        let (tool, args) = openable_artifact_command(5, "linux", &out).expect("linux artifact");
        assert_eq!(tool, "gcore");
        assert!(args.contains(&"5".to_string()));

        let (tool, _) = openable_artifact_command(5, "windows", &out).expect("windows artifact");
        assert_eq!(tool, "procdump");

        // macOS has no packaged equivalent that works without disabling SIP,
        // so nothing is offered rather than something that will not work.
        assert!(openable_artifact_command(5, "macos", &out).is_none());
    }

    #[test]
    fn debugger_commands_are_batch_and_detach() {
        // An interactive debugger left attached to a production process is a
        // worse outcome than the hang being diagnosed.
        let (tool, args) = debugger_command("gdb", 11);
        assert_eq!(tool, "gdb");
        assert!(args.contains(&"-batch".to_string()));
        assert!(args.contains(&"thread apply all bt".to_string()));

        let (tool, args) = debugger_command("lldb", 11);
        assert_eq!(tool, "lldb");
        assert!(args.contains(&"--batch".to_string()));
        assert!(args.contains(&"detach".to_string()));
    }
}
