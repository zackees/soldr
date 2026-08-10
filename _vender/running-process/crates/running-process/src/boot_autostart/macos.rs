//! macOS backend: launchd user agent plist at
//! `~/Library/LaunchAgents/com.zackees.runpm-daemon.plist`.
//!
//! `install` writes the plist and runs
//! `launchctl bootstrap gui/<uid> <plist>`. We use `bootstrap`/`bootout`
//! (the modern launchd API since 10.10) over `load -w` so the install
//! survives a clean migration to launchd's per-domain bootstrap model;
//! the older `load -w` still works on every supported macOS but is
//! deprecated.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{xml_escape, BootAutostartError, UnitPath};

/// Reverse-DNS launchd label. Must match `uninstall`.
const LABEL: &str = "com.zackees.runpm-daemon";

/// `LaunchAgents` filename derived from the label.
const PLIST_FILENAME: &str = "com.zackees.runpm-daemon.plist";

/// Render the launchd plist body with `daemon_binary` baked in.
pub fn render_unit(daemon_binary: &Path) -> String {
    let bin = xml_escape(&daemon_binary.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>start</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#,
    )
}

/// Resolve the plist path: `$HOME/Library/LaunchAgents/<plist>`.
pub fn plist_path() -> Result<PathBuf, BootAutostartError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| BootAutostartError::Resolve("HOME is not set".into()))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(PLIST_FILENAME))
}

/// Read the current user's uid via `id -u`. We avoid `libc::getuid` so
/// the rest of the file stays portable enough for tests to compile on
/// non-macOS during cross-checking.
fn current_uid() -> Result<u32, BootAutostartError> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| BootAutostartError::InitSystem(format!("id -u failed: {e}")))?;
    if !out.status.success() {
        return Err(BootAutostartError::InitSystem(format!(
            "id -u exited non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.parse::<u32>()
        .map_err(|e| BootAutostartError::InitSystem(format!("could not parse uid {s:?}: {e}")))
}

pub fn install(daemon_binary: &Path) -> Result<UnitPath, BootAutostartError> {
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_unit(daemon_binary))?;

    // Best-effort bootstrap. If the modern bootstrap call is rejected
    // (older macOS without it), fall back to `launchctl load -w`.
    let uid = current_uid()?;
    let domain = format!("gui/{uid}");
    match Command::new("launchctl")
        .args(["bootstrap", &domain, &path.to_string_lossy()])
        .status()
    {
        Ok(s) if s.success() => {}
        _ => {
            // Fallback to the legacy interface; ignore non-zero so we
            // still report the written path.
            let _ = Command::new("launchctl")
                .args(["load", "-w", &path.to_string_lossy()])
                .status();
        }
    }

    Ok(UnitPath(path))
}

pub fn uninstall() -> Result<(), BootAutostartError> {
    let path = plist_path()?;
    let uid = current_uid()?;
    let target = format!("gui/{uid}/{LABEL}");
    // Modern API first, then fall back to legacy `unload`.
    let modern = Command::new("launchctl")
        .args(["bootout", &target])
        .status();
    if !matches!(modern, Ok(s) if s.success()) {
        let _ = Command::new("launchctl")
            .args(["unload", "-w", &path.to_string_lossy()])
            .status();
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
