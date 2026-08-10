//! systemd `KillMode=control-group` detection (#391, part of #354).
//!
//! When the daemon runs inside a systemd unit whose `KillMode` is
//! `control-group` (systemd's default), stopping the unit kills every
//! process in the unit's cgroup — including spawned children the daemon
//! expected to outlive it. At startup the daemon probes for this and emits
//! a WARN; `broker doctor` surfaces the same assessment.
//!
//! The decision logic ([`assess`]) is a pure function over
//! [`SystemdProbeInputs`] so it is testable on every platform with
//! simulated environments. Only [`probe`]'s input gathering is
//! Linux-specific; on other platforms it reports [`KillModeAssessment::NotSystemd`].

/// Inputs to the KillMode assessment, gathered by [`probe`] or injected
/// by tests.
#[derive(Clone, Debug, Default)]
pub struct SystemdProbeInputs {
    /// `$INVOCATION_ID` — set by systemd for managed services.
    pub invocation_id: Option<String>,
    /// Contents of `/proc/self/cgroup`.
    pub cgroup: Option<String>,
    /// Output of `systemctl show -p KillMode <unit>`, or the failure
    /// reason when systemctl is unavailable / errored. `None` when the
    /// query was never attempted (no unit resolved).
    pub kill_mode_query: Option<Result<String, String>>,
}

/// Result of the KillMode assessment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillModeAssessment {
    /// Not running under systemd; nothing to report.
    NotSystemd,
    /// systemd-managed with a KillMode that leaves spawned children alone.
    Safe {
        /// Owning unit name.
        unit: String,
        /// Reported KillMode value (e.g. `process`, `mixed`, `none`).
        kill_mode: String,
    },
    /// systemd-managed with `KillMode=control-group`: stopping the unit
    /// reaps every spawned child.
    ControlGroup {
        /// Owning unit name.
        unit: String,
    },
    /// systemd-managed but the KillMode could not be determined.
    Unknown {
        /// Owning unit name, when it could be resolved.
        unit: Option<String>,
        /// Why the KillMode is unknown.
        reason: String,
    },
}

impl KillModeAssessment {
    /// Startup warning message, `Some` only when operators should act:
    /// `KillMode=control-group`, or systemd-managed with an undetermined
    /// KillMode. Silent when not under systemd or when the KillMode is
    /// known-safe.
    pub fn warning(&self) -> Option<String> {
        match self {
            KillModeAssessment::NotSystemd | KillModeAssessment::Safe { .. } => None,
            KillModeAssessment::ControlGroup { unit } => Some(format!(
                "running under systemd unit {unit} with KillMode=control-group: stopping the \
                 unit will kill every spawned child process; set KillMode=process (or mixed) \
                 in the unit to let children outlive the daemon"
            )),
            KillModeAssessment::Unknown { unit, reason } => {
                let unit = unit.as_deref().unwrap_or("<unresolved>");
                Some(format!(
                    "running under systemd (unit {unit}) but KillMode could not be determined \
                     ({reason}); if the unit uses the default KillMode=control-group, stopping \
                     it will kill every spawned child process"
                ))
            }
        }
    }
}

/// Pure KillMode assessment over injected inputs.
pub fn assess(inputs: &SystemdProbeInputs) -> KillModeAssessment {
    let systemd_managed = inputs
        .invocation_id
        .as_deref()
        .map(|id| !id.trim().is_empty())
        .unwrap_or(false);
    if !systemd_managed {
        return KillModeAssessment::NotSystemd;
    }
    let unit = inputs.cgroup.as_deref().and_then(unit_from_cgroup);
    let Some(unit) = unit else {
        return KillModeAssessment::Unknown {
            unit: None,
            reason: "owning unit could not be resolved from /proc/self/cgroup".into(),
        };
    };
    match &inputs.kill_mode_query {
        None => KillModeAssessment::Unknown {
            unit: Some(unit),
            reason: "KillMode was not queried".into(),
        },
        Some(Err(err)) => KillModeAssessment::Unknown {
            unit: Some(unit),
            reason: format!("systemctl query failed: {err}"),
        },
        Some(Ok(output)) => match parse_kill_mode(output) {
            Some(mode) if mode.eq_ignore_ascii_case("control-group") => {
                KillModeAssessment::ControlGroup { unit }
            }
            Some(mode) => KillModeAssessment::Safe {
                unit,
                kill_mode: mode,
            },
            None => KillModeAssessment::Unknown {
                unit: Some(unit),
                reason: format!("unparsable systemctl output {output:?}"),
            },
        },
    }
}

/// Resolve the owning systemd unit name from `/proc/self/cgroup` contents.
///
/// Handles cgroup v2 (`0::/system.slice/foo.service`) and v1
/// (`1:name=systemd:/system.slice/foo.service`) layouts; the deepest
/// `.service` / `.scope` path component wins.
pub fn unit_from_cgroup(cgroup: &str) -> Option<String> {
    for line in cgroup.lines() {
        let path = line.rsplit_once(':').map(|(_, path)| path)?;
        let unit = path
            .split('/')
            .rfind(|component| component.ends_with(".service") || component.ends_with(".scope"));
        if let Some(unit) = unit {
            return Some(unit.to_string());
        }
    }
    None
}

/// Extract the KillMode value from `systemctl show -p KillMode <unit>`
/// output (`KillMode=control-group`), tolerating a bare `--value` form.
pub fn parse_kill_mode(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(value) = trimmed.strip_prefix("KillMode=") {
        let value = value.trim();
        return (!value.is_empty()).then(|| value.to_string());
    }
    // `systemctl show --value` prints the bare value.
    if !trimmed.contains('=') && !trimmed.contains(char::is_whitespace) {
        return Some(trimmed.to_string());
    }
    None
}

/// Probe the live environment. Linux-only gathering; other platforms
/// always report [`KillModeAssessment::NotSystemd`].
pub fn probe() -> KillModeAssessment {
    #[cfg(target_os = "linux")]
    {
        assess(&gather_inputs_linux())
    }
    #[cfg(not(target_os = "linux"))]
    {
        KillModeAssessment::NotSystemd
    }
}

#[cfg(target_os = "linux")]
fn gather_inputs_linux() -> SystemdProbeInputs {
    let invocation_id = std::env::var("INVOCATION_ID").ok();
    let systemd_managed = invocation_id
        .as_deref()
        .map(|id| !id.trim().is_empty())
        .unwrap_or(false);
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok();
    let kill_mode_query = if systemd_managed {
        cgroup
            .as_deref()
            .and_then(unit_from_cgroup)
            .map(|unit| query_kill_mode_linux(&unit))
    } else {
        None
    };
    SystemdProbeInputs {
        invocation_id,
        cgroup,
        kill_mode_query,
    }
}

#[cfg(target_os = "linux")]
fn query_kill_mode_linux(unit: &str) -> Result<String, String> {
    let output = std::process::Command::new("systemctl")
        .args(["show", "-p", "KillMode", unit])
        .output()
        .map_err(|err| format!("cannot run systemctl: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "systemctl exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(
        invocation_id: Option<&str>,
        cgroup: Option<&str>,
        query: Option<Result<&str, &str>>,
    ) -> SystemdProbeInputs {
        SystemdProbeInputs {
            invocation_id: invocation_id.map(str::to_string),
            cgroup: cgroup.map(str::to_string),
            kill_mode_query: query.map(|result| result.map(str::to_string).map_err(str::to_string)),
        }
    }

    #[test]
    fn silent_without_invocation_id() {
        let assessment = assess(&inputs(None, Some("0::/user.slice"), None));
        assert_eq!(assessment, KillModeAssessment::NotSystemd);
        assert!(assessment.warning().is_none());

        let empty = assess(&inputs(Some("  "), Some("0::/user.slice"), None));
        assert_eq!(empty, KillModeAssessment::NotSystemd);
    }

    #[test]
    fn control_group_warns() {
        let assessment = assess(&inputs(
            Some("abc123"),
            Some("0::/system.slice/myapp.service"),
            Some(Ok("KillMode=control-group\n")),
        ));
        assert_eq!(
            assessment,
            KillModeAssessment::ControlGroup {
                unit: "myapp.service".into()
            }
        );
        let warning = assessment.warning().expect("warns");
        assert!(warning.contains("myapp.service"));
        assert!(warning.contains("KillMode=control-group"));
    }

    #[test]
    fn safe_kill_mode_is_silent() {
        let assessment = assess(&inputs(
            Some("abc123"),
            Some("0::/system.slice/myapp.service"),
            Some(Ok("KillMode=process\n")),
        ));
        assert_eq!(
            assessment,
            KillModeAssessment::Safe {
                unit: "myapp.service".into(),
                kill_mode: "process".into()
            }
        );
        assert!(assessment.warning().is_none());
    }

    #[test]
    fn systemctl_failure_warns_as_unknown() {
        let assessment = assess(&inputs(
            Some("abc123"),
            Some("0::/system.slice/myapp.service"),
            Some(Err("cannot run systemctl: No such file or directory")),
        ));
        match &assessment {
            KillModeAssessment::Unknown { unit, reason } => {
                assert_eq!(unit.as_deref(), Some("myapp.service"));
                assert!(reason.contains("systemctl query failed"));
            }
            other => panic!("unexpected assessment: {other:?}"),
        }
        assert!(assessment.warning().is_some());
    }

    #[test]
    fn unresolved_unit_warns_as_unknown() {
        let assessment = assess(&inputs(Some("abc123"), Some("0::/user.slice"), None));
        assert_eq!(
            assessment,
            KillModeAssessment::Unknown {
                unit: None,
                reason: "owning unit could not be resolved from /proc/self/cgroup".into()
            }
        );
        assert!(assessment.warning().unwrap().contains("<unresolved>"));
    }

    #[test]
    fn unit_resolution_handles_v1_and_v2_and_scopes() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/foo.service"),
            Some("foo.service".into())
        );
        assert_eq!(
            unit_from_cgroup("1:name=systemd:/system.slice/bar.service\n2:cpu:/"),
            Some("bar.service".into())
        );
        assert_eq!(
            unit_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/run-u123.scope"
            ),
            Some("run-u123.scope".into())
        );
        assert_eq!(unit_from_cgroup("0::/"), None);
        assert_eq!(unit_from_cgroup(""), None);
    }

    #[test]
    fn kill_mode_parsing() {
        assert_eq!(
            parse_kill_mode("KillMode=control-group\n"),
            Some("control-group".into())
        );
        assert_eq!(parse_kill_mode("KillMode=mixed"), Some("mixed".into()));
        assert_eq!(
            parse_kill_mode("control-group\n"),
            Some("control-group".into())
        );
        assert_eq!(parse_kill_mode("KillMode="), None);
        assert_eq!(parse_kill_mode(""), None);
        assert_eq!(parse_kill_mode("Failed to get properties"), None);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn probe_is_not_systemd_off_linux() {
        assert_eq!(probe(), KillModeAssessment::NotSystemd);
    }
}
