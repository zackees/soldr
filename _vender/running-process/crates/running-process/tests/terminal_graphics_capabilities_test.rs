use running_process::{
    detect_terminal_capabilities, CapabilityStatus, EvidenceStrength, GraphicsProtocol,
    TerminalCapabilities, TerminalCapabilityInput, TerminalProbeEvidence,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
struct Case {
    name: &'static str,
    is_tty: bool,
    env: &'static [(&'static str, &'static str)],
    probe: TerminalProbeEvidence,
    expected_sixel_status: CapabilityStatus,
    expected_sixel_evidence: EvidenceStrength,
    expected_preferred: Option<GraphicsProtocol>,
}

#[test]
fn terminal_graphics_capability_matrix_matches_expectations() {
    let cases = cases();
    let mut rows = Vec::new();
    for case in cases {
        let caps = detect_terminal_capabilities(TerminalCapabilityInput {
            is_tty: case.is_tty,
            env: env(case.env),
            probe: case.probe.clone(),
        });
        let sixel = caps
            .graphics
            .by_protocol(GraphicsProtocol::Sixel)
            .expect("sixel capability present");
        assert_eq!(
            sixel.status, case.expected_sixel_status,
            "{} sixel status",
            case.name
        );
        assert_eq!(
            sixel.evidence, case.expected_sixel_evidence,
            "{} sixel evidence",
            case.name
        );
        assert_eq!(
            caps.graphics.preferred, case.expected_preferred,
            "{} preferred protocol",
            case.name
        );
        export_case(case.name, &caps);
        rows.push(json!({
            "case": case.name,
            "sixel_status": format!("{:?}", sixel.status),
            "sixel_evidence": format!("{:?}", sixel.evidence),
            "preferred": caps.graphics.preferred.map(|p| format!("{p:?}")),
            "source": sixel.source,
            "risks": sixel.risks,
        }));
    }
    export_aggregate(&rows);
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "non_tty",
            is_tty: false,
            env: &[("TERM", "xterm-256color")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Blocked,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: None,
        },
        Case {
            name: "linux_console",
            is_tty: true,
            env: &[("TERM", "linux")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Blocked,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: None,
        },
        Case {
            name: "screen",
            is_tty: true,
            env: &[("TERM", "screen-256color")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Blocked,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: None,
        },
        Case {
            name: "weak_xterm",
            is_tty: true,
            env: &[("TERM", "xterm-256color")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Unknown,
            expected_sixel_evidence: EvidenceStrength::WeakEnv,
            expected_preferred: None,
        },
        Case {
            name: "da1_probe_sixel",
            is_tty: true,
            env: &[("TERM", "xterm-256color")],
            probe: TerminalProbeEvidence {
                sixel_da1: Some("\x1b[?62;4;22c".into()),
                ..Default::default()
            },
            expected_sixel_status: CapabilityStatus::Supported,
            expected_sixel_evidence: EvidenceStrength::Probe,
            expected_preferred: Some(GraphicsProtocol::Sixel),
        },
        Case {
            name: "xtsmgraphics_probe_sixel",
            is_tty: true,
            env: &[("TERM", "xterm-256color")],
            probe: TerminalProbeEvidence {
                sixel_xtsmgraphics: Some("\x1b[?2;1;256S".into()),
                ..Default::default()
            },
            expected_sixel_status: CapabilityStatus::Supported,
            expected_sixel_evidence: EvidenceStrength::Probe,
            expected_preferred: Some(GraphicsProtocol::Sixel),
        },
        Case {
            name: "windows_terminal_hint",
            is_tty: true,
            env: &[("TERM", "xterm-256color"), ("WT_SESSION", "abc")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Supported,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: Some(GraphicsProtocol::Sixel),
        },
        Case {
            name: "alacritty_sixel_blocked",
            is_tty: true,
            env: &[("TERM", "alacritty")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Blocked,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: None,
        },
        Case {
            name: "kitty_prefers_kitty_not_sixel",
            is_tty: true,
            env: &[("TERM", "xterm-kitty"), ("TERM_PROGRAM", "kitty")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Blocked,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: Some(GraphicsProtocol::Kitty),
        },
        Case {
            name: "wezterm_multi_protocol_hint",
            is_tty: true,
            env: &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "WezTerm")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Supported,
            expected_sixel_evidence: EvidenceStrength::StrongHostSignal,
            expected_preferred: Some(GraphicsProtocol::Sixel),
        },
        Case {
            name: "tmux_weak_env_risk",
            is_tty: true,
            env: &[("TERM", "tmux-256color"), ("TMUX", "/tmp/tmux")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Unknown,
            expected_sixel_evidence: EvidenceStrength::WeakEnv,
            expected_preferred: None,
        },
        Case {
            name: "iterm2_file_hint",
            is_tty: true,
            env: &[("TERM", "xterm-256color"), ("TERM_PROGRAM", "iTerm.app")],
            probe: TerminalProbeEvidence::default(),
            expected_sixel_status: CapabilityStatus::Unknown,
            expected_sixel_evidence: EvidenceStrength::WeakEnv,
            expected_preferred: Some(GraphicsProtocol::Iterm2File),
        },
    ]
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn export_case(name: &str, caps: &TerminalCapabilities) {
    let Some(dir) = export_dir() else {
        return;
    };
    fs::create_dir_all(&dir).expect("create export dir");
    let path = dir.join(format!("{name}.json"));
    fs::write(
        path,
        serde_json::to_string_pretty(caps).expect("serialize capabilities"),
    )
    .expect("write capability json");
}

fn export_aggregate(rows: &[serde_json::Value]) {
    let Some(dir) = export_dir() else {
        return;
    };
    fs::create_dir_all(&dir).expect("create export dir");
    fs::write(
        dir.join("matrix-summary.json"),
        serde_json::to_string_pretty(rows).expect("serialize summary"),
    )
    .expect("write matrix summary");
}

fn export_dir() -> Option<PathBuf> {
    std::env::var_os("RUNNING_PROCESS_TERMINAL_CAPABILITY_EXPORT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
