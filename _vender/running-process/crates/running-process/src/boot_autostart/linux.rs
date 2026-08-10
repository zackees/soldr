//! Linux backend: systemd user unit at
//! `$XDG_CONFIG_HOME/systemd/user/runpm-daemon.service`.
//!
//! `install` writes the unit and runs `systemctl --user enable
//! runpm-daemon.service`. If `systemctl` is missing or fails (operator
//! is on a non-systemd Linux, or sd-bus is unavailable in this session)
//! the file is still written and the path is returned, with a warning.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{shell_quote_single, BootAutostartError, UnitPath};

/// Canonical filename. Must match `uninstall`.
const UNIT_FILENAME: &str = "runpm-daemon.service";

/// Render the systemd unit file body with `daemon_binary` baked in.
pub fn render_unit(daemon_binary: &Path) -> String {
    let bin = shell_quote_single(&daemon_binary.to_string_lossy());
    format!(
        "[Unit]\n\
         Description=runpm process supervisor (running-process daemon)\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} start\n\
         ExecStop={bin} stop\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
}

/// Resolve the unit-file path: `$XDG_CONFIG_HOME/systemd/user/runpm-daemon.service`
/// with a `~/.config/systemd/user/` fallback when `XDG_CONFIG_HOME` is unset.
pub fn unit_path() -> Result<PathBuf, BootAutostartError> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                BootAutostartError::Resolve("neither XDG_CONFIG_HOME nor HOME is set".into())
            })?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("systemd").join("user").join(UNIT_FILENAME))
}

pub fn install(daemon_binary: &Path) -> Result<UnitPath, BootAutostartError> {
    let path = unit_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_unit(daemon_binary))?;

    // Best-effort daemon-reload + enable. A non-zero status means the
    // file is written but the unit is not armed; the operator can run
    // `systemctl --user enable` later. We surface the failure as a
    // tracing warning rather than an error so the caller still gets the
    // path that was written.
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    match Command::new("systemctl")
        .args(["--user", "enable", UNIT_FILENAME])
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("warning: systemctl --user enable {UNIT_FILENAME} returned non-zero ({s:?})");
        }
        Err(e) => {
            eprintln!("warning: systemctl --user enable {UNIT_FILENAME} failed to spawn: {e}");
        }
    }

    Ok(UnitPath(path))
}

pub fn uninstall() -> Result<(), BootAutostartError> {
    let path = unit_path()?;
    let _ = Command::new("systemctl")
        .args(["--user", "disable", UNIT_FILENAME])
        .status();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}
