//! Terminal graphics capability detection and reporting.
//!
//! The API is intentionally evidence-shaped instead of boolean-shaped. A
//! caller such as `clud` needs to know whether graphics support came from a
//! live probe, a strong host hint, weak environment identity, or a hard
//! negative. `auto` policies can then stay conservative while still surfacing
//! useful diagnostics.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Terminal graphics protocols recognized by capability detection.
pub enum GraphicsProtocol {
    /// Sixel bitmap graphics escape sequences.
    Sixel,
    /// Kitty graphics protocol escape sequences.
    Kitty,
    /// iTerm2 inline file protocol escape sequences.
    Iterm2File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Detection result for a terminal graphics protocol.
pub enum CapabilityStatus {
    /// The protocol is available for use.
    Supported,
    /// The protocol is known to be unavailable.
    Unsupported,
    /// Available evidence is not strong enough to decide.
    Unknown,
    /// The current terminal/session prevents using the protocol safely.
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Confidence level for graphics capability evidence.
pub enum EvidenceStrength {
    /// A live terminal probe confirmed the capability.
    Probe,
    /// A strong terminal-host signal identified expected support.
    StrongHostSignal,
    /// Terminfo advertised the capability.
    Terminfo,
    /// Environment identity provides only weak evidence.
    WeakEnv,
    /// A user setting supplied the evidence.
    UserOverride,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Detection result for one terminal graphics protocol.
pub struct GraphicsCapability {
    /// Protocol described by this result.
    pub protocol: GraphicsProtocol,
    /// Whether the protocol is usable, unavailable, unknown, or blocked.
    pub status: CapabilityStatus,
    /// Strength of the evidence behind `status`.
    pub evidence: EvidenceStrength,
    /// Human-readable source of the evidence.
    pub source: String,
    /// Context markers that may affect rendering reliability.
    pub risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Graphics capability set detected for a terminal.
pub struct TerminalGraphicsCapabilities {
    /// Per-protocol capability results.
    pub protocols: Vec<GraphicsCapability>,
    /// Preferred protocol when one is supported.
    pub preferred: Option<GraphicsProtocol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Terminal capability snapshot used by clients and diagnostics.
pub struct TerminalCapabilities {
    /// Whether standard input and output are attached to a terminal.
    pub is_tty: bool,
    /// Value of the `TERM` environment variable, when present.
    pub term: Option<String>,
    /// Value of the `TERM_PROGRAM` environment variable, when present.
    pub terminal_program: Option<String>,
    /// Detected terminal graphics capabilities.
    pub graphics: TerminalGraphicsCapabilities,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Raw terminal replies collected during active graphics probing.
pub struct TerminalProbeEvidence {
    /// Reply to the Sixel XTSMGRAPHICS query.
    pub sixel_xtsmgraphics: Option<String>,
    /// Primary device attributes reply used for Sixel detection.
    pub sixel_da1: Option<String>,
    /// Reply to the Kitty graphics query.
    pub kitty_graphics: Option<String>,
    /// Reply to the iTerm2 capabilities query.
    pub iterm2_capabilities: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Inputs used to detect terminal capabilities.
pub struct TerminalCapabilityInput {
    /// Whether the inspected streams are attached to a terminal.
    pub is_tty: bool,
    /// Environment variables used as host and session hints.
    pub env: BTreeMap<String, String>,
    /// Active probe replies to incorporate into detection.
    pub probe: TerminalProbeEvidence,
}

impl TerminalCapabilityInput {
    /// Builds detection input from the current process environment.
    pub fn from_env(is_tty: bool) -> Self {
        Self {
            is_tty,
            env: std::env::vars().collect(),
            probe: TerminalProbeEvidence::default(),
        }
    }

    /// Returns this input with active probe evidence attached.
    pub fn with_probe(mut self, probe: TerminalProbeEvidence) -> Self {
        self.probe = probe;
        self
    }
}

impl TerminalGraphicsCapabilities {
    /// Returns an all-protocol unknown capability set.
    pub fn unknown() -> Self {
        Self {
            protocols: vec![
                capability(
                    GraphicsProtocol::Sixel,
                    CapabilityStatus::Unknown,
                    EvidenceStrength::WeakEnv,
                    "missing",
                    Vec::<String>::new(),
                ),
                capability(
                    GraphicsProtocol::Kitty,
                    CapabilityStatus::Unknown,
                    EvidenceStrength::WeakEnv,
                    "missing",
                    Vec::<String>::new(),
                ),
                capability(
                    GraphicsProtocol::Iterm2File,
                    CapabilityStatus::Unknown,
                    EvidenceStrength::WeakEnv,
                    "missing",
                    Vec::<String>::new(),
                ),
            ],
            preferred: None,
        }
    }

    /// Finds the capability result for `protocol`.
    pub fn by_protocol(&self, protocol: GraphicsProtocol) -> Option<&GraphicsCapability> {
        self.protocols.iter().find(|c| c.protocol == protocol)
    }
}

/// Detects capabilities for the current terminal with the default probe timeout.
pub fn current_terminal_capabilities() -> TerminalCapabilities {
    current_terminal_capabilities_with_timeout(Duration::from_millis(80))
}

/// Detects capabilities for the current terminal using `timeout` for active probes.
pub fn current_terminal_capabilities_with_timeout(timeout: Duration) -> TerminalCapabilities {
    let is_tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let probe = if is_tty {
        active_probe(timeout)
    } else {
        TerminalProbeEvidence::default()
    };
    detect_terminal_capabilities(TerminalCapabilityInput::from_env(is_tty).with_probe(probe))
}

/// Detects terminal capabilities from explicit environment and probe evidence.
pub fn detect_terminal_capabilities(input: TerminalCapabilityInput) -> TerminalCapabilities {
    let term = env_value(&input.env, "TERM");
    let terminal_program = env_value(&input.env, "TERM_PROGRAM");
    let risks = base_risks(&input);

    let graphics = if !input.is_tty {
        blocked_all("non_tty", ["non_tty"])
    } else if is_linux_console(term.as_deref()) {
        blocked_all("TERM=linux", ["linux_console"])
    } else if is_screen(term.as_deref()) {
        blocked_all("TERM=screen", ["screen"])
    } else {
        let sixel = detect_sixel(&input, &risks);
        let kitty = detect_kitty(&input, &risks);
        let iterm2 = detect_iterm2(&input, &risks);
        let preferred = choose_preferred(&[sixel.clone(), kitty.clone(), iterm2.clone()]);
        TerminalGraphicsCapabilities {
            protocols: vec![sixel, kitty, iterm2],
            preferred,
        }
    };

    TerminalCapabilities {
        is_tty: input.is_tty,
        term,
        terminal_program,
        graphics,
    }
}

fn detect_sixel(input: &TerminalCapabilityInput, risks: &[String]) -> GraphicsCapability {
    if let Some(reply) = input.probe.sixel_xtsmgraphics.as_deref() {
        if xtsmgraphics_reports_sixel(reply) {
            return capability(
                GraphicsProtocol::Sixel,
                CapabilityStatus::Supported,
                EvidenceStrength::Probe,
                "XTSMGRAPHICS",
                risks.to_vec(),
            );
        }
    }
    if let Some(reply) = input.probe.sixel_da1.as_deref() {
        if primary_da_reports_sixel(reply) {
            return capability(
                GraphicsProtocol::Sixel,
                CapabilityStatus::Supported,
                EvidenceStrength::Probe,
                "DA1",
                risks.to_vec(),
            );
        }
    }

    let term = env_value(&input.env, "TERM").unwrap_or_default();
    let program = env_value(&input.env, "TERM_PROGRAM").unwrap_or_default();
    if contains_any(&term, &["alacritty", "kitty", "ghostty"])
        || contains_any(&program, &["Alacritty", "kitty", "Ghostty"])
        || env_value(&input.env, "VTE_VERSION").is_some()
        || contains_any(&program, &["gnome-terminal"])
        || contains_any(&term, &["vte"])
    {
        return capability(
            GraphicsProtocol::Sixel,
            CapabilityStatus::Blocked,
            EvidenceStrength::StrongHostSignal,
            first_source(&[
                ("TERM", &term),
                ("TERM_PROGRAM", &program),
                (
                    "VTE_VERSION",
                    &env_value(&input.env, "VTE_VERSION").unwrap_or_default(),
                ),
            ]),
            risks.to_vec(),
        );
    }

    if env_value(&input.env, "WT_SESSION").is_some() {
        let mut local_risks = risks.to_vec();
        local_risks.push("requires_windows_terminal_1_22".into());
        return capability(
            GraphicsProtocol::Sixel,
            CapabilityStatus::Supported,
            EvidenceStrength::StrongHostSignal,
            "WT_SESSION",
            local_risks,
        );
    }
    if term == "foot"
        || env_value(&input.env, "KONSOLE_VERSION").is_some()
        || contains_any(&program, &["WezTerm", "mintty"])
        || env_value(&input.env, "WEZTERM_PANE").is_some()
    {
        return capability(
            GraphicsProtocol::Sixel,
            CapabilityStatus::Supported,
            EvidenceStrength::StrongHostSignal,
            first_source(&[
                ("TERM", &term),
                ("TERM_PROGRAM", &program),
                (
                    "KONSOLE_VERSION",
                    &env_value(&input.env, "KONSOLE_VERSION").unwrap_or_default(),
                ),
                (
                    "WEZTERM_PANE",
                    &env_value(&input.env, "WEZTERM_PANE").unwrap_or_default(),
                ),
            ]),
            risks.to_vec(),
        );
    }

    if contains_any(&term, &["xterm"]) {
        return capability(
            GraphicsProtocol::Sixel,
            CapabilityStatus::Unknown,
            EvidenceStrength::WeakEnv,
            format!("TERM={term}"),
            risks.to_vec(),
        );
    }

    capability(
        GraphicsProtocol::Sixel,
        CapabilityStatus::Unknown,
        EvidenceStrength::WeakEnv,
        if term.is_empty() {
            "TERM missing".to_string()
        } else {
            format!("TERM={term}")
        },
        risks.to_vec(),
    )
}

fn detect_kitty(input: &TerminalCapabilityInput, risks: &[String]) -> GraphicsCapability {
    if let Some(reply) = input.probe.kitty_graphics.as_deref() {
        if reply.contains("_G") || reply.contains("OK") {
            return capability(
                GraphicsProtocol::Kitty,
                CapabilityStatus::Supported,
                EvidenceStrength::Probe,
                "kitty-query",
                risks.to_vec(),
            );
        }
    }
    let term = env_value(&input.env, "TERM").unwrap_or_default();
    let program = env_value(&input.env, "TERM_PROGRAM").unwrap_or_default();
    if contains_any(&term, &["kitty", "ghostty"])
        || contains_any(&program, &["kitty", "Ghostty", "WezTerm"])
        || env_value(&input.env, "WEZTERM_PANE").is_some()
    {
        return capability(
            GraphicsProtocol::Kitty,
            CapabilityStatus::Supported,
            EvidenceStrength::StrongHostSignal,
            first_source(&[
                ("TERM", &term),
                ("TERM_PROGRAM", &program),
                (
                    "WEZTERM_PANE",
                    &env_value(&input.env, "WEZTERM_PANE").unwrap_or_default(),
                ),
            ]),
            risks.to_vec(),
        );
    }
    capability(
        GraphicsProtocol::Kitty,
        CapabilityStatus::Unknown,
        EvidenceStrength::WeakEnv,
        if term.is_empty() {
            "TERM missing".to_string()
        } else {
            format!("TERM={term}")
        },
        risks.to_vec(),
    )
}

fn detect_iterm2(input: &TerminalCapabilityInput, risks: &[String]) -> GraphicsCapability {
    if let Some(reply) = input.probe.iterm2_capabilities.as_deref() {
        if reply.contains("Capabilities=") || reply.contains("File=") {
            return capability(
                GraphicsProtocol::Iterm2File,
                CapabilityStatus::Supported,
                EvidenceStrength::Probe,
                "OSC 1337;Capabilities",
                risks.to_vec(),
            );
        }
    }
    let program = env_value(&input.env, "TERM_PROGRAM").unwrap_or_default();
    if contains_any(&program, &["iTerm.app", "WezTerm", "mintty"]) {
        return capability(
            GraphicsProtocol::Iterm2File,
            CapabilityStatus::Supported,
            EvidenceStrength::StrongHostSignal,
            format!("TERM_PROGRAM={program}"),
            risks.to_vec(),
        );
    }
    capability(
        GraphicsProtocol::Iterm2File,
        CapabilityStatus::Unknown,
        EvidenceStrength::WeakEnv,
        if program.is_empty() {
            "TERM_PROGRAM missing".to_string()
        } else {
            format!("TERM_PROGRAM={program}")
        },
        risks.to_vec(),
    )
}

fn choose_preferred(capabilities: &[GraphicsCapability]) -> Option<GraphicsProtocol> {
    capabilities
        .iter()
        .find(|c| c.status == CapabilityStatus::Supported && c.evidence == EvidenceStrength::Probe)
        .or_else(|| {
            capabilities
                .iter()
                .find(|c| c.status == CapabilityStatus::Supported)
        })
        .map(|c| c.protocol)
}

fn blocked_all(
    source: &str,
    risks: impl IntoIterator<Item = impl Into<String>>,
) -> TerminalGraphicsCapabilities {
    let risks: Vec<String> = risks.into_iter().map(Into::into).collect();
    TerminalGraphicsCapabilities {
        protocols: vec![
            capability(
                GraphicsProtocol::Sixel,
                CapabilityStatus::Blocked,
                EvidenceStrength::StrongHostSignal,
                source,
                risks.clone(),
            ),
            capability(
                GraphicsProtocol::Kitty,
                CapabilityStatus::Blocked,
                EvidenceStrength::StrongHostSignal,
                source,
                risks.clone(),
            ),
            capability(
                GraphicsProtocol::Iterm2File,
                CapabilityStatus::Blocked,
                EvidenceStrength::StrongHostSignal,
                source,
                risks,
            ),
        ],
        preferred: None,
    }
}

fn base_risks(input: &TerminalCapabilityInput) -> Vec<String> {
    let mut risks = Vec::new();
    if env_value(&input.env, "TMUX").is_some() || is_tmux(env_value(&input.env, "TERM").as_deref())
    {
        risks.push("tmux".into());
    }
    if env_value(&input.env, "SSH_CONNECTION").is_some()
        || env_value(&input.env, "SSH_TTY").is_some()
    {
        risks.push("ssh".into());
    }
    risks
}

fn capability(
    protocol: GraphicsProtocol,
    status: CapabilityStatus,
    evidence: EvidenceStrength,
    source: impl Into<String>,
    risks: impl IntoIterator<Item = impl Into<String>>,
) -> GraphicsCapability {
    GraphicsCapability {
        protocol,
        status,
        evidence,
        source: source.into(),
        risks: risks.into_iter().map(Into::into).collect(),
    }
}

fn env_value(env: &BTreeMap<String, String>, key: &str) -> Option<String> {
    env.get(key).filter(|v| !v.is_empty()).cloned()
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    let lower = value.to_ascii_lowercase();
    needles
        .iter()
        .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
}

fn is_linux_console(term: Option<&str>) -> bool {
    matches!(term, Some("linux"))
}

fn is_screen(term: Option<&str>) -> bool {
    term.is_some_and(|t| t.starts_with("screen") || t == "screen")
}

fn is_tmux(term: Option<&str>) -> bool {
    term.is_some_and(|t| t.starts_with("tmux") || t.contains("tmux"))
}

fn first_source(candidates: &[(&str, &str)]) -> String {
    for (key, value) in candidates {
        if !value.is_empty() {
            return format!("{key}={value}");
        }
    }
    "unknown".into()
}

/// Returns whether a primary device attributes reply advertises Sixel support.
pub fn primary_da_reports_sixel(reply: &str) -> bool {
    reply
        .split('\x1b')
        .filter_map(|part| part.strip_prefix("[?"))
        .filter_map(|part| part.split('c').next())
        .flat_map(|params| params.split(';'))
        .any(|param| param == "4")
}

/// Returns whether an XTSMGRAPHICS reply indicates Sixel graphics support.
pub fn xtsmgraphics_reports_sixel(reply: &str) -> bool {
    reply.contains("\x1b[?") && reply.contains('S')
}

#[cfg(unix)]
fn active_probe(timeout: Duration) -> TerminalProbeEvidence {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return TerminalProbeEvidence::default();
    };
    let fd = tty.as_raw_fd();
    let mut old_termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    let have_termios = unsafe { libc::tcgetattr(fd, old_termios.as_mut_ptr()) == 0 };
    let old_termios = if have_termios {
        Some(unsafe { old_termios.assume_init() })
    } else {
        None
    };
    if let Some(mut raw) = old_termios {
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    }
    let old_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags | libc::O_NONBLOCK) };
    }

    let _ = tty.write_all(
        b"\x1b[c\x1b[?2;1;0S\x1b_Gi=running-process-probe,a=q;\x1b\\\x1b]1337;Capabilities\x07",
    );
    let _ = tty.flush();

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    while Instant::now() < deadline {
        let mut chunk = [0_u8; 512];
        match tty.read(&mut chunk) {
            Ok(0) => std::thread::sleep(Duration::from_millis(5)),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }

    if old_flags >= 0 {
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFL, old_flags) };
    }
    if let Some(old) = old_termios {
        let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &old) };
    }

    let reply = String::from_utf8_lossy(&buf).into_owned();
    TerminalProbeEvidence {
        sixel_xtsmgraphics: reply.contains('S').then(|| reply.clone()),
        sixel_da1: reply.contains("[?").then(|| reply.clone()),
        kitty_graphics: reply.contains("_G").then(|| reply.clone()),
        iterm2_capabilities: reply.contains("Capabilities=").then_some(reply),
    }
}

#[cfg(not(unix))]
fn active_probe(_timeout: Duration) -> TerminalProbeEvidence {
    TerminalProbeEvidence::default()
}

#[cfg(feature = "client")]
/// Converts terminal graphics capabilities into the daemon protobuf form.
pub fn terminal_graphics_capabilities_to_proto(
    caps: &TerminalGraphicsCapabilities,
) -> crate::proto::daemon::TerminalGraphicsCapabilities {
    crate::proto::daemon::TerminalGraphicsCapabilities {
        protocols: caps
            .protocols
            .iter()
            .map(graphics_capability_to_proto)
            .collect(),
        preferred: caps
            .preferred
            .map(proto_graphics_protocol)
            .unwrap_or(crate::proto::daemon::GraphicsProtocol::Unspecified)
            as i32,
    }
}

#[cfg(feature = "client")]
/// Converts daemon protobuf terminal graphics capabilities into the Rust form.
pub fn terminal_graphics_capabilities_from_proto(
    caps: &crate::proto::daemon::TerminalGraphicsCapabilities,
) -> TerminalGraphicsCapabilities {
    let protocols = caps
        .protocols
        .iter()
        .map(graphics_capability_from_proto)
        .collect();
    TerminalGraphicsCapabilities {
        protocols,
        preferred: graphics_protocol_from_i32(caps.preferred),
    }
}

#[cfg(feature = "client")]
fn graphics_capability_to_proto(
    capability: &GraphicsCapability,
) -> crate::proto::daemon::TerminalGraphicsCapability {
    crate::proto::daemon::TerminalGraphicsCapability {
        protocol: proto_graphics_protocol(capability.protocol) as i32,
        status: proto_capability_status(capability.status) as i32,
        evidence: proto_evidence_strength(capability.evidence) as i32,
        source: capability.source.clone(),
        risks: capability.risks.clone(),
    }
}

#[cfg(feature = "client")]
fn graphics_capability_from_proto(
    capability: &crate::proto::daemon::TerminalGraphicsCapability,
) -> GraphicsCapability {
    GraphicsCapability {
        protocol: graphics_protocol_from_i32(capability.protocol)
            .unwrap_or(GraphicsProtocol::Sixel),
        status: capability_status_from_i32(capability.status),
        evidence: evidence_strength_from_i32(capability.evidence),
        source: capability.source.clone(),
        risks: capability.risks.clone(),
    }
}

#[cfg(feature = "client")]
fn proto_graphics_protocol(protocol: GraphicsProtocol) -> crate::proto::daemon::GraphicsProtocol {
    match protocol {
        GraphicsProtocol::Sixel => crate::proto::daemon::GraphicsProtocol::Sixel,
        GraphicsProtocol::Kitty => crate::proto::daemon::GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2File => crate::proto::daemon::GraphicsProtocol::Iterm2File,
    }
}

#[cfg(feature = "client")]
fn graphics_protocol_from_i32(protocol: i32) -> Option<GraphicsProtocol> {
    match crate::proto::daemon::GraphicsProtocol::try_from(protocol).ok()? {
        crate::proto::daemon::GraphicsProtocol::Sixel => Some(GraphicsProtocol::Sixel),
        crate::proto::daemon::GraphicsProtocol::Kitty => Some(GraphicsProtocol::Kitty),
        crate::proto::daemon::GraphicsProtocol::Iterm2File => Some(GraphicsProtocol::Iterm2File),
        crate::proto::daemon::GraphicsProtocol::Unspecified => None,
    }
}

#[cfg(feature = "client")]
fn proto_capability_status(status: CapabilityStatus) -> crate::proto::daemon::CapabilityStatus {
    match status {
        CapabilityStatus::Supported => crate::proto::daemon::CapabilityStatus::Supported,
        CapabilityStatus::Unsupported => crate::proto::daemon::CapabilityStatus::Unsupported,
        CapabilityStatus::Unknown => crate::proto::daemon::CapabilityStatus::Unknown,
        CapabilityStatus::Blocked => crate::proto::daemon::CapabilityStatus::Blocked,
    }
}

#[cfg(feature = "client")]
fn capability_status_from_i32(status: i32) -> CapabilityStatus {
    match crate::proto::daemon::CapabilityStatus::try_from(status)
        .unwrap_or(crate::proto::daemon::CapabilityStatus::Unknown)
    {
        crate::proto::daemon::CapabilityStatus::Supported => CapabilityStatus::Supported,
        crate::proto::daemon::CapabilityStatus::Unsupported => CapabilityStatus::Unsupported,
        crate::proto::daemon::CapabilityStatus::Unknown
        | crate::proto::daemon::CapabilityStatus::Unspecified => CapabilityStatus::Unknown,
        crate::proto::daemon::CapabilityStatus::Blocked => CapabilityStatus::Blocked,
    }
}

#[cfg(feature = "client")]
fn proto_evidence_strength(evidence: EvidenceStrength) -> crate::proto::daemon::EvidenceStrength {
    match evidence {
        EvidenceStrength::Probe => crate::proto::daemon::EvidenceStrength::Probe,
        EvidenceStrength::StrongHostSignal => {
            crate::proto::daemon::EvidenceStrength::StrongHostSignal
        }
        EvidenceStrength::Terminfo => crate::proto::daemon::EvidenceStrength::Terminfo,
        EvidenceStrength::WeakEnv => crate::proto::daemon::EvidenceStrength::WeakEnv,
        EvidenceStrength::UserOverride => crate::proto::daemon::EvidenceStrength::UserOverride,
    }
}

#[cfg(feature = "client")]
fn evidence_strength_from_i32(evidence: i32) -> EvidenceStrength {
    match crate::proto::daemon::EvidenceStrength::try_from(evidence)
        .unwrap_or(crate::proto::daemon::EvidenceStrength::WeakEnv)
    {
        crate::proto::daemon::EvidenceStrength::Probe => EvidenceStrength::Probe,
        crate::proto::daemon::EvidenceStrength::StrongHostSignal => {
            EvidenceStrength::StrongHostSignal
        }
        crate::proto::daemon::EvidenceStrength::Terminfo => EvidenceStrength::Terminfo,
        crate::proto::daemon::EvidenceStrength::WeakEnv
        | crate::proto::daemon::EvidenceStrength::Unspecified => EvidenceStrength::WeakEnv,
        crate::proto::daemon::EvidenceStrength::UserOverride => EvidenceStrength::UserOverride,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(term: &str, pairs: &[(&str, &str)]) -> TerminalCapabilityInput {
        let mut env = BTreeMap::new();
        if !term.is_empty() {
            env.insert("TERM".into(), term.into());
        }
        for (k, v) in pairs {
            env.insert((*k).into(), (*v).into());
        }
        TerminalCapabilityInput {
            is_tty: true,
            env,
            probe: TerminalProbeEvidence::default(),
        }
    }

    #[test]
    fn weak_xterm_does_not_confirm_sixel() {
        let caps = detect_terminal_capabilities(input("xterm-256color", &[]));
        let sixel = caps.graphics.by_protocol(GraphicsProtocol::Sixel).unwrap();
        assert_eq!(sixel.status, CapabilityStatus::Unknown);
        assert_eq!(sixel.evidence, EvidenceStrength::WeakEnv);
        assert_eq!(caps.graphics.preferred, None);
    }

    #[test]
    fn da1_probe_confirms_sixel() {
        let mut case = input("xterm-256color", &[]);
        case.probe.sixel_da1 = Some("\x1b[?62;4;22c".into());
        let caps = detect_terminal_capabilities(case);
        let sixel = caps.graphics.by_protocol(GraphicsProtocol::Sixel).unwrap();
        assert_eq!(sixel.status, CapabilityStatus::Supported);
        assert_eq!(sixel.evidence, EvidenceStrength::Probe);
        assert_eq!(caps.graphics.preferred, Some(GraphicsProtocol::Sixel));
    }

    #[test]
    fn vt100_da_does_not_confirm_sixel() {
        assert!(!primary_da_reports_sixel("\x1b[?1;2c"));
        assert!(!primary_da_reports_sixel("\x1b[?62;22c"));
    }
}
