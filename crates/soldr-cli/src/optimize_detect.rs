//! Platform, CI, and tool detection helpers for `soldr optimize`.
//!
//! Detection is intentionally a small surface so each helper can be
//! unit-tested in isolation. Action-layer dispatch lives in
//! `optimize_windows.rs`.

use std::path::PathBuf;

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
/// `RtlGetVersion` (preferred) so we get the real build number even
/// under compatibility mode. Falls back to broad bucketing from
/// `std::env::consts::OS` on every other platform.
pub(crate) fn detect_platform() -> Platform {
    match std::env::consts::OS {
        "windows" => {
            let (major, minor, build) = current_windows_version();
            parse_windows_build(major, minor, build)
        }
        "macos" => Platform::MacOS,
        "linux" => Platform::Linux,
        _ => Platform::Other,
    }
}

#[cfg(target_os = "windows")]
fn current_windows_version() -> (u32, u32, u32) {
    // Try the registry first — it always reflects the host OS, never
    // the compatibility-mode lie.
    if let Some(triple) = registry_windows_version() {
        return triple;
    }
    // Fall back to PowerShell. Slow but correct.
    if let Some(triple) = powershell_windows_version() {
        return triple;
    }
    // Last-resort fallback: assume modern Windows 10 22H2 baseline so
    // the action layer at least attempts Defender exclusions.
    (10, 0, 19045)
}

#[cfg(not(target_os = "windows"))]
fn current_windows_version() -> (u32, u32, u32) {
    (0, 0, 0)
}

#[cfg(target_os = "windows")]
fn registry_windows_version() -> Option<(u32, u32, u32)> {
    use std::process::Command;
    let output = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "/v",
            "CurrentBuildNumber",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let build: u32 = stdout
        .lines()
        .find_map(|line| line.split_whitespace().last())
        .and_then(|tok| tok.parse().ok())?;
    Some((10, 0, build))
}

#[cfg(target_os = "windows")]
fn powershell_windows_version() -> Option<(u32, u32, u32)> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[System.Environment]::OSVersion.Version | Select-Object -Property Major,Minor,Build | ConvertTo-Json -Compress",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_powershell_version_json(&stdout)
}

#[cfg(target_os = "windows")]
fn parse_powershell_version_json(stdout: &str) -> Option<(u32, u32, u32)> {
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(rename = "Major")]
        major: u32,
        #[serde(rename = "Minor")]
        minor: u32,
        #[serde(rename = "Build")]
        build: u32,
    }
    let trimmed = stdout.trim();
    let parsed: Version = serde_json::from_str(trimmed).ok()?;
    Some((parsed.major, parsed.minor, parsed.build))
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

/// Detect tools relevant to the optimize subcommand. Runs side-effect-
/// free helper queries; safe to call without elevation.
pub(crate) fn detect_tools(platform: Platform) -> InstalledTools {
    if !matches!(
        platform,
        Platform::Windows10 | Platform::Windows11Pre22H2 | Platform::Windows11Post22H2
    ) {
        return InstalledTools::default();
    }
    let powershell = find_powershell();
    let defender = powershell
        .as_ref()
        .map(|ps| query_defender_status(ps))
        .unwrap_or_default();
    let fsutil_devdrv_supported =
        matches!(platform, Platform::Windows11Post22H2) && fsutil_devdrv_is_supported();
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

fn query_defender_status(powershell: &std::path::Path) -> DefenderStatus {
    let output = std::process::Command::new(powershell)
        .args([
            "-NoProfile",
            "-Command",
            "Get-MpComputerStatus | Select-Object AntivirusEnabled, RealTimeProtectionEnabled | ConvertTo-Json -Compress",
        ])
        .output();
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
    let output = std::process::Command::new("fsutil")
        .args(["devdrv", "query"])
        .output();
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
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(target_os = "windows")]
    #[test]
    fn parse_windows_version_json_basic_shape() {
        let json = r#"{"Major":10,"Minor":0,"Build":19045}"#;
        assert_eq!(parse_powershell_version_json(json), Some((10, 0, 19045)));
    }
}
