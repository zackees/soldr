use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) fn summarize_compiler_report(
    path: &std::path::Path,
) -> std::io::Result<CompilerRunReport> {
    let contents = std::fs::read_to_string(path)?;
    let mut groups: BTreeMap<String, Vec<CompilerEvent>> = BTreeMap::new();
    for line in contents.lines() {
        if let Ok(event) = serde_json::from_str::<CompilerEvent>(line) {
            groups
                .entry(event.identity.digest.clone())
                .or_default()
                .push(event);
        }
    }
    let compiler_executions = groups.values().map(Vec::len).sum();
    let duplicates: Vec<DuplicateCompilerIdentity> = groups
        .values()
        .filter(|events| events.len() > 1)
        .map(|events| DuplicateCompilerIdentity {
            identity: events[0].identity.clone(),
            executions: events.len(),
            stages: events
                .iter()
                .filter_map(|event| event.stage.clone())
                .collect(),
        })
        .collect();
    let duplicate_executions = duplicates
        .iter()
        .map(|duplicate| duplicate.executions.saturating_sub(1))
        .sum();
    Ok(CompilerRunReport {
        schema_version: 1,
        compiler_executions,
        unique_identities: groups.len(),
        duplicate_executions,
        duplicates,
    })
}

pub(super) fn write_compiler_run_report(
    path: &std::path::Path,
    report: &CompilerRunReport,
) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

#[derive(Debug, Serialize)]
pub(super) struct CompilerRunReport {
    pub(super) schema_version: u32,
    pub(super) compiler_executions: usize,
    pub(super) unique_identities: usize,
    pub(super) duplicate_executions: usize,
    pub(super) duplicates: Vec<DuplicateCompilerIdentity>,
}

#[derive(Debug, Serialize)]
pub(super) struct DuplicateCompilerIdentity {
    pub(super) identity: CompilerIdentity,
    pub(super) executions: usize,
    pub(super) stages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CompilerEvent {
    pub(super) stage: Option<String>,
    pub(super) identity: CompilerIdentity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct CompilerIdentity {
    pub(super) digest: String,
    #[serde(flatten)]
    pub(super) fields: BTreeMap<String, serde_json::Value>,
}
