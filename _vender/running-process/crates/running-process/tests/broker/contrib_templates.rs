#![cfg(feature = "client")]

use std::path::{Path, PathBuf};

#[test]
fn systemd_unit_preserves_required_broker_hardening() {
    let path = repo_root().join("contrib/systemd/running-process-broker-v1.service");
    let text = read(&path);

    assert_contains_all(
        &text,
        &[
            "[Unit]",
            "[Service]",
            "ExecStart=%h/.local/bin/running-process-broker-v1",
            "KillMode=process",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "PrivateTmp=yes",
            "RestrictAddressFamilies=AF_UNIX",
            "SystemCallFilter=@system-service",
            "[Install]",
        ],
    );
    assert!(
        !text.contains("KillMode=control-group"),
        "broker systemd unit must not use KillMode=control-group"
    );
}

#[test]
fn launchd_agent_preserves_broker_identity_and_restart_policy() {
    let path = repo_root().join("contrib/launchd/com.zackees.running-process-broker-v1.plist");
    let text = read(&path);

    assert_contains_all(
        &text,
        &[
            "<key>Label</key>",
            "<string>com.zackees.running-process-broker-v1</string>",
            "<key>ProgramArguments</key>",
            "<string>/usr/local/bin/running-process-broker-v1</string>",
            "<key>RunAtLoad</key>",
            "<false/>",
            "<key>KeepAlive</key>",
            "<key>SuccessfulExit</key>",
            "<key>ProcessType</key>",
            "<string>Background</string>",
        ],
    );
}

#[test]
fn windows_service_installer_preserves_manual_optional_install_contract() {
    let path = repo_root().join("contrib/windows-service/install.ps1");
    let text = read(&path);

    assert_contains_all(
        &text,
        &[
            "#Requires -Version 5.1",
            "[CmdletBinding(SupportsShouldProcess = $true)]",
            "[ValidateSet(\"Install\", \"Uninstall\", \"Start\", \"Stop\", \"Status\")]",
            "running-process-broker-v1.exe",
            "Assert-Administrator",
            "Test-Path -LiteralPath $BinaryPath -PathType Leaf",
            "New-Service @parameters",
            "StartupType = \"Manual\"",
            "BinaryPathName = $quotedBinary",
            "--service",
            "sc.exe failure",
            "sc.exe delete",
        ],
    );
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "expected contrib template to be readable at {}: {err}",
            path.display()
        )
    })
}

fn assert_contains_all(text: &str, required: &[&str]) {
    let missing = required
        .iter()
        .copied()
        .filter(|needle| !text.contains(needle))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "contrib template is missing required entries:\n{}",
        missing.join("\n")
    );
}
