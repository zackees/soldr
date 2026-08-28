//! Process-level coverage for the re-entrancy guard's strict mode
//! (soldr#2547 / soldr#2566 first slice): an ordinary CLI entry that
//! inherits a foreign `IN_SOLDR_PID` must exit 1 with a diagnostic, while
//! sanctioned edges and non-strict mode stay untouched.

use std::process::Command;

use crate::common;

/// A foreign marker naming a process that is definitely alive.
///
/// The test process itself: it is running, and its pid differs from the
/// soldr it spawns, which is exactly the shape of a real re-entry. Before
/// soldr#2739 this was the literal `999999`; once the guard began ignoring
/// markers whose writer has exited, a pid that does not exist would have made
/// these fixtures pass for the wrong reason.
fn live_foreign_pid() -> u32 {
    std::process::id()
}

fn guarded_soldr() -> Command {
    let mut cmd = common::isolated_soldr_command();
    // The helper scrubs the marker so fixtures are honest; this suite
    // re-injects it deliberately.
    cmd.env(
        soldr_cli::reentrancy_guard::IN_SOLDR_PID_ENV,
        live_foreign_pid().to_string(),
    );
    cmd
}

/// A pid that is definitely dead: spawn a trivial child and reap it.
fn reaped_child_pid() -> u32 {
    let mut child = common::isolated_soldr_command()
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn throwaway child");
    let pid = child.id();
    child.wait().expect("reap throwaway child");
    pid
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
        stderr.contains(&format!("IN_SOLDR_PID={}", live_foreign_pid())),
        "diagnostic must name the inherited pid: {stderr}"
    );
}

/// soldr#2547 item 5: the diagnostic must survive stderr being redirected or
/// detached.
///
/// The processes this guard exists to catch are the ones nobody is watching —
/// a detached `broker serve`, a child several tools deep — so a rejection that
/// only ever reached stderr would be invisible exactly when it matters. This
/// asserts the same facts land on disk, and that the record does not carry the
/// wider environment with it.
#[test]
fn a_rejection_is_journalled_under_the_soldr_logs_root() {
    let home = common::unique_temp_dir("reentrancy-record");
    let output = guarded_soldr()
        .env(soldr_cli::reentrancy_guard::GUARD_MODE_ENV, "strict")
        .env("SOLDR_CACHE_DIR", home.join("cache"))
        .env("SOLDR_TRAMPOLINING_DECOY", "must-not-be-disclosed")
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert_eq!(output.status.code(), Some(1));

    let stderr = String::from_utf8_lossy(&output.stderr);
    let record_line = stderr
        .lines()
        .find_map(|line| line.trim().strip_prefix("soldr:   record: "))
        .unwrap_or_else(|| panic!("stderr must point at the record it wrote:\n{stderr}"));
    assert_ne!(
        record_line, "<not written>",
        "the record must be written when the logs root is writable:\n{stderr}"
    );

    let body = std::fs::read_to_string(record_line)
        .unwrap_or_else(|error| panic!("record {record_line} unreadable: {error}\n{stderr}"));
    let record: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|error| panic!("record is not JSON: {error}"));

    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["event"], "reentrancy_rejected");
    assert_eq!(record["inherited_in_soldr_pid"], live_foreign_pid());
    assert_eq!(record["argv"][1], "--version");
    assert!(
        record["pid"].as_u64().is_some_and(|pid| pid > 0),
        "the record must name the rejected process: {body}"
    );

    // Redaction is the property most worth pinning: this file outlives the
    // process, and the guard fires on graphs that may carry secrets.
    assert!(
        !body.contains("must-not-be-disclosed"),
        "only the routing allowlist may be disclosed, got: {body}"
    );
    for never in ["PATH", "HOME", "USERPROFILE", "SOLDR_TRAMPOLINING_DECOY"] {
        assert!(
            !record["routing_env"]
                .as_object()
                .expect("routing_env object")
                .contains_key(never),
            "{never} must not appear in the record: {body}"
        );
    }
}

#[test]
fn non_strict_mode_only_stamps_and_proceeds() {
    // soldr#2739 flipped the default to enforcing, so the permissive path is
    // now reached only through the explicit hatch. Kept rather than deleted:
    // the hatch still exists, so it still needs coverage.
    let output = guarded_soldr()
        .env(
            soldr_cli::reentrancy_guard::GUARD_MODE_ENV,
            soldr_cli::reentrancy_guard::GUARD_MODE_OFF,
        )
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert!(
        output.status.success(),
        "with the guard off a foreign marker is informational only; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The flip itself: what used to need `SOLDR_REENTRANCY_GUARD=strict` now
/// happens with the variable unset.
#[test]
fn enforcement_is_on_by_default_with_no_env_var() {
    let output = guarded_soldr()
        .env_remove(soldr_cli::reentrancy_guard::GUARD_MODE_ENV)
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unsanctioned re-entry must be rejected without any opt-in; \
         stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A typo must not silently disable a safety check.
#[test]
fn an_unrecognised_guard_value_fails_loudly() {
    let output = guarded_soldr()
        .env(soldr_cli::reentrancy_guard::GUARD_MODE_ENV, "strck")
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a recognised value"),
        "must name the bad value rather than falling back: {stderr}"
    );
}

/// An empty value means "no preference expressed", not "opt out".
#[test]
fn an_empty_guard_value_still_enforces() {
    let output = guarded_soldr()
        .env(soldr_cli::reentrancy_guard::GUARD_MODE_ENV, "")
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// soldr#2739: a marker whose writer has exited is not re-entrancy, and
/// under default-on enforcement treating it as such would be an `exit 1` on
/// a machine where nothing is wrong.
#[test]
fn a_stale_marker_from_a_dead_parent_is_allowed() {
    let dead = reaped_child_pid();
    let output = common::isolated_soldr_command()
        .env(
            soldr_cli::reentrancy_guard::IN_SOLDR_PID_ENV,
            dead.to_string(),
        )
        .arg("--version")
        .output()
        .expect("spawn soldr");
    assert!(
        output.status.success(),
        "a marker naming dead pid {dead} must not reject; stderr: {}",
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
