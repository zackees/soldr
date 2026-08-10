//! Per-OS boot autostart for the `runpm` daemon (Phase 4 of #222 — #427).
//!
//! Each OS implementation exposes the same trio:
//!   - `install(daemon_binary)` — write the unit/plist/task and arm the
//!     init system. Returns the unit path that was written.
//!   - `uninstall()` — disarm the init system and remove the unit.
//!   - `render_unit(daemon_binary)` — render the unit text without
//!     touching the filesystem. Used by fixture tests and by `install`.
//!
//! Backends:
//!   - Linux: systemd user unit at
//!     `$XDG_CONFIG_HOME/systemd/user/runpm-daemon.service`.
//!   - macOS: launchd user agent at
//!     `~/Library/LaunchAgents/com.zackees.runpm-daemon.plist`.
//!   - Windows: Task Scheduler ONLOGON task named `runpm-daemon` via
//!     the `schtasks` CLI.
//!
//! Tests never call `install` — they assert against `render_unit` output
//! to avoid mutating the runner's init system.

use std::fmt;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Typed wrapper around the path where the unit/plist/task was written.
/// Wrapped so callers can't accidentally pass it as a generic `PathBuf`
/// and lose the "this is the autostart artifact" intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitPath(pub PathBuf);

impl UnitPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl fmt::Display for UnitPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

/// Anything that can go wrong installing/uninstalling boot autostart.
#[derive(Debug)]
pub enum BootAutostartError {
    /// Could not resolve where to write the unit file.
    Resolve(String),
    /// Filesystem write/remove failed.
    Io(std::io::Error),
    /// The init-system CLI (`systemctl`, `launchctl`, `schtasks`) failed.
    /// Non-zero exit code does NOT propagate as `Io`; it lands here so
    /// callers can distinguish "we wrote the file but enabling failed"
    /// from "filesystem error".
    InitSystem(String),
    /// Compiled for an OS we do not have a backend for (defensive — the
    /// public install/uninstall functions are `cfg`-gated, so this only
    /// fires if someone bypasses the gate).
    Unsupported(String),
}

impl fmt::Display for BootAutostartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootAutostartError::Resolve(m) => write!(f, "could not resolve install path: {m}"),
            BootAutostartError::Io(e) => write!(f, "filesystem error: {e}"),
            BootAutostartError::InitSystem(m) => write!(f, "init system error: {m}"),
            BootAutostartError::Unsupported(os) => {
                write!(f, "boot autostart not supported on {os}")
            }
        }
    }
}

impl std::error::Error for BootAutostartError {}

impl From<std::io::Error> for BootAutostartError {
    fn from(e: std::io::Error) -> Self {
        BootAutostartError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Public entry points (cfg-dispatched)
// ---------------------------------------------------------------------------

/// Install boot autostart for the running-process daemon. Returns the
/// path where the unit/plist/task was written.
pub fn install(daemon_binary: &Path) -> Result<UnitPath, BootAutostartError> {
    #[cfg(target_os = "linux")]
    {
        linux::install(daemon_binary)
    }
    #[cfg(target_os = "macos")]
    {
        macos::install(daemon_binary)
    }
    #[cfg(target_os = "windows")]
    {
        windows::install(daemon_binary)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = daemon_binary;
        Err(BootAutostartError::Unsupported(
            std::env::consts::OS.to_string(),
        ))
    }
}

/// Uninstall boot autostart for the running-process daemon.
pub fn uninstall() -> Result<(), BootAutostartError> {
    #[cfg(target_os = "linux")]
    {
        linux::uninstall()
    }
    #[cfg(target_os = "macos")]
    {
        macos::uninstall()
    }
    #[cfg(target_os = "windows")]
    {
        windows::uninstall()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(BootAutostartError::Unsupported(
            std::env::consts::OS.to_string(),
        ))
    }
}

/// Render the unit/plist/task text for the current OS without touching
/// the filesystem. Test seam used by `tests/runpm_boot_autostart_fixtures.rs`.
pub fn render_unit(daemon_binary: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        linux::render_unit(daemon_binary)
    }
    #[cfg(target_os = "macos")]
    {
        macos::render_unit(daemon_binary)
    }
    #[cfg(target_os = "windows")]
    {
        windows::render_unit(daemon_binary)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = daemon_binary;
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Shared shell-quoting helper (Linux + macOS).
// ---------------------------------------------------------------------------

/// Wrap a string in POSIX single-quotes, escaping embedded single quotes
/// with the standard `'\''` dance. Safe for paths-with-spaces injected
/// into the systemd unit's `ExecStart` line or the macOS plist's
/// `ProgramArguments` array.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn shell_quote_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            // Close quote, escaped literal single, re-open quote.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Escape a string for inclusion inside an XML/plist text node. Only
/// `<`, `>`, `&`, `"`, and `'` need escaping for plist values.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_simple_path() {
        assert_eq!(shell_quote_single("/usr/bin/foo"), "'/usr/bin/foo'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        assert_eq!(shell_quote_single("o'malley"), "'o'\\''malley'");
    }

    #[test]
    fn xml_escape_handles_metacharacters() {
        assert_eq!(
            xml_escape("a<b&c>d\"e'f"),
            "a&lt;b&amp;c&gt;d&quot;e&apos;f"
        );
    }
}
