//! Process-level coverage for the re-entrancy guard's strict mode
//! (soldr#2547 / soldr#2566 first slice): an ordinary CLI entry that
//! inherits a foreign `IN_SOLDR_PID` must exit 1 with a diagnostic, while
//! sanctioned edges and non-strict mode stay untouched.

use std::process::Command;

mod common;

fn guarded_soldr() -> Command {
    let mut cmd = common::isolated_soldr_command();
    // The helper scrubs the marker so fixtures are honest; this suite
    // re-injects it deliberately.
    cmd.env(soldr_cli::reentrancy_guard::IN_SOLDR_PID_ENV, "999999");
    cmd
}

#[test]
fn strict_rejects_plain_cli_entry_with_foreign_marker() {
    let output = guarded_soldr()
        .env(soldr_cli::reentrancy_guard::GUARD_MODE_ENV, "strict")
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rejected unsanctioned Soldr re-entrancy"),
        "diagnostic must name the rejection: {stderr}"
    );
    assert!(
        stderr.contains("IN_SOLDR_PID=999999"),
        "diagnostic must name the inherited pid: {stderr}"
    );
}

#[test]
fn non_strict_mode_only_stamps_and_proceeds() {
    let output = guarded_soldr()
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert!(
        output.status.success(),
        "without strict mode a foreign marker is informational only; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn strict_allows_a_sanctioned_internal_edge() {
    let output = guarded_soldr()
        .env(soldr_cli::reentrancy_guard::GUARD_MODE_ENV, "strict")
        // The trampoline marker identifies a sanctioned Soldr-to-Soldr
        // hand-off; any single sanctioned-edge variable must pass.
        .env("SOLDR_TRAMPOLINING", "test-edge")
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert!(
        output.status.success(),
        "sanctioned edge must not be rejected; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
