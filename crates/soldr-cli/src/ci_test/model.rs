//! Frozen data model for `soldr ci-test` (soldr#2867).
//!
//! The plan is deliberately a value, rather than an ad-hoc set of argv
//! builders.  CI may change how it schedules this DAG, but a host-validation
//! invocation must continue to name the same compiler domains and stages.

use serde::Serialize;

// Version 3 preserves unset Cargo/Soldr job limits as JSON null. Consumers must
// not interpret null as one: ci-test leaves the variable untouched and the
// canonical Cargo/daemon schedulers retain ownership of concurrency.
pub(crate) const PLAN_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Human,
    Json,
}

impl OutputFormat {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "human" | "text" => Some(Self::Human),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    pub(crate) packages: Vec<String>,
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
}

impl Scope {
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for package in &self.packages {
            args.extend(["--package".into(), package.clone()]);
        }
        if !self.features.is_empty() {
            args.extend(["--features".into(), self.features.join(",")]);
        }
        if self.all_features {
            args.push("--all-features".into());
        }
        if self.no_default_features {
            args.push("--no-default-features".into());
        }
        args
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) explain: bool,
    pub(crate) format: OutputFormat,
    pub(crate) scope: Scope,
    pub(crate) requested_target: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CiTestPlan {
    pub(crate) schema_version: u32,
    pub(crate) command: &'static str,
    pub(crate) workspace_root: String,
    pub(crate) workspace_metadata: WorkspaceMetadata,
    pub(crate) host_triple: String,
    pub(crate) scope: PlanScope,
    pub(crate) domains: Vec<CompileDomain>,
    pub(crate) stages: Vec<Stage>,
    pub(crate) subsumed_steps: Vec<SubsumedStep>,
    pub(crate) cook: CookDecision,
    pub(crate) resource_limits: ResourceLimits,
    /// Integration-test *link targets* in the workspace, and the count above
    /// which planning shouts (soldr#2936).
    ///
    /// Additive fields on schema v1: a v1 consumer reads the payload it always
    /// read, so the version does not move. They are on the plan rather than
    /// computed by the renderer because the census is a planning observation —
    /// `--explain-plan --format json` has to report the same number the
    /// warning was derived from.
    pub(crate) test_target_count: u64,
    pub(crate) test_target_warn_threshold: u64,
    pub(crate) dylint_target_trees: DylintTargetTrees,
    /// Counts are intentionally present before Phase 6 enforces them. Cargo
    /// remains freshness authority, so explain mode cannot invent outcomes.
    pub(crate) compiler_execution_groups: Vec<CompilerExecutionGroup>,
    pub(crate) observability: Observability,
}

#[derive(Debug, Serialize)]
pub(crate) struct PlanScope {
    pub(crate) packages: Vec<String>,
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceMetadata {
    pub(crate) manifest_path: String,
    pub(crate) lockfile_path: String,
    pub(crate) cargo_config: Vec<String>,
    pub(crate) fingerprint: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DylintTargetTrees {
    pub(crate) libraries: String,
    pub(crate) analysis: String,
    pub(crate) tests: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CompileDomain {
    pub(crate) id: &'static str,
    pub(crate) family: &'static str,
    pub(crate) toolchain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) compiler_release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) compiler_commit: Option<String>,
    pub(crate) target_triple: String,
    pub(crate) target_directory: String,
    pub(crate) profile: &'static str,
    pub(crate) rustflags: Option<String>,
    pub(crate) cargo_config: Vec<String>,
    pub(crate) wrapper_identity: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Stage {
    pub(crate) name: String,
    pub(crate) domain: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) command: Vec<String>,
    pub(crate) working_directory: String,
    pub(crate) depends_on: Vec<String>,
    pub(crate) concurrency_group: Option<&'static str>,
    /// Whether the planned stage command builds its own Cargo graph. This does
    /// not classify compiler fixtures intentionally launched by test bodies.
    pub(crate) executes_compiler: bool,
    pub(crate) metrics: StageMetrics,
}

/// Explain mode describes the metrics slots without fabricating Cargo's
/// freshness or zccache observations. Execution fills the human diagnostics
/// as stages finish; persisted metrics are deliberately out of scope here.
#[derive(Debug, Serialize)]
pub(crate) struct StageMetrics {
    pub(crate) wall_time_ms: Option<u64>,
    pub(crate) bytes: Option<u64>,
    pub(crate) zccache_counters: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SubsumedStep {
    pub(crate) name: &'static str,
    pub(crate) subsumed_by: &'static str,
    pub(crate) reason: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct CookDecision {
    pub(crate) action: &'static str,
    pub(crate) reason: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResourceLimits {
    /// Exact inherited value, or null when ci-test must not stamp the child.
    pub(crate) cargo_build_jobs: Option<String>,
    /// Exact inherited value, or null when ci-test must not stamp the child.
    pub(crate) soldr_jobs: Option<String>,
    pub(crate) nextest_test_threads: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CompilerExecutionGroup {
    pub(crate) id: &'static str,
    pub(crate) domain: &'static str,
    pub(crate) stages: Vec<String>,
    pub(crate) fresh_dirty: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct Observability {
    pub(crate) freshness_authority: &'static str,
    pub(crate) zccache_counters: &'static str,
    pub(crate) stage_wall_time: &'static str,
    pub(crate) stage_bytes: &'static str,
}
