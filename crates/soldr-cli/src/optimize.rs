//! `soldr optimize` — scaffolding for issue #358. Real implementation
//! lands in the GREEN commit; this RED commit only ships the test
//! surface and stub function signatures so the failing tests can be
//! observed in CI.

use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use soldr_core::SoldrError;
use std::path::{Path, PathBuf};

use crate::JSON_SCHEMA_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub(crate) enum OptimizeScope {
    Global,
    Project,
    All,
}

#[derive(Debug, Args)]
pub(crate) struct OptimizeArgs {
    #[arg(long, value_enum, default_value_t = OptimizeScope::All)]
    pub(crate) scope: OptimizeScope,
    #[arg(long)]
    pub(crate) undo: bool,
    #[arg(long)]
    pub(crate) dry_run: bool,
    #[arg(long)]
    pub(crate) json: bool,
    #[arg(long, value_name = "PATH")]
    pub(crate) manifest_path: Option<PathBuf>,
    #[arg(long = "as-elevated-helper", hide = true)]
    pub(crate) as_elevated_helper: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ManagedExclusionFile {
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) exclusions: Vec<ManagedExclusion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ManagedExclusion {
    pub(crate) path: String,
    pub(crate) added_at_unix: u64,
    pub(crate) scope: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ExclusionAction {
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionStatus {
    Planned,
    Applied,
    AlreadyApplied,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct PathAction {
    pub(crate) path: String,
    pub(crate) action: ExclusionAction,
    pub(crate) scope: String,
    pub(crate) status: ActionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct OptimizeOutput {
    pub(crate) schema_version: u32,
    pub(crate) command: String,
    pub(crate) platform: String,
    pub(crate) scope: String,
    pub(crate) undo: bool,
    pub(crate) dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ci_label: Option<String>,
    pub(crate) defender_present: bool,
    pub(crate) defender_active: bool,
    pub(crate) actions: Vec<PathAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

impl OptimizeOutput {
    #[cfg(test)]
    pub(crate) fn sample() -> Self {
        Self {
            schema_version: JSON_SCHEMA_VERSION,
            command: "optimize".into(),
            platform: "Stub".into(),
            scope: "global".into(),
            undo: false,
            dry_run: true,
            ci_label: None,
            defender_present: false,
            defender_active: false,
            actions: Vec::new(),
            note: None,
        }
    }
}

pub(crate) fn resolve_project_target_dir(
    _start_dir: &Path,
    _manifest_path: Option<&Path>,
) -> Result<PathBuf, SoldrError> {
    Err(SoldrError::Other("not yet implemented".into()))
}

pub(crate) fn plan_global_paths(_root: &Path, _zccache: &Path) -> Vec<PathBuf> {
    Vec::new()
}

pub(crate) fn plan_project_paths(_workspace_root: &Path) -> Vec<PathBuf> {
    Vec::new()
}

pub(crate) fn filter_undo_entries(
    _managed: &ManagedExclusionFile,
    _current: &[String],
    _scope: Option<OptimizeScope>,
) -> Vec<String> {
    Vec::new()
}

pub(crate) fn run_optimize(_args: OptimizeArgs) -> Result<i32, SoldrError> {
    Err(SoldrError::Other("optimize: not yet implemented".into()))
}

#[cfg(test)]
#[path = "optimize_tests.rs"]
mod tests;
