//! Windows backend: Task Scheduler ONLOGON task named `runpm-daemon`,
//! created via the `schtasks.exe` CLI. We deliberately do not write the
//! Task Scheduler XML directly — Microsoft's XML schema is verbose and
//! changes shape between Windows versions, while `schtasks /Create` is
//! stable from Windows 7 onward.
//!
//! `render_unit` returns the equivalent `schtasks` command line so the
//! fixture tests can assert on the flags without invoking the actual
//! Task Scheduler.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{BootAutostartError, UnitPath};

/// Stable task name. Must match `uninstall`.
const TASK_NAME: &str = "runpm-daemon";

/// Wrap `s` in CMD-style double quotes, escaping any embedded double
/// quote by doubling it. This is what `schtasks /TR` expects when the
/// command path or arguments contain spaces.
fn cmd_quote(s: &str) -> String {
    let escaped = s.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Render the `schtasks /Create` invocation we would run for
/// `daemon_binary`. The string mirrors the actual argv we pass to
/// `Command::new("schtasks")` in [`install`] so the fixture tests
/// exercise the same flags the real path uses.
pub fn render_unit(daemon_binary: &Path) -> String {
    let bin = daemon_binary.to_string_lossy();
    let tr = cmd_quote(&format!("{bin} start"));
    format!(
        "schtasks /Create /SC ONLOGON /TN {tn} /TR {tr} /RL HIGHEST /F",
        tn = cmd_quote(TASK_NAME),
    )
}

/// Stable identifier the CLI prints after a successful install. Not a
/// real filesystem path — Task Scheduler stores tasks in the registry —
/// but `UnitPath` is the contract, so we wrap the task name as a
/// pseudo-path the operator can grep in `schtasks /Query`.
pub fn unit_path() -> PathBuf {
    PathBuf::from(format!(r"\Task Scheduler\{TASK_NAME}"))
}

pub fn install(daemon_binary: &Path) -> Result<UnitPath, BootAutostartError> {
    let bin = daemon_binary.to_string_lossy().into_owned();
    let tr = format!("{bin} start");
    let status = Command::new("schtasks")
        .args([
            "/Create", "/SC", "ONLOGON", "/TN", TASK_NAME, "/TR", &tr, "/RL", "HIGHEST", "/F",
        ])
        .status()
        .map_err(|e| BootAutostartError::InitSystem(format!("schtasks /Create failed: {e}")))?;
    if !status.success() {
        return Err(BootAutostartError::InitSystem(format!(
            "schtasks /Create exited non-zero ({status})"
        )));
    }
    Ok(UnitPath(unit_path()))
}

pub fn uninstall() -> Result<(), BootAutostartError> {
    let status = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .status()
        .map_err(|e| BootAutostartError::InitSystem(format!("schtasks /Delete failed: {e}")))?;
    if !status.success() {
        // Missing task is fine — the operator's intent was "make sure
        // it's not installed", which is already satisfied.
        eprintln!("warning: schtasks /Delete returned non-zero ({status:?}) (already removed?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_quote_doubles_embedded_quotes() {
        assert_eq!(
            cmd_quote(r#"C:\path with "quotes"\runpm.exe"#),
            "\"C:\\path with \"\"quotes\"\"\\runpm.exe\""
        );
    }
}
