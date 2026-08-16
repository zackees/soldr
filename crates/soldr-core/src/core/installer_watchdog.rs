//! Progress-based supervision for long-running installer subprocesses.
//!
//! Installer downloads and source builds can legitimately take much longer
//! than a fixed wall-clock deadline.  This module therefore fails only when a
//! child has stopped making observable progress, while retaining a large,
//! configurable safety ceiling for genuinely runaway commands.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

use super::SoldrError;

/// Environment variable for the maximum time an installer may be quiet.
pub const INSTALLER_STALL_TIMEOUT_ENV_VAR: &str = "SOLDR_INSTALLER_STALL_TIMEOUT_SECS";
/// Environment variable for the maximum total installer runtime.
pub const INSTALLER_SAFETY_TIMEOUT_ENV_VAR: &str = "SOLDR_INSTALLER_SAFETY_TIMEOUT_SECS";
/// Default quiet-period watchdog. Output and CPU activity reset this timer.
pub const DEFAULT_INSTALLER_STALL_TIMEOUT_SECS: u64 = 15 * 60;
/// Deliberately high runaway-process backstop. It is not a normal installer
/// deadline; active work may continue up to this ceiling.
pub const DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const KILLED_INSTALLER_REAP_TIMEOUT_SECS: u64 = 5;
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Effective bounds for one installer subprocess.
#[derive(Debug, Clone)]
pub struct InstallerWatchdogConfig {
    /// Maximum duration with no output or observed CPU activity.
    pub stall_timeout: Duration,
    /// Absolute, opt-in-adjustable upper bound for a runaway process.
    pub safety_timeout: Duration,
    safety_timeout_env_var: &'static str,
    poll_interval: Duration,
}

impl InstallerWatchdogConfig {
    /// Resolve the common watchdog configuration.
    ///
    /// `explicit_safety_timeout_env_var` is a legacy, operation-specific
    /// setting. When present and valid, it remains an explicit hard ceiling so
    /// existing automation keeps its requested bound. Without it, the shared
    /// 24-hour ceiling is used.
    pub fn from_env(explicit_safety_timeout_env_var: &'static str) -> Self {
        let (safety_timeout, safety_timeout_env_var) =
            match positive_env_duration(explicit_safety_timeout_env_var) {
                Some(timeout) => (timeout, explicit_safety_timeout_env_var),
                None => (installer_safety_timeout(), INSTALLER_SAFETY_TIMEOUT_ENV_VAR),
            };
        Self {
            stall_timeout: installer_stall_timeout(),
            safety_timeout,
            safety_timeout_env_var,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn for_test(stall_timeout: Duration, safety_timeout: Duration) -> Self {
        Self {
            stall_timeout,
            safety_timeout,
            safety_timeout_env_var: INSTALLER_SAFETY_TIMEOUT_ENV_VAR,
            poll_interval: Duration::from_millis(10),
        }
    }
}

/// Resolve the shared no-progress deadline.
pub fn installer_stall_timeout() -> Duration {
    positive_env_duration(INSTALLER_STALL_TIMEOUT_ENV_VAR)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_INSTALLER_STALL_TIMEOUT_SECS))
}

/// Resolve the shared maximum-runtime safety ceiling.
pub fn installer_safety_timeout() -> Duration {
    positive_env_duration(INSTALLER_SAFETY_TIMEOUT_ENV_VAR)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS))
}

fn positive_env_duration(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

/// Spawn and supervise a long-running installer while preserving its stdout
/// and stderr exactly as live terminal output.
pub fn run_installer_command(
    command: &mut Command,
    context: &str,
    phase: &str,
    config: InstallerWatchdogConfig,
) -> Result<ExitStatus, SoldrError> {
    configure_installer_process_tree(command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("failed to invoke {context}: {err}")))?;
    let (progress_tx, progress_rx) = mpsc::channel();
    let stdout_reader = child
        .stdout
        .take()
        .map(|pipe| tee_pipe_async(pipe, std::io::stdout(), progress_tx.clone()));
    let stderr_reader = child
        .stderr
        .take()
        .map(|pipe| tee_pipe_async(pipe, std::io::stderr(), progress_tx));

    let cpu_probe = ProcessTreeCpuProbe::new(child.id());
    let result =
        wait_for_child_with_watchdog(&mut child, context, phase, &config, &progress_rx, cpu_probe);
    match result {
        Ok(status) => {
            wait_for_pipe_drain(stdout_reader, context, "stdout")?;
            wait_for_pipe_drain(stderr_reader, context, "stderr")?;
            Ok(status)
        }
        // A killed installer can leave a descendant holding a copied pipe
        // descriptor. Do not turn a diagnosed watchdog failure into an
        // unbounded join on that descendant; dropping the handles detaches the
        // forwarding threads and returns the useful error immediately.
        Err(error) => Err(error),
    }
}

fn tee_pipe_async<R, W>(
    mut pipe: R,
    mut terminal: W,
    progress: mpsc::Sender<()>,
) -> mpsc::Receiver<std::io::Result<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (done_tx, done_rx) = mpsc::channel();
    let _ = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let result = (|| loop {
            let read = pipe.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            terminal.write_all(&buffer[..read])?;
            terminal.flush()?;
            // The receiver intentionally does not care which pipe moved: any
            // byte from the child is progress.
            let _ = progress.send(());
        })();
        let _ = done_tx.send(result);
    });
    done_rx
}

fn wait_for_pipe_drain(
    reader: Option<mpsc::Receiver<std::io::Result<()>>>,
    context: &str,
    pipe_name: &str,
) -> Result<(), SoldrError> {
    match reader {
        Some(receiver) => receiver
            .recv_timeout(PIPE_DRAIN_TIMEOUT)
            .map_err(|_| {
                SoldrError::Other(format!(
                    "{context}: {pipe_name} remained open for {} seconds after the installer exited; a descendant may still be running",
                    PIPE_DRAIN_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|err| {
                SoldrError::Other(format!(
                    "failed to forward {pipe_name} from {context}: {err}"
                ))
            }),
        None => Ok(()),
    }
}

trait ProgressProbe {
    fn made_progress(&mut self) -> bool;
}

struct ProcessTreeCpuProbe {
    root_pid: u32,
    previous_ticks: Option<u64>,
}

impl ProcessTreeCpuProbe {
    fn new(root_pid: u32) -> Self {
        Self {
            root_pid,
            previous_ticks: crate::platform::process::cpu_ticks::process_tree_cpu_ticks(root_pid),
        }
    }
}

impl ProgressProbe for ProcessTreeCpuProbe {
    fn made_progress(&mut self) -> bool {
        let Some(current_ticks) =
            crate::platform::process::cpu_ticks::process_tree_cpu_ticks(self.root_pid)
        else {
            return false;
        };
        let progressed = self
            .previous_ticks
            .is_some_and(|previous_ticks| current_ticks > previous_ticks);
        self.previous_ticks = Some(current_ticks);
        progressed
    }
}

fn wait_for_child_with_watchdog<P: ProgressProbe>(
    child: &mut Child,
    context: &str,
    phase: &str,
    config: &InstallerWatchdogConfig,
    progress_rx: &mpsc::Receiver<()>,
    mut probe: P,
) -> Result<ExitStatus, SoldrError> {
    let started = Instant::now();
    let mut last_progress = started;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
        {
            return Ok(status);
        }

        while progress_rx.try_recv().is_ok() {
            last_progress = Instant::now();
        }
        if probe.made_progress() {
            last_progress = Instant::now();
        }

        let now = Instant::now();
        if now.duration_since(started) >= config.safety_timeout {
            return Err(kill_for_watchdog(
                child,
                context,
                phase,
                "safety-ceiling",
                now.duration_since(started),
                now.duration_since(last_progress),
                config,
            ));
        }
        if now.duration_since(last_progress) >= config.stall_timeout {
            return Err(kill_for_watchdog(
                child,
                context,
                phase,
                "stall",
                now.duration_since(started),
                now.duration_since(last_progress),
                config,
            ));
        }
        thread::sleep(config.poll_interval);
    }
}

fn kill_for_watchdog(
    child: &mut Child,
    context: &str,
    phase: &str,
    category: &str,
    elapsed: Duration,
    since_progress: Duration,
    config: &InstallerWatchdogConfig,
) -> SoldrError {
    let kill_result = kill_installer_process_tree(child);
    let reap_result = child.wait_timeout(Duration::from_secs(KILLED_INSTALLER_REAP_TIMEOUT_SECS));
    let mut message = format!(
        "{context}: installer watchdog category={category} phase={phase} total_elapsed={}s since_progress={}s; stall_timeout={}s ({INSTALLER_STALL_TIMEOUT_ENV_VAR}); safety_ceiling={}s ({})",
        elapsed.as_secs(),
        since_progress.as_secs(),
        config.stall_timeout.as_secs(),
        config.safety_timeout.as_secs(),
        config.safety_timeout_env_var,
    );
    match kill_result {
        Ok(detail) => message.push_str(&format!("; {detail}")),
        Err(err) => message.push_str(&format!("; kill failed: {err}")),
    }
    match reap_result {
        Ok(Some(_)) => {}
        Ok(None) => message.push_str(&format!(
            "; process did not exit within {KILLED_INSTALLER_REAP_TIMEOUT_SECS} seconds after kill"
        )),
        Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
    }
    SoldrError::Other(message)
}

fn kill_installer_process_tree(child: &mut Child) -> std::io::Result<&'static str> {
    use crate::platform::process::terminate::TreeKill;
    match crate::platform::process::terminate::terminate_tree(child)? {
        TreeKill::TreeKilled => Ok("killed installer process tree"),
        TreeKill::ProcessKilled => Ok("killed installer process"),
    }
}

fn configure_installer_process_tree(command: &mut Command) {
    crate::platform::process::command::configure_process_group(command);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_EXPLICIT_CEILING_ENV_VAR: &str = "SOLDR_TEST_INSTALLER_EXPLICIT_CEILING_SECS";

    struct NeverProgress;

    impl ProgressProbe for NeverProgress {
        fn made_progress(&mut self) -> bool {
            false
        }
    }

    struct AlwaysProgress;

    impl ProgressProbe for AlwaysProgress {
        fn made_progress(&mut self) -> bool {
            true
        }
    }

    fn is_windows_test_host() -> bool {
        crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
    }

    fn unix_shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn steady_progress_command() -> Command {
        if is_windows_test_host() {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "1..6 | ForEach-Object { Write-Output progress; Start-Sleep -Milliseconds 500 }",
            ]);
            command
        } else {
            unix_shell("i=0; while [ $i -lt 8 ]; do echo progress; sleep 0.10; i=$((i + 1)); done")
        }
    }

    fn quiet_wait_command() -> Command {
        if is_windows_test_host() {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 4",
            ]);
            command
        } else {
            unix_shell("sleep 0.3")
        }
    }

    // Windows budgets carry wide contention margins (soldr#2565): the old
    // 1s-progress-gap vs 1.5s-stall pairing left 0.5s of slack, and loaded
    // CI runners stretched a single Start-Sleep past it — a false stall
    // kill that flaked the target-run lanes. Every pairing below keeps at
    // least 1.5s between the event being asserted and the budget that
    // must not fire first.
    fn test_stall_timeout() -> Duration {
        if is_windows_test_host() {
            Duration::from_millis(2500)
        } else {
            Duration::from_millis(250)
        }
    }

    fn test_safety_timeout() -> Duration {
        if is_windows_test_host() {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(2)
        }
    }

    fn test_short_safety_timeout() -> Duration {
        if is_windows_test_host() {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(45)
        }
    }

    fn wait_for_test_child<P: ProgressProbe>(
        mut command: Command,
        config: InstallerWatchdogConfig,
        probe: P,
    ) -> Result<ExitStatus, SoldrError> {
        let mut child = command.spawn().unwrap();
        let (_sender, receiver) = mpsc::channel();
        wait_for_child_with_watchdog(
            &mut child,
            "test installer",
            "test",
            &config,
            &receiver,
            probe,
        )
    }

    #[test]
    fn steady_progress_can_outlast_the_old_deadline() {
        let mut command = steady_progress_command();
        let result = run_installer_command(
            &mut command,
            "test installer",
            "test",
            InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout()),
        );
        assert!(result.unwrap().success());
    }

    #[test]
    fn a_true_stall_reports_category_phase_and_elapsed_times() {
        let command = quiet_wait_command();
        let error = wait_for_test_child(
            command,
            InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout()),
            NeverProgress,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("category=stall"), "{error}");
        assert!(error.contains("phase=test"), "{error}");
        assert!(error.contains("total_elapsed="), "{error}");
        assert!(error.contains("since_progress="), "{error}");
    }

    #[test]
    fn quiet_active_work_does_not_false_stall() {
        let command = quiet_wait_command();
        let result = wait_for_test_child(
            command,
            InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout()),
            AlwaysProgress,
        );
        assert!(result.unwrap().success());
    }

    #[test]
    fn quiet_cpu_active_work_resets_the_watchdog() {
        // The CPU-ticks probe is Linux-only (procfs); other hosts have no
        // progress source beyond output, so the quiet-but-active scenario
        // does not exist there.
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
            return;
        }
        let mut command = unix_shell(
            "yes >/dev/null & worker=$!; sleep 0.2; kill \"$worker\"; wait \"$worker\" 2>/dev/null || true",
        );
        let result = run_installer_command(
            &mut command,
            "quiet compiler simulation",
            "compile",
            InstallerWatchdogConfig::for_test(test_stall_timeout(), test_safety_timeout()),
        );
        assert!(result.unwrap().success());
    }

    #[test]
    fn watchdog_terminates_the_installer_process_group() {
        // Asserts against /proc, which only exists on Linux.
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let descendant_pid_file = temp.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 30 & echo $! > \"$1\"; sleep 30",
                "watchdog-test",
            ])
            .arg(&descendant_pid_file);
        let error = run_installer_command(
            &mut command,
            "process tree test",
            "test",
            InstallerWatchdogConfig::for_test(Duration::from_millis(100), Duration::from_secs(1)),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("category=stall"), "{error}");
        let descendant_pid = std::fs::read_to_string(&descendant_pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        for _ in 0..50 {
            if !std::path::Path::new("/proc")
                .join(descendant_pid.to_string())
                .exists()
            {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("descendant {descendant_pid} survived the watchdog process-group kill");
    }

    #[test]
    fn safety_ceiling_wins_even_when_work_keeps_progressing() {
        let command = quiet_wait_command();
        let error = wait_for_test_child(
            command,
            InstallerWatchdogConfig::for_test(test_safety_timeout(), test_short_safety_timeout()),
            AlwaysProgress,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("category=safety-ceiling"), "{error}");
    }

    #[test]
    fn shared_safety_ceiling_applies_without_an_explicit_operation_limit() {
        let _lock = crate::test_util::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_shared = std::env::var_os(INSTALLER_SAFETY_TIMEOUT_ENV_VAR);
        let previous_explicit = std::env::var_os(TEST_EXPLICIT_CEILING_ENV_VAR);
        unsafe {
            std::env::set_var(INSTALLER_SAFETY_TIMEOUT_ENV_VAR, "123");
            std::env::remove_var(TEST_EXPLICIT_CEILING_ENV_VAR);
        }
        assert_eq!(
            InstallerWatchdogConfig::from_env(TEST_EXPLICIT_CEILING_ENV_VAR).safety_timeout,
            Duration::from_secs(123)
        );
        unsafe {
            match previous_shared {
                Some(value) => std::env::set_var(INSTALLER_SAFETY_TIMEOUT_ENV_VAR, value),
                None => std::env::remove_var(INSTALLER_SAFETY_TIMEOUT_ENV_VAR),
            }
            match previous_explicit {
                Some(value) => std::env::set_var(TEST_EXPLICIT_CEILING_ENV_VAR, value),
                None => std::env::remove_var(TEST_EXPLICIT_CEILING_ENV_VAR),
            }
        }
    }
}
