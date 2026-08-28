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

use crate::common;

use std::process::Command;

/// soldr#2739 inverted this canary.
///
/// soldr#2566's version asserted every lane *exported* `strict`. That
/// invariant died with the default-on flip: the exports are redundant now,
/// and requiring them would re-create the very fragility soldr#2698 exposed,
/// where a new lane escaped the sweep and silently ran unguarded.
///
/// The remaining risk runs the other way. With enforcement on by default,
/// the only way a lane loses it is by actively opting out, so that is what
/// this pins — in CI *and* locally, since the default now applies everywhere.
#[test]
fn no_lane_opts_out_of_the_reentrancy_guard() {
    let raw = std::env::var("SOLDR_REENTRANCY_GUARD").ok();
    let Some(value) = raw else {
        return; // Unset is the enforcing default. Nothing to check.
    };
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return; // Empty expresses no preference; the default applies.
    }
    assert_ne!(
        normalized,
        soldr_cli::reentrancy_guard::GUARD_MODE_OFF,
        "soldr#2739: this lane disables the re-entrancy guard. The hatch is \
         emergency-only and must not be committed to a workflow; remove the \
         SOLDR_REENTRANCY_GUARD=off export."
    );
    assert!(
        soldr_cli::reentrancy_guard::GuardMode::from_env_value(Some(&value)).is_ok(),
        "soldr#2739: SOLDR_REENTRANCY_GUARD={value:?} is not a recognised \
         value, so soldr will refuse to start in this lane"
    );
}

#[test]
fn unsanctioned_nested_entry_exits_one_with_diagnostic() {
    let soldr = common::soldr_bin();
    // This test process: alive, and necessarily a different pid from the
    // child it spawns. soldr#2566 used the literal `1` on the reasoning that
    // init is never a test harness -- true on Unix, but since soldr#2739 the
    // guard ignores markers whose writer has exited, and pid 1 is not a live
    // process on Windows. That would have made this canary pass vacuously on
    // the Windows lanes, which is precisely what a canary must never do.
    let foreign_pid = std::process::id();
    let output = Command::new(&soldr)
        .arg("status")
        .env("SOLDR_REENTRANCY_GUARD", "strict")
        .env("IN_SOLDR_PID", foreign_pid.to_string())
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
        stderr.contains(&format!("IN_SOLDR_PID={foreign_pid}")),
        "diagnostic must name the inherited marker: {stderr}"
    );
}

#[test]
fn sanctioned_edge_still_enters_under_strict() {
    let soldr = common::soldr_bin();
    // The same nested shape, but carrying a sanctioned internal edge marker:
    // strict mode must let it through (exit 0 from `soldr --version`).
    //
    // The marker must name a *live* process (soldr#2739): with a dead pid the
    // guard would ignore the marker entirely, so this would pass without ever
    // exercising the sanctioned-edge path it exists to cover.
    let output = Command::new(&soldr)
        .arg("--version")
        .env("SOLDR_REENTRANCY_GUARD", "strict")
        .env("IN_SOLDR_PID", std::process::id().to_string())
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
