//! soldr#2566 — CI canaries for the strict re-entrancy switch.
//!
//! Two invariants, deliberately separate:
//!
//! 1. **The switch is live in every soldr CI lane.** Workflow refactors are
//!    exactly how an env-level flag silently disappears; a test that runs in
//!    every lane class (native `cargo test`, the nextest target-run archive
//!    replays, the acceptance workflows) and asserts the variable is present
//!    turns that regression into a named failure. Outside CI it passes
//!    vacuously so local runs are unaffected.
//! 2. **The mechanism actually rejects.** A deliberately unsanctioned nested
//!    `soldr status` under a foreign `IN_SOLDR_PID` must exit 1 with the
//!    bounded diagnostic — end-to-end through the real binary, not just the
//!    `decide()` unit tests that shipped with soldr#2580.

mod common;

use std::process::Command;

#[test]
fn ci_lane_exports_strict_reentrancy_guard() {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        // Local run: enforcement is opt-in (soldr#2547 owns the default-on
        // rollout); the canary only pins soldr's own CI lanes.
        return;
    }
    let mode = std::env::var("SOLDR_REENTRANCY_GUARD").unwrap_or_default();
    assert_eq!(
        mode.trim().to_ascii_lowercase(),
        "strict",
        "soldr#2566: this CI lane does not export SOLDR_REENTRANCY_GUARD=strict — \
         a workflow refactor dropped the switch (add it to the workflow-level env block)"
    );
}

#[test]
fn unsanctioned_nested_entry_exits_one_with_diagnostic() {
    let soldr = common::soldr_bin();
    // A foreign pid that cannot be this process (pid 1..=2 are init/kthreadd
    // shaped on Unix and never a test harness; any value != child pid works
    // because the child compares against its own pid).
    let output = Command::new(&soldr)
        .arg("status")
        .env("SOLDR_REENTRANCY_GUARD", "strict")
        .env("IN_SOLDR_PID", "1")
        // Scrub every sanctioned-edge variable a surrounding soldr (or this
        // suite's own harness) may have exported, so the entry is judged
        // purely as an unsanctioned re-entry.
        .env_remove("SOLDR_INTERNAL_BROKER_INSTANCE_ID")
        .env_remove("SOLDR_INTERNAL_DAEMON_EXE")
        .env_remove("SOLDR_INTERNAL_DAEMON_REEXECED")
        .env_remove("SOLDR_INTERNAL_INHERIT_PROCESS_GROUP")
        .env_remove("SOLDR_TRAMPOLINING")
        .env_remove("SOLDR_GLOBAL_DELEGATING")
        .output()
        .expect("spawn soldr status");

    assert_eq!(
        output.status.code(),
        Some(1),
        "an unsanctioned nested entry must exit 1, got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsanctioned Soldr re-entrancy"),
        "diagnostic must name the rejection: {stderr}"
    );
    assert!(
        stderr.contains("IN_SOLDR_PID=1"),
        "diagnostic must name the inherited marker: {stderr}"
    );
}

#[test]
fn sanctioned_edge_still_enters_under_strict() {
    let soldr = common::soldr_bin();
    // The same nested shape, but carrying a sanctioned internal edge marker:
    // strict mode must let it through (exit 0 from `soldr --version`).
    let output = Command::new(&soldr)
        .arg("--version")
        .env("SOLDR_REENTRANCY_GUARD", "strict")
        .env("IN_SOLDR_PID", "1")
        .env("SOLDR_TRAMPOLINING", "1")
        .output()
        .expect("spawn soldr --version");
    assert!(
        output.status.success(),
        "a sanctioned edge must pass strict mode: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
