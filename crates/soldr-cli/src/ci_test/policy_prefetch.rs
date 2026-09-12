//! Prefetch the policy tail's tool binaries while the long stages run (soldr#3143).
//!
//! The tail group — `doctests`, `cargo-deny-bans`, `cargo-audit`,
//! `cargo-machete` — waits on the Nextest + Dylint join. The three policy
//! stages then each fetch their tool from GitHub Releases at the very end of
//! the gating job: on main run 34715053480 all three still logged
//! `downloaded cargo-…` in the seconds after `nextest` exited.
//!
//! Those downloads depend on nothing any earlier stage produces, so this starts
//! them at the top of the run and joins before the tail. Two effects:
//!
//! * the tail stops paying the downloads on the critical path — small, since
//!   soldr#3143 measured the whole tail at 8.6 s; and
//! * a flaky GitHub Releases fetch surfaces while the run is starting, instead
//!   of after the rest of a gating job has already succeeded. That is the
//!   larger benefit.
//!
//! # What this deliberately does not change
//!
//! The DAG. `execute::validate_tail_dependencies` still pins the policy stages
//! to the shared join, and each stage still resolves its own tool — this only
//! warms the cache it reads. A failed prefetch is reported and otherwise
//! ignored: the stage then fetches as it always has and reports the error with
//! full context, so a best-effort prefetch can never turn a green run red.
//!
//! # Why the tool list and rules come from elsewhere
//!
//! The list is derived from the frozen plan's `policy` stages rather than
//! hard-coded, and the version and PATH-deferral rules are the cargo front
//! door's own (`managed_subcommand_version`, `path_deferred_subcommand_tool`).
//! A second copy of either would drift, and drift here is silent: a prefetch
//! that downloads a version nobody reads.

use super::model::Stage;
use crate::fetch::known_tools::ToolSpec;

/// The known managed tools the plan's `policy` stages will fetch, in stage
/// order, deduplicated by crate.
pub(super) fn policy_tool_specs(stages: &[Stage]) -> Vec<&'static ToolSpec> {
    let mut specs: Vec<&'static ToolSpec> = Vec::new();
    for stage in stages.iter().filter(|stage| stage.domain == "policy") {
        let Some(sub) = cargo_subcommand(&stage.command) else {
            continue;
        };
        let Some(spec) = crate::fetch::lookup_by_cargo_subcommand(sub) else {
            continue;
        };
        if !specs
            .iter()
            .any(|known| known.crate_name == spec.crate_name)
        {
            specs.push(spec);
        }
    }
    specs
}

/// `<sub>` from a planned `soldr cargo <sub> …` command.
fn cargo_subcommand(command: &[String]) -> Option<&str> {
    match command {
        [program, cargo, sub, ..] if program == "soldr" && cargo == "cargo" => Some(sub),
        _ => None,
    }
}

/// A started prefetch. Join it before the tail group so the policy stages find
/// the cache warm and never race it for the package-cache lock.
pub(super) struct PolicyPrefetch {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl PolicyPrefetch {
    /// Start fetching every managed policy tool the plan needs that the cargo
    /// front door would not instead take from `PATH`.
    pub(super) fn start(stages: &[Stage]) -> Self {
        let specs: Vec<&'static ToolSpec> = policy_tool_specs(stages)
            .into_iter()
            .filter(|spec| {
                spec.cargo_subcommand.is_some_and(|sub| {
                    crate::cargo_front_door::path_deferred_subcommand_tool(sub).is_none()
                })
            })
            .collect();
        if specs.is_empty() {
            return Self { task: None };
        }
        Self {
            task: Some(tokio::spawn(prefetch(specs))),
        }
    }

    /// Wait for the prefetch to finish. Never fails; see the module docs.
    pub(super) async fn join(self) {
        if let Some(task) = self.task {
            let _ = task.await;
        }
    }
}

async fn prefetch(specs: Vec<&'static ToolSpec>) {
    let paths = match crate::core::SoldrPaths::new() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!(
                "soldr ci-test: policy tool prefetch skipped ({error}); the stages will fetch their own"
            );
            return;
        }
    };
    for spec in specs {
        let version = crate::cargo_front_door::managed_subcommand_version(spec);
        match crate::fetch::fetch_tool_for_host_with_paths(spec.crate_name, &version, &paths).await
        {
            Ok(result) if !result.cached => eprintln!(
                "soldr ci-test: prefetched {} v{} for the policy tail",
                spec.crate_name, result.version
            ),
            Ok(_) => {}
            Err(error) => eprintln!(
                "soldr ci-test: prefetch of {} failed ({error}); its stage will fetch it and report",
                spec.crate_name
            ),
        }
    }
}

#[cfg(test)]
#[path = "policy_prefetch_tests.rs"]
mod tests;
