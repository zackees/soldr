//! soldr#3143: which tools the policy-tail prefetch warms.

use super::*;

fn stage(name: &str, domain: &'static str, command: &[&str]) -> Stage {
    Stage {
        name: name.into(),
        domain,
        kind: "policy",
        command: command.iter().map(|part| (*part).to_string()).collect(),
        working_directory: String::new(),
        depends_on: Vec::new(),
        concurrency_group: Some("policy"),
        executes_compiler: false,
        metrics: super::super::model::StageMetrics {
            wall_time_ms: None,
            bytes: None,
            zccache_counters: None,
        },
    }
}

fn crate_names(stages: &[Stage]) -> Vec<&'static str> {
    policy_tool_specs(stages)
        .iter()
        .map(|spec| spec.crate_name)
        .collect()
}

/// The three planned policy stages resolve, through the same registry lookup
/// the cargo front door uses, to the three crates the tail downloads.
#[test]
fn planned_policy_stages_resolve_to_their_managed_crates() {
    let stages = [
        stage(
            "cargo-deny-bans",
            "policy",
            &["soldr", "cargo", "deny", "check", "bans"],
        ),
        stage("cargo-audit", "policy", &["soldr", "cargo", "audit"]),
        stage("cargo-machete", "policy", &["soldr", "cargo", "machete"]),
    ];
    assert_eq!(
        crate_names(&stages),
        ["cargo-deny", "cargo-audit", "cargo-machete"]
    );
}

/// Only the policy tail is prefetched. `nextest` is a known managed tool too,
/// but its stage is not in the tail and is not this change's concern.
#[test]
fn stages_outside_the_policy_domain_are_ignored() {
    let stages = [
        stage("nextest", "stable", &["soldr", "cargo", "nextest", "run"]),
        stage("cargo-audit", "policy", &["soldr", "cargo", "audit"]),
    ];
    assert_eq!(crate_names(&stages), ["cargo-audit"]);
}

/// A policy stage that is not a `soldr cargo <sub>` command, or names a
/// subcommand soldr does not manage, contributes nothing rather than guessing.
#[test]
fn unrecognised_policy_commands_contribute_nothing() {
    let stages = [
        stage("shell", "policy", &["sh", "-c", "true"]),
        stage("bare-cargo", "policy", &["cargo", "deny", "check"]),
        stage(
            "unknown",
            "policy",
            &["soldr", "cargo", "not-a-managed-tool"],
        ),
        stage("too-short", "policy", &["soldr", "cargo"]),
    ];
    assert!(crate_names(&stages).is_empty());
}

/// Two stages using one tool download it once.
#[test]
fn a_tool_shared_by_several_policy_stages_is_fetched_once() {
    let stages = [
        stage(
            "deny-bans",
            "policy",
            &["soldr", "cargo", "deny", "check", "bans"],
        ),
        stage(
            "deny-licenses",
            "policy",
            &["soldr", "cargo", "deny", "check", "licenses"],
        ),
    ];
    assert_eq!(crate_names(&stages), ["cargo-deny"]);
}

/// The prefetch shares the front door's version rule rather than copying it:
/// an unpinned tool resolves to `Latest`, a pinned one to its exact pin.
#[test]
fn the_prefetch_version_rule_is_the_front_doors() {
    let deny = crate::fetch::lookup_by_cargo_subcommand("deny").expect("deny is managed");
    assert!(matches!(
        crate::cargo_front_door::managed_subcommand_version(deny),
        crate::fetch::VersionSpec::Latest
    ));
    let nextest = crate::fetch::lookup_by_cargo_subcommand("nextest").expect("nextest is managed");
    let pin = nextest
        .pinned_version
        .expect("nextest carries a registry pin");
    assert!(matches!(
        crate::cargo_front_door::managed_subcommand_version(nextest),
        crate::fetch::VersionSpec::Exact(ref exact) if exact == pin
    ));
}
