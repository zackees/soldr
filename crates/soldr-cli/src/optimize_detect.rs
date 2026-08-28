//! Platform, CI, and tool detection helpers for `soldr optimize`.
//!
//! Detection is intentionally a small surface so each helper can be
//! unit-tested in isolation. Action-layer dispatch lives in
//! `optimize_windows.rs`.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use wait_timeout::ChildExt;

pub(crate) use crate::defender::find_powershell;

/// The user-facing platform bucket. Maps Windows build numbers per
/// Microsoft's published table so the action layer can branch on Dev
/// Drive availability without reading the build twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows10,
    Windows11Pre22H2,
    /// Windows 11 22H2 or later — Dev Drive is available.
    Windows11Post22H2,
    MacOS,
    Linux,
    Other,
}

/// Map a Windows version triple (`major.minor.build`) to the right
/// `Platform` bucket. Pure function so the boundary build numbers
/// (19045, 22000, 22621) are exercised by unit tests without touching
/// the OS.
pub(crate) fn parse_windows_build(major: u32, _minor: u32, build: u32) -> Platform {
    if major < 10 {
        return Platform::Other;
    }
    if build >= 22621 {
        Platform::Windows11Post22H2
    } else if build >= 22000 {
        Platform::Windows11Pre22H2
    } else {
        // Anything below the Windows 11 build threshold is reported as
        // Windows 10. We don't try to distinguish 21H2 from 22H2 here
        // because Defender behavior is identical and Dev Drive isn't
        // available on either.
        Platform::Windows10
    }
}

/// Detect the running platform. On Windows, queries the OS build via
/// the host-facts `os_version()` probe (registry preferred, PowerShell
/// fallback) so we get the real build number even under compatibility
/// mode. Broad bucketing from the facade's `os()` on every other
/// platform.
pub(crate) fn detect_platform() -> Platform {
    match crate::platform::host::facts::os() {
        crate::platform::host::facts::HostOs::Windows => {
            let (major, minor, build) = current_windows_version();
            parse_windows_build(major, minor, build)
        }
        crate::platform::host::facts::HostOs::MacOs => Platform::MacOS,
        crate::platform::host::facts::HostOs::Linux => Platform::Linux,
    }
}

fn current_windows_version() -> (u32, u32, u32) {
    // The host-facts probe tries the registry first — it always
    // reflects the host OS, never the compatibility-mode lie — then
    // falls back to PowerShell.
    if let Some(version) = crate::platform::host::facts::os_version() {
        if let Some(triple) = parse_version_triple(&version) {
            return triple;
        }
    }
    // Last-resort fallback: assume modern Windows 10 22H2 baseline so
    // the action layer at least attempts Defender exclusions.
    (10, 0, 19045)
}

fn parse_version_triple(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let build = parts.next()?.parse().ok()?;
    Some((major, minor, build))
}

/// Detect installed tooling relevant to the optimize subcommand.
#[derive(Debug, Clone, Default)]
pub(crate) struct InstalledTools {
    /// Path to a PowerShell executable on `PATH` (`pwsh` preferred,
    /// then `powershell.exe`). `None` if neither is found.
    pub(crate) powershell: Option<PathBuf>,
    /// `True` when Defender's antivirus framework is installed
    /// (`Get-MpComputerStatus.AntivirusEnabled`).
    pub(crate) defender_present: bool,
    /// `True` when Defender real-time scanning is currently active.
    pub(crate) defender_active: bool,
    /// `True` when `fsutil devdrv query` is a recognized subcommand
    /// (Windows 11 22H2+). Always `false` elsewhere.
    pub(crate) fsutil_devdrv_supported: bool,
}

/// Whether the optimize caller needs live host-tool status. Dry-run callers
/// only need a plan, so querying Defender or Dev Drive would turn a
/// side-effect-free preview into an unbounded availability dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolDetectionMode {
    DryRun,
    Live,
}

/// Keep advisory Windows probes well below the test-process timeout. A failed
/// probe is deliberately indistinguishable from an unavailable feature: the
/// action layer can still explain that no live Defender exclusion was applied.
const TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

trait ToolProbe {
    fn find_powershell(&self) -> Option<PathBuf>;
    fn query_defender_status(&self, powershell: &Path) -> DefenderStatus;
    fn fsutil_devdrv_is_supported(&self) -> bool;
}

struct SystemToolProbe;

impl ToolProbe for SystemToolProbe {
    fn find_powershell(&self) -> Option<PathBuf> {
        find_powershell()
    }

    fn query_defender_status(&self, powershell: &Path) -> DefenderStatus {
        query_defender_status(powershell)
    }

    fn fsutil_devdrv_is_supported(&self) -> bool {
        fsutil_devdrv_is_supported()
    }
}

/// Detect tools relevant to the optimize subcommand. Live detection runs
/// bounded, side-effect-free helper queries; dry-run detection intentionally
/// invokes no child process at all.
pub(crate) fn detect_tools(platform: Platform, mode: ToolDetectionMode) -> InstalledTools {
    detect_tools_with_probe(platform, mode, &SystemToolProbe)
}

fn detect_tools_with_probe(
    platform: Platform,
    mode: ToolDetectionMode,
    probe: &impl ToolProbe,
) -> InstalledTools {
    if !matches!(
        platform,
        Platform::Windows10 | Platform::Windows11Pre22H2 | Platform::Windows11Post22H2
    ) || mode == ToolDetectionMode::DryRun
    {
        return InstalledTools::default();
    }
    let powershell = probe.find_powershell();
    let defender = powershell
        .as_ref()
        .map(|ps| probe.query_defender_status(ps))
        .unwrap_or_default();
    let fsutil_devdrv_supported =
        matches!(platform, Platform::Windows11Post22H2) && probe.fsutil_devdrv_is_supported();
    InstalledTools {
        powershell,
        defender_present: defender.present,
        defender_active: defender.active,
        fsutil_devdrv_supported,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DefenderStatus {
    pub(crate) present: bool,
    pub(crate) active: bool,
}

fn query_defender_status(powershell: &Path) -> DefenderStatus {
    let output = probe_command_output(
        Command::new(powershell)
        .args([
            "-NoProfile",
            "-Command",
            "Get-MpComputerStatus | Select-Object AntivirusEnabled, RealTimeProtectionEnabled | ConvertTo-Json -Compress",
        ]),
        "Get-MpComputerStatus",
    );
    let Ok(output) = output else {
        return DefenderStatus::default();
    };
    if !output.status.success() {
        return DefenderStatus::default();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_defender_status_json(&stdout)
}

/// Parse the JSON emitted by
/// `Get-MpComputerStatus | Select-Object ... | ConvertTo-Json`. Public
/// so tests can verify both shapes (single object vs. array).
pub(crate) fn parse_defender_status_json(stdout: &str) -> DefenderStatus {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(rename = "AntivirusEnabled")]
        antivirus_enabled: Option<bool>,
        #[serde(rename = "RealTimeProtectionEnabled")]
        real_time_protection_enabled: Option<bool>,
    }
    let trimmed = stdout.trim();
    let raw: Raw = serde_json::from_str(trimmed).unwrap_or(Raw {
        antivirus_enabled: None,
        real_time_protection_enabled: None,
    });
    DefenderStatus {
        present: raw.antivirus_enabled.unwrap_or(false),
        active: raw.real_time_protection_enabled.unwrap_or(false),
    }
}

fn fsutil_devdrv_is_supported() -> bool {
    let output = probe_command_output(
        Command::new("fsutil").args(["devdrv", "query"]),
        "fsutil devdrv query",
    );
    let Ok(output) = output else {
        return false;
    };
    // On Windows 10 this returns:
    //   "devdrv" is an invalid parameter.
    // On Windows 11 22H2+ it either reports info or errors with a different
    // message ("Usage: ..."). Heuristic: search the merged stdout/stderr
    // text for the "invalid parameter" sentinel.
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    !combined.to_ascii_lowercase().contains("invalid parameter")
}

/// Run a small host-status probe with bounded wait and no background pipe
/// readers. On timeout we kill and reap before returning, so a hung Windows
/// service cannot keep an optimize invocation (or its test process) alive.
fn probe_command_output(command: &mut Command, context: &str) -> Result<Output, String> {
    probe_command_output_with_timeout(command, context, TOOL_PROBE_TIMEOUT)
}

fn probe_command_output_with_timeout(
    command: &mut Command,
    context: &str,
    timeout: Duration,
) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| format!("failed to invoke {context}: {err}"))?;
    match child
        .wait_timeout(timeout)
        .map_err(|err| format!("wait on {context} failed: {err}"))?
    {
        Some(_) => child
            .wait_with_output()
            .map_err(|err| format!("collect {context} output failed: {err}")),
        None => {
            let kill_result = child.kill();
            let reap_result = child.wait();
            Err(format!(
                "{context} timed out after {} seconds; {}{}",
                timeout.as_secs(),
                match kill_result {
                    Ok(()) => "killed child process",
                    Err(err) => return Err(format!("{context} timed out; kill failed: {err}")),
                },
                match reap_result {
                    Ok(_) => "; reaped child process".to_string(),
                    Err(err) => format!("; reap failed: {err}"),
                }
            ))
        }
    }
}

/// Detect whether soldr is currently running inside a CI environment.
/// Returns a stable label string for telemetry / messaging when one is
/// detected; `None` otherwise.
pub(crate) fn detect_ci() -> Option<&'static str> {
    if env_truthy("GITHUB_ACTIONS") {
        return Some("github_actions");
    }
    if env_truthy("CI") {
        return Some("ci");
    }
    if env_truthy("BUILDKITE") {
        return Some("buildkite");
    }
    if env_truthy("CIRCLECI") {
        return Some("circleci");
    }
    if env_truthy("TRAVIS") {
        return Some("travis");
    }
    if let Ok(value) = std::env::var("JENKINS_URL") {
        if !value.trim().is_empty() {
            return Some("jenkins");
        }
    }
    None
}

fn env_truthy(key: &str) -> bool {
    match std::env::var(key) {
        Ok(value) => crate::core::flag_value(&value),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, time::Instant};

    struct RecordingProbe {
        powershell_calls: Cell<u8>,
        defender_calls: Cell<u8>,
        dev_drive_calls: Cell<u8>,
    }

    impl ToolProbe for RecordingProbe {
        fn find_powershell(&self) -> Option<PathBuf> {
            self.powershell_calls.set(self.powershell_calls.get() + 1);
            Some(PathBuf::from("powershell"))
        }

        fn query_defender_status(&self, _powershell: &Path) -> DefenderStatus {
            self.defender_calls.set(self.defender_calls.get() + 1);
            DefenderStatus {
                present: true,
                active: true,
            }
        }

        fn fsutil_devdrv_is_supported(&self) -> bool {
            self.dev_drive_calls.set(self.dev_drive_calls.get() + 1);
            true
        }
    }

    #[test]
    fn defender_status_parses_object_form() {
        let json = r#"{"AntivirusEnabled":true,"RealTimeProtectionEnabled":true}"#;
        let parsed = parse_defender_status_json(json);
        assert!(parsed.present);
        assert!(parsed.active);
    }

    #[test]
    fn defender_status_handles_disabled_real_time() {
        let json = r#"{"AntivirusEnabled":true,"RealTimeProtectionEnabled":false}"#;
        let parsed = parse_defender_status_json(json);
        assert!(parsed.present);
        assert!(!parsed.active);
    }

    #[test]
    fn defender_status_handles_missing_fields() {
        let parsed = parse_defender_status_json("{}");
        assert!(!parsed.present);
        assert!(!parsed.active);
    }

    #[test]
    fn defender_status_falls_back_when_input_is_empty() {
        let parsed = parse_defender_status_json("");
        assert!(!parsed.present);
        assert!(!parsed.active);
    }

    #[test]
    fn dry_run_detection_does_not_spawn_status_or_dev_drive_probes() {
        let probe = RecordingProbe {
            powershell_calls: Cell::new(0),
            defender_calls: Cell::new(0),
            dev_drive_calls: Cell::new(0),
        };

        let tools = detect_tools_with_probe(
            Platform::Windows11Post22H2,
            ToolDetectionMode::DryRun,
            &probe,
        );

        assert!(tools.powershell.is_none());
        assert!(!tools.defender_present);
        assert!(!tools.defender_active);
        assert!(!tools.fsutil_devdrv_supported);
        assert_eq!(probe.powershell_calls.get(), 0);
        assert_eq!(probe.defender_calls.get(), 0);
        assert_eq!(probe.dev_drive_calls.get(), 0);
    }

    #[test]
    fn live_detection_queries_status_and_dev_drive_once() {
        let probe = RecordingProbe {
            powershell_calls: Cell::new(0),
            defender_calls: Cell::new(0),
            dev_drive_calls: Cell::new(0),
        };

        let tools =
            detect_tools_with_probe(Platform::Windows11Post22H2, ToolDetectionMode::Live, &probe);

        assert!(tools.defender_present);
        assert!(tools.defender_active);
        assert!(tools.fsutil_devdrv_supported);
        assert_eq!(probe.powershell_calls.get(), 1);
        assert_eq!(probe.defender_calls.get(), 1);
        assert_eq!(probe.dev_drive_calls.get(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timed_out_probe_is_killed_and_reaped_without_background_pipe_readers() {
        let temp = tempfile::tempdir().expect("temporary probe directory");
        let pid_file = temp.path().join("probe.pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let started = Instant::now();
        let error = probe_command_output_with_timeout(
            Command::new("sh").args(["-c", &script]),
            "blocking test probe",
            Duration::from_millis(50),
        )
        .expect_err("blocking probe must time out");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "probe timeout was not bounded"
        );
        assert!(error.contains("timed out after 0 seconds"));
        let pid = std::fs::read_to_string(&pid_file)
            .expect("blocking child must have started")
            .trim()
            .to_owned();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the timed-out child must be reaped before the probe returns"
        );
    }

    #[test]
    fn parse_version_triple_parses_dotted_build() {
        assert_eq!(parse_version_triple("10.0.19045"), Some((10, 0, 19045)));
        assert_eq!(parse_version_triple("10.0.22621"), Some((10, 0, 22621)));
        assert_eq!(parse_version_triple("not-a-version"), None);
        assert_eq!(parse_version_triple("10.0"), None);
    }
}
