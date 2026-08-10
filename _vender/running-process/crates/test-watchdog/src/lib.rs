//! Hang-watchdog for tests.
//!
//! Drop the returned [`WatchdogGuard`] before the timeout expires to cancel;
//! otherwise the watcher thread fires and:
//!
//! - **Windows**: invokes `procdump -ma <pid>` to write a full minidump
//!   (all threads, full memory), prints `message` plus the actual dump
//!   path, then `std::process::exit(1)`.
//! - **Unix (Linux/macOS)**: attaches an external debugger to the hung
//!   process — `gdb -p <pid> -batch -ex 'thread apply all bt'` (preferred
//!   on Linux) or `lldb -p <pid> --batch -o 'thread backtrace all'`
//!   (preferred on macOS), whichever is available — and prints every
//!   thread's backtrace to stderr (and writes it to `dump_path`), then
//!   `std::process::exit(1)`. Works for non-cooperative hangs (threads
//!   blocked in syscalls) because the dump is taken out-of-process. If no
//!   debugger is on PATH or the attach fails (e.g. Yama
//!   `ptrace_scope` restrictions), a one-line note is printed instead —
//!   a dump failure never masks the hang itself.
//! - **Other platforms**: no-op. The watcher thread is still spawned so
//!   the same API works cross-platform, but nothing happens on timeout.
//!
//! Cancel happens via channel-disconnect when the guard's `Sender` drops,
//! so normal returns AND panic unwinds both cancel cleanly.
//!
//! # Usage
//!
//! ```no_run
//! use std::time::Duration;
//!
//! #[test]
//! fn slow_test() {
//!     let _wd = test_watchdog::install(
//!         Duration::from_secs(30),
//!         "slow_test appears to be hung",
//!         None, // auto-generate dump path in env::temp_dir()
//!     );
//!     // ... test body ...
//! }
//! ```

use std::path::PathBuf;
use std::time::Duration;

/// Cancel handle for a watchdog. Drop it to cancel the watcher.
pub struct WatchdogGuard {
    // The watcher thread is the only consumer; dropping this Sender
    // produces a `RecvTimeoutError::Disconnected` which the thread
    // treats as "cancelled, exit silently".
    #[allow(dead_code)]
    inner: GuardInner,
}

enum GuardInner {
    #[allow(dead_code)]
    Noop,
    #[cfg(any(windows, unix))]
    #[allow(dead_code)]
    Active(std::sync::mpsc::Sender<()>),
}

/// Install a hang watchdog.
///
/// `timeout` — how long to wait before firing.
/// `message` — prefix printed to stderr when the watchdog fires (callers
/// pass something like `"<test name> appears to be hung"`).
/// `dump_path` — explicit dump location: a minidump on Windows, a text
/// file with all-thread backtraces on Unix. `None` auto-generates a path
/// in `env::temp_dir()`.
mod probe_dump;

pub fn install(
    timeout: Duration,
    message: impl Into<String>,
    dump_path: Option<PathBuf>,
) -> WatchdogGuard {
    #[cfg(any(windows, unix))]
    {
        install_active(timeout, message.into(), dump_path)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (timeout, message, dump_path);
        WatchdogGuard {
            inner: GuardInner::Noop,
        }
    }
}

#[cfg(any(windows, unix))]
fn install_active(timeout: Duration, message: String, dump_path: Option<PathBuf>) -> WatchdogGuard {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let pid = std::process::id();
    let dump_path = dump_path.unwrap_or_else(|| default_dump_path(pid));
    std::thread::Builder::new()
        .name("test-watchdog".to_string())
        .spawn(move || match rx.recv_timeout(timeout) {
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                fire(pid, timeout, &message, &dump_path);
            }
        })
        .expect("spawn watchdog thread");
    WatchdogGuard {
        inner: GuardInner::Active(tx),
    }
}

#[cfg(any(windows, unix))]
fn default_dump_path(pid: u32) -> PathBuf {
    // NOTE: the file name must not contain the substring `pid`.
    //
    // ProcDump treats `PID` in an output file name as a substitution token
    // and replaces it with the process id. Asking for
    // `test-watchdog-pid38972.dmp` therefore writes
    // `test-watchdog-3897238972.dmp` — the id, twice.
    //
    // `fire` already recovers by parsing procdump's `Dump 1 initiated:`
    // line, so the path it reports has always been correct. What silently
    // did not work was anything matching on the name it *asked* for: a CI
    // step archiving `test-watchdog-pid*` collected nothing, and a caller
    // that passes an explicit `dump_path` could not rely on that file
    // existing. Observed 2026-07-28 while chasing running-process#747,
    // where the missing artifact made an intermittent hang look
    // undiagnosable.
    #[cfg(windows)]
    let name = format!("test-watchdog-{pid}.dmp");
    // Unix has no such rewriting; kept in the same shape so both platforms
    // agree on where dumps land.
    #[cfg(unix)]
    let name = format!("test-watchdog-{pid}.backtrace.txt");
    std::env::temp_dir().join(name)
}

/// Which external debugger to use for the Unix all-thread backtrace dump.
///
/// Defined unconditionally (not `cfg(unix)`) so the command-construction
/// logic is unit-testable on every host platform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
enum Debugger {
    Gdb,
    Lldb,
}

/// Build the debugger invocation that attaches to `pid` and prints every
/// thread's backtrace, batch-mode (no interaction), then detaches/exits.
#[allow(dead_code)]
fn debugger_command(debugger: Debugger, pid: u32) -> (&'static str, Vec<String>) {
    let pid = pid.to_string();
    match debugger {
        Debugger::Gdb => (
            "gdb",
            vec![
                "-p".into(),
                pid,
                "-batch".into(),
                "-ex".into(),
                "set confirm off".into(),
                "-ex".into(),
                "thread apply all bt".into(),
            ],
        ),
        Debugger::Lldb => (
            "lldb",
            vec![
                "-p".into(),
                pid,
                "--batch".into(),
                "-o".into(),
                "thread backtrace all".into(),
                "-o".into(),
                "detach".into(),
            ],
        ),
    }
}

/// Probe PATH for an available debugger, preferring the platform-native
/// one (gdb on Linux, lldb on macOS) but accepting either.
#[cfg(unix)]
fn find_debugger() -> Option<Debugger> {
    #[cfg(target_os = "macos")]
    const PREFERENCE: [Debugger; 2] = [Debugger::Lldb, Debugger::Gdb];
    #[cfg(not(target_os = "macos"))]
    const PREFERENCE: [Debugger; 2] = [Debugger::Gdb, Debugger::Lldb];
    PREFERENCE.into_iter().find(|d| {
        let (prog, _) = debugger_command(*d, 0);
        std::process::Command::new(prog)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
    })
}

/// Dump every thread, then exit non-zero.
///
/// The probe is tried first (S19 / #649). It is cooperative and
/// in-process, so it needs no debugger installed, no attach permission,
/// and no second process — all three of which are routinely unavailable on
/// exactly the hosts where a hang has to be diagnosed. See
/// [`probe_dump`] for the specifics.
///
/// The external debugger stays as the fallback, because a cooperative
/// capture cannot dump a process too wedged to run its own code.
#[cfg(any(windows, unix))]
fn fire(pid: u32, after: Duration, message: &str, dump_path: &std::path::Path) -> ! {
    if let Some(report) = probe_dump::capture_self(after, message, dump_path) {
        eprintln!(
            "{message} — cooperative all-thread dump (no progress in {after:?}); \
             written to {}",
            dump_path.display()
        );
        eprintln!("{report}");
        std::process::exit(1);
    }

    eprintln!(
        "test-watchdog: cooperative capture unavailable here; \
         falling back to an external debugger"
    );
    fire_external(pid, after, message, dump_path)
}

#[cfg(windows)]
fn fire_external(pid: u32, after: Duration, message: &str, dump_path: &std::path::Path) -> ! {
    eprintln!(
        "{message} — stack dump because process appears to be hung \
         (no progress in {after:?}); invoking procdump on PID {pid}"
    );
    let output = std::process::Command::new("procdump")
        .args(["-accepteula", "-ma", &pid.to_string()])
        .arg(dump_path)
        .output();
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("procdump exit: {}", o.status);
            if !stdout.is_empty() {
                eprintln!("procdump stdout:\n{stdout}");
            }
            if !stderr.is_empty() {
                eprintln!("procdump stderr:\n{stderr}");
            }
            // procdump prints `Dump 1 initiated: <full_path>` — grab the
            // actual on-disk path (procdump sometimes rewrites the name
            // we asked for).
            let actual = stdout
                .lines()
                .find_map(|l| l.split_once("Dump 1 initiated:").map(|(_, p)| p.trim()))
                .map(PathBuf::from);
            match actual {
                Some(p) if p.exists() => {
                    eprintln!("minidump written: {}", p.display());
                    eprintln!("open in WinDbg / Visual Studio: `~* k` for all-thread callstacks");
                }
                Some(p) => {
                    eprintln!("procdump reported {} but file is missing", p.display());
                }
                None => {
                    eprintln!("could not parse dump path from procdump output");
                }
            }
        }
        Err(e) => {
            eprintln!("failed to invoke procdump: {e}");
            eprintln!(
                "(install procdump from sysinternals or place it on PATH \
                 to capture a thread dump on hang)"
            );
        }
    }
    std::process::exit(1);
}

#[cfg(unix)]
fn fire_external(pid: u32, after: Duration, message: &str, dump_path: &std::path::Path) -> ! {
    eprintln!(
        "{message} — stack dump because process appears to be hung \
         (no progress in {after:?}); attaching debugger to PID {pid}"
    );
    // On Linux with Yama `ptrace_scope = 1`, a process may only be traced
    // by its ancestors — and the debugger we spawn below is a *child* of
    // the hung process, so the attach would be refused. PR_SET_PTRACER
    // with PR_SET_PTRACER_ANY opts this process into being traceable by
    // anyone, which covers the child-debugger case. Best-effort: EINVAL
    // simply means Yama isn't loaded.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
    }
    match find_debugger() {
        Some(debugger) => {
            let (prog, args) = debugger_command(debugger, pid);
            eprintln!("invoking: {prog} {}", args.join(" "));
            match std::process::Command::new(prog).args(&args).output() {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    eprintln!("{prog} exit: {}", o.status);
                    if !stdout.is_empty() {
                        eprintln!("{prog} stdout (all-thread backtraces):\n{stdout}");
                    }
                    if !stderr.is_empty() {
                        eprintln!("{prog} stderr:\n{stderr}");
                    }
                    let report = format!(
                        "{message}\n{prog} exit: {}\n\n=== {prog} stdout ===\n{stdout}\n\
                         === {prog} stderr ===\n{stderr}\n",
                        o.status
                    );
                    match std::fs::write(dump_path, report) {
                        Ok(()) => eprintln!("backtrace dump written: {}", dump_path.display()),
                        Err(e) => eprintln!(
                            "could not write backtrace dump to {}: {e}",
                            dump_path.display()
                        ),
                    }
                    if !o.status.success()
                        && (stderr.contains("Operation not permitted") || stderr.contains("ptrace"))
                    {
                        eprintln!(
                            "(debugger attach appears to have been refused; on Linux \
                             check /proc/sys/kernel/yama/ptrace_scope — values > 0 \
                             restrict non-ancestor attach, and hardened kernels may \
                             ignore PR_SET_PTRACER)"
                        );
                    }
                }
                Err(e) => {
                    eprintln!("failed to invoke {prog}: {e}");
                }
            }
        }
        None => {
            eprintln!(
                "no debugger found on PATH (tried gdb and lldb); cannot capture \
                 thread backtraces — install gdb (Linux) or lldb (macOS)"
            );
        }
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdb_command_attaches_batch_and_dumps_all_threads() {
        let (prog, args) = debugger_command(Debugger::Gdb, 4242);
        assert_eq!(prog, "gdb");
        assert_eq!(
            args,
            vec![
                "-p",
                "4242",
                "-batch",
                "-ex",
                "set confirm off",
                "-ex",
                "thread apply all bt",
            ]
        );
    }

    #[test]
    fn lldb_command_attaches_batch_dumps_all_threads_and_detaches() {
        let (prog, args) = debugger_command(Debugger::Lldb, 7);
        assert_eq!(prog, "lldb");
        assert_eq!(
            args,
            vec![
                "-p",
                "7",
                "--batch",
                "-o",
                "thread backtrace all",
                "-o",
                "detach",
            ]
        );
    }

    #[test]
    fn dropping_guard_cancels_watchdog() {
        // Arm a very short watchdog and immediately cancel it by dropping
        // the guard. If cancellation were broken the watchdog would fire
        // and `std::process::exit(1)` the test binary before the sleep
        // finishes.
        let guard = install(
            Duration::from_millis(50),
            "dropping_guard_cancels_watchdog should never fire",
            None,
        );
        drop(guard);
        std::thread::sleep(Duration::from_millis(300));
        // Reaching this point proves the watchdog did not fire.
    }

    /// ProcDump substitutes `PID` in an output file name with the process
    /// id, so a name containing it is rewritten and the file lands somewhere
    /// the caller never asked for. Guarding the rule directly, because the
    /// symptom — a dump that exists but cannot be found by name — reads as
    /// "no dump was produced" and makes a hang look undiagnosable.
    #[test]
    fn the_default_dump_name_avoids_procdump_substitution_tokens() {
        let name = default_dump_path(38972)
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();

        assert!(
            !name.to_ascii_lowercase().contains("pid"),
            "{name} contains the `pid` token, which procdump rewrites"
        );
        assert!(
            name.contains("38972"),
            "{name} should still identify the process"
        );
        // The doubled-id shape the old name produced.
        assert!(
            !name.contains("3897238972"),
            "{name} looks like the id was substituted twice"
        );
    }
}
