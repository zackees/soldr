use std::path::Path;

use crate::broker::{host_identity, manifest};
use crate::cleanup::json_escape;

/// One manifest verification finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFinding {
    /// Manifest path.
    pub path: std::path::PathBuf,
    /// Finding severity.
    pub severity: &'static str,
    /// Human-readable message.
    pub message: String,
}

/// Basic v1 verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Number of `.pb` entries scanned.
    pub scanned: usize,
    /// Findings generated during verification.
    pub findings: Vec<VerifyFinding>,
}

/// Run basic verification over the central registry.
pub fn run(registry_dir: &Path) -> VerifyReport {
    let current = host_identity::current();
    let entries = manifest::scan_central(registry_dir);
    let mut findings = Vec::new();
    let scanned = entries.len();

    for entry in entries {
        match entry.result {
            Ok(manifest) => {
                if let Some(host) = manifest.host.as_ref() {
                    if !host.machine_id.is_empty() && host.machine_id != current.machine_id {
                        findings.push(VerifyFinding {
                            path: entry.path.clone(),
                            severity: "stale",
                            message: "manifest belongs to another machine".to_string(),
                        });
                    }
                    if !host.boot_id.is_empty() && host.boot_id != current.boot_id {
                        findings.push(VerifyFinding {
                            path: entry.path.clone(),
                            severity: "stale",
                            message: "manifest belongs to a prior boot".to_string(),
                        });
                    }
                }
                if let Some(daemon) = manifest.current_daemon.as_ref() {
                    if !process_is_alive(daemon.pid) {
                        findings.push(VerifyFinding {
                            path: entry.path,
                            severity: "stale",
                            message: format!("daemon pid {} is not alive", daemon.pid),
                        });
                    }
                }
            }
            Err(err) => findings.push(VerifyFinding {
                path: entry.path,
                severity: "error",
                message: err.to_string(),
            }),
        }
    }

    VerifyReport { scanned, findings }
}

/// Render `running-process-cleanup verify --json`.
pub fn render_json(report: &VerifyReport) -> String {
    let findings = report
        .findings
        .iter()
        .map(|finding| {
            format!(
                "{{\"path\":\"{}\",\"severity\":\"{}\",\"message\":\"{}\"}}",
                json_escape(&finding.path.to_string_lossy()),
                finding.severity,
                json_escape(&finding.message)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"schema_version\":1,\"scanned\":{},\"findings\":[{}]}}",
        report.scanned, findings
    )
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let handle = unsafe {
        winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    unsafe {
        winapi::um::handleapi::CloseHandle(handle);
    }
    true
}
