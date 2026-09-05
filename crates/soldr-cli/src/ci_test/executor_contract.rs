//! The frozen executor contract `soldr ci-test` checks before running a
//! plan. Split out of `execute.rs` for the 1,000-line production ceiling.

use super::execute::stage_named;
use super::model::{CiTestPlan, Stage};
use crate::core::SoldrError;

pub(super) fn validate_executor_contract(plan: &CiTestPlan) -> Result<(), SoldrError> {
    let mut expected = vec!["rustfmt", "lint-ci", "clippy"];
    expected.extend(
        plan.stages
            .iter()
            .filter(|stage| stage.name.starts_with("dylint-library-"))
            .map(|stage| stage.name.as_str()),
    );
    expected.push("dylint-workspace");
    expected.extend(
        plan.stages
            .iter()
            .filter(|stage| stage.name.starts_with("dylint-test-"))
            .map(|stage| stage.name.as_str()),
    );
    expected.extend([
        "nextest-compile",
        "nextest",
        "doctests",
        "cargo-deny-bans",
        "cargo-audit",
        "cargo-machete",
    ]);
    let actual: Vec<&str> = plan
        .stages
        .iter()
        .map(|stage| stage.name.as_str())
        .collect();
    if actual != expected {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: executor/stage inventory drift: expected {expected:?}, got {actual:?}"
        )));
    }
    for (index, stage) in plan.stages.iter().enumerate() {
        for dependency in &stage.depends_on {
            if !plan.stages[..index]
                .iter()
                .any(|candidate| candidate.name == *dependency)
            {
                return Err(SoldrError::Other(format!(
                    "soldr ci-test: stage `{}` has missing or non-prior dependency `{dependency}`",
                    stage.name
                )));
            }
        }
    }
    let libraries: Vec<_> = plan
        .stages
        .iter()
        .filter(|stage| stage.name.starts_with("dylint-library-"))
        .collect();
    for (index, stage) in libraries.iter().enumerate() {
        let dependency = if index == 0 {
            "clippy"
        } else {
            libraries[index - 1].name.as_str()
        };
        require_dependencies(stage, &[dependency])?;
    }
    require_dependencies(
        stage_named(plan, "dylint-workspace")?,
        &[libraries
            .last()
            .ok_or_else(|| SoldrError::Other("soldr ci-test: no Dylint libraries".into()))?
            .name
            .as_str()],
    )?;
    let ui_tests: Vec<_> = plan
        .stages
        .iter()
        .filter(|stage| stage.name.starts_with("dylint-test-"))
        .collect();
    for (index, stage) in ui_tests.iter().enumerate() {
        let dependency = if index == 0 {
            "dylint-workspace"
        } else {
            ui_tests[index - 1].name.as_str()
        };
        require_dependencies(stage, &[dependency])?;
    }
    require_dependencies(stage_named(plan, "nextest-compile")?, &["clippy"])?;
    let nextest = stage_named(plan, "nextest")?;
    require_dependencies(nextest, &["nextest-compile"])?;
    if nextest.executes_compiler {
        return Err(SoldrError::Other(
            "soldr ci-test: Nextest execution must not rebuild its planned test profile after nextest-compile; nested compiler fixtures launched by tests remain allowed".into(),
        ));
    }
    let last_ui_test = &ui_tests
        .last()
        .ok_or_else(|| SoldrError::Other("soldr ci-test: no Dylint UI tests".into()))?
        .name;
    validate_tail_dependencies(&plan.stages, last_ui_test)?;
    Ok(())
}

pub(super) fn validate_tail_dependencies(
    stages: &[Stage],
    last_ui_test: &str,
) -> Result<(), SoldrError> {
    let tail_dependencies = super::plan::tail_join_dependencies(last_ui_test);
    for stage in [
        "doctests",
        "cargo-deny-bans",
        "cargo-audit",
        "cargo-machete",
    ] {
        let planned = stages
            .iter()
            .find(|candidate| candidate.name == stage)
            .ok_or_else(|| {
                SoldrError::Other(format!(
                    "soldr ci-test: frozen plan is missing stage {stage:?}"
                ))
            })?;
        require_dependencies(planned, &tail_dependencies)?;
    }
    Ok(())
}

pub(super) fn require_dependencies(stage: &Stage, expected: &[&str]) -> Result<(), SoldrError> {
    if stage
        .depends_on
        .iter()
        .map(String::as_str)
        .eq(expected.iter().copied())
    {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "soldr ci-test: stage `{}` dependency contract drift: expected {expected:?}, got {:?}",
        stage.name, stage.depends_on
    )))
}
