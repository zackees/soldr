use super::model::{CiTestPlan, Stage};
use crate::core::SoldrError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

/// Execute the frozen plan in its dependency order. Only independent
/// non-compiler stages fan out. Dylint manifests are kept sequential because
/// all six intentionally share one Cargo target tree per profile/toolchain.
pub(crate) async fn run(
    plan: &CiTestPlan,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    validate_executor_contract(plan)?;
    let mut factory = StageCommandFactory::new(plan, cache_enabled, trust_inherited_soldr_env)?;
    macro_rules! stop_on_failure {
        ($result:expr) => {
            let code = $result?;
            if let Some(failure) = failure_code(code) {
                return Ok(failure);
            }
        };
    }

    stop_on_failure!(run_group(&factory, plan, &["rustfmt", "lint-ci"]));
    stop_on_failure!(run_group(&factory, plan, &["clippy"]));
    factory.prepare_dylint().await?;
    stop_on_failure!(run_named_prefix(&factory, plan, "dylint-library-"));
    verify_target_tree("Dylint library", &plan.dylint_target_trees.libraries)?;
    stop_on_failure!(run_group(&factory, plan, &["dylint-workspace"]));
    verify_target_tree("Dylint analysis", &plan.dylint_target_trees.analysis)?;
    stop_on_failure!(run_named_prefix(&factory, plan, "dylint-test-"));
    verify_dylint_test_targets(plan)?;
    stop_on_failure!(run_group(&factory, plan, &["nextest"]));
    stop_on_failure!(run_group(&factory, plan, &["doctests"]));
    run_group(
        &factory,
        plan,
        &["cargo-deny-bans", "cargo-audit", "cargo-machete"],
    )
}

fn failure_code(code: i32) -> Option<i32> {
    (code != 0).then_some(code)
}

fn validate_executor_contract(plan: &CiTestPlan) -> Result<(), SoldrError> {
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
    Ok(())
}

fn run_named_prefix(
    factory: &StageCommandFactory,
    plan: &CiTestPlan,
    prefix: &str,
) -> Result<i32, SoldrError> {
    for stage in plan
        .stages
        .iter()
        .filter(|stage| stage.name.starts_with(prefix))
    {
        let code = run_group(factory, plan, &[stage.name.as_str()])?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

fn run_group(
    factory: &StageCommandFactory,
    plan: &CiTestPlan,
    names: &[&str],
) -> Result<i32, SoldrError> {
    let started = std::time::Instant::now();
    let stages: Vec<&Stage> = names
        .iter()
        .map(|name| {
            plan.stages
                .iter()
                .find(|stage| stage.name == *name)
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "soldr ci-test: frozen plan is missing stage {name:?}"
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    if stages.len() == 1 {
        let code = wait_one(factory.spawn(stages[0])?, stages[0])?;
        eprintln!(
            "soldr ci-test: stage `{}` completed in {} ms",
            stages[0].name,
            started.elapsed().as_millis()
        );
        return Ok(code);
    }
    let mut children = Vec::with_capacity(stages.len());
    for stage in stages {
        match factory.spawn(stage) {
            Ok(child) => children.push((stage, child)),
            Err(error) => {
                cancel_remaining(&mut children, None);
                return Err(error);
            }
        }
    }
    let code = wait_parallel(&mut children)?;
    eprintln!(
        "soldr ci-test: stage group [{}] completed in {} ms",
        names.join(", "),
        started.elapsed().as_millis()
    );
    Ok(code)
}

fn wait_one(mut child: Child, stage: &Stage) -> Result<i32, SoldrError> {
    let status = child.wait()?;
    report_status(stage, &child, status);
    Ok(exit_code(status))
}

fn wait_parallel(children: &mut [(&Stage, Child)]) -> Result<i32, SoldrError> {
    let mut reported = vec![false; children.len()];
    loop {
        let mut complete = 0;
        for index in 0..children.len() {
            let failure = {
                let polled = children[index].1.try_wait();
                let status = match polled {
                    Ok(status) => status,
                    Err(error) => {
                        cancel_remaining(children, None);
                        return Err(error.into());
                    }
                };
                let (stage, child) = &mut children[index];
                if let Some(status) = status {
                    complete += 1;
                    if !reported[index] {
                        reported[index] = true;
                        report_status(stage, child, status);
                    }
                    (!status.success()).then_some((child.id(), exit_code(status)))
                } else {
                    None
                }
            };
            if let Some((failed_pid, code)) = failure {
                cancel_remaining(children, Some(failed_pid));
                return Ok(code);
            }
        }
        if complete == children.len() {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn cancel_remaining(children: &mut [(&Stage, Child)], failed_pid: Option<u32>) {
    for (stage, child) in children {
        if Some(child.id()) == failed_pid {
            continue;
        }
        if child.try_wait().ok().flatten().is_some() {
            continue;
        }
        match crate::cargo_front_door::kill_cargo_process_tree(child) {
            Ok(kind) => eprintln!(
                "soldr ci-test: stage `{}` (pid {}) {kind}",
                stage.name,
                child.id()
            ),
            Err(error) => eprintln!(
                "soldr ci-test: stage `{}` (pid {}) could not be terminated: {error}",
                stage.name,
                child.id()
            ),
        }
        if let Ok(status) = child.wait() {
            eprintln!(
                "soldr ci-test: stage `{}` (pid {}) canceled with {status}",
                stage.name,
                child.id()
            );
        }
    }
}

fn report_status(stage: &Stage, child: &Child, status: std::process::ExitStatus) {
    eprintln!(
        "soldr ci-test: stage `{}` (pid {}) exited with {status}",
        stage.name,
        child.id()
    );
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn verify_target_tree(label: &str, directory: &str) -> Result<(), SoldrError> {
    let path = std::path::Path::new(directory);
    let populated = std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    if populated {
        return Ok(());
    }
    Err(SoldrError::Other(format!(
        "soldr ci-test: {label} target tree was not populated at {}; refusing to continue with a different target domain",
        path.display()
    )))
}

fn verify_dylint_test_targets(plan: &CiTestPlan) -> Result<(), SoldrError> {
    let shared = std::path::Path::new(&plan.dylint_target_trees.tests);
    let deps = shared.join("debug").join("deps");
    verify_target_tree("Dylint UI-test dependencies", &deps.display().to_string())?;

    let dylints = std::path::Path::new(&plan.workspace_root).join("dylints");
    let entries = std::fs::read_dir(&dylints).map_err(|error| {
        SoldrError::Other(format!(
            "soldr ci-test: cannot inspect {}: {error}",
            dylints.display()
        ))
    })?;
    for entry in entries {
        let local_target = entry?.path().join("target");
        if contains_material_artifact(&local_target)? {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: Dylint UI tests created compiler artifacts in {}; all six tests must share {}",
                local_target.display(),
                shared.display()
            )));
        }
    }
    Ok(())
}

fn contains_material_artifact(path: &std::path::Path) -> Result<bool, SoldrError> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            if contains_material_artifact(&child)? {
                return Ok(true);
            }
            continue;
        }
        let name = child.file_name().and_then(|value| value.to_str());
        if !matches!(
            name,
            Some(".rustc_info.json" | "CACHEDIR.TAG" | ".cargo-lock")
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

struct StageCommandFactory {
    soldr: PathBuf,
    trust_inherited_soldr_env: bool,
    cache_enabled: bool,
    cargo_build_jobs: String,
    soldr_jobs: String,
    dylint: crate::dylint_toolchain::DylintToolchainPlan,
    dylint_bin_dirs: Vec<PathBuf>,
    dylint_env: Vec<(String, String)>,
    ci_test_report_path: PathBuf,
}

impl StageCommandFactory {
    fn new(
        plan: &CiTestPlan,
        cache_enabled: bool,
        trust_inherited_soldr_env: bool,
    ) -> Result<Self, SoldrError> {
        let dylint_domain = plan
            .domains
            .iter()
            .find(|domain| domain.id == "dylint-libraries")
            .ok_or_else(|| SoldrError::Other("soldr ci-test: missing Dylint domain".into()))?;
        let compiler_release = dylint_domain.compiler_release.clone().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: Dylint domain has no compiler release".into())
        })?;
        let compiler_commit = dylint_domain.compiler_commit.clone().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: Dylint domain has no compiler commit".into())
        })?;
        let cargo_build_jobs = plan
            .resource_limits
            .cargo_build_jobs
            .clone()
            .ok_or_else(|| {
                SoldrError::Other("soldr ci-test: plan has no Cargo job limit".into())
            })?;
        let soldr_jobs = plan.resource_limits.soldr_jobs.clone().ok_or_else(|| {
            SoldrError::Other("soldr ci-test: plan has no Soldr job limit".into())
        })?;
        let paths = crate::core::SoldrPaths::new()?;
        let report_dir = paths.cache.join("logs").join("ci-test");
        std::fs::create_dir_all(&report_dir)?;
        let ci_test_report_path = report_dir.join(format!(
            "compiler-events-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        eprintln!(
            "soldr ci-test: compiler-unit report will be written to {}",
            ci_test_report_path.display()
        );
        Ok(Self {
            soldr: crate::current_soldr_binary()?,
            trust_inherited_soldr_env,
            // The parent `--no-cache` choice is process-local. It needs to be
            // repeated for child Soldr commands, otherwise the plan would
            // silently switch wrapper identity at its first stage.
            cache_enabled,
            cargo_build_jobs,
            soldr_jobs,
            // Reconstructed from the frozen plan document rather than
            // re-derived, so it carries no precedence tier (soldr#2945).
            dylint: crate::dylint_toolchain::DylintToolchainPlan::identity(
                dylint_domain.toolchain.clone(),
                compiler_release,
                compiler_commit,
            ),
            dylint_bin_dirs: Vec::new(),
            dylint_env: Vec::new(),
            ci_test_report_path,
        })
    }

    async fn prepare_dylint(&mut self) -> Result<(), SoldrError> {
        let paths = crate::core::SoldrPaths::new()?;
        paths.ensure_dirs()?;
        let bootstrap =
            crate::cargo_front_door::ensure_known_subcommand_tool(&["dylint".to_string()], &paths)
                .await?;
        // The prebuilt driver probe needs the selected rustc's runtime
        // libraries. Install and verify the exact nightly first so a clean
        // managed rustup home cannot fail while probing an otherwise valid
        // catalogued driver.
        self.dylint = crate::dylint_toolchain::prepare_resolved(self.dylint.clone())?;
        crate::dylint_toolchain::ensure_prebuilt_driver(&self.dylint, &paths).await?;
        self.dylint_bin_dirs = bootstrap.bin_dirs;
        self.dylint_env = bootstrap.env;
        Ok(())
    }

    fn spawn(&self, stage: &Stage) -> Result<Child, SoldrError> {
        let mut command = Command::new(&self.soldr);
        if !self.cache_enabled {
            command.arg("--no-cache");
        }
        if self.trust_inherited_soldr_env {
            command.arg("--trust-inherited-soldr-env");
        }
        command.args(stage.command.iter().skip(1));
        command.current_dir(&stage.working_directory);
        command.env("CARGO_BUILD_JOBS", &self.cargo_build_jobs);
        command.env("SOLDR_JOBS", &self.soldr_jobs);
        command.env("SOLDR_CI_TEST_REPORT_PATH", &self.ci_test_report_path);
        command.env("SOLDR_CI_TEST_STAGE", &stage.name);
        if stage.domain.starts_with("dylint-") {
            for (key, value) in &self.dylint_env {
                command.env(key, value);
            }
            self.dylint.apply_to_command(&mut command);
            prepend_command_path(&mut command, &self.dylint_bin_dirs)?;
            command.env("SOLDR_NO_GC_TARGET", "1");
            command.env("SOLDR_LINKER", "default");
            // UI tests spawn nested Cargo work (trybuild/UI harnesses). Give
            // that child the same nightly-keyed target root, not Cargo's
            // ambient workspace target directory.
            if stage.name.starts_with("dylint-test-") {
                command.env("CARGO_TARGET_DIR", &self.dylint_test_target_dir(stage)?);
            }
        }
        crate::cargo_front_door::configure_cargo_child_for_timeout(&mut command);
        command.env(
            crate::cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV,
            "1",
        );
        crate::exit_guard::mark_spoke();
        command.spawn().map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: failed to start stage `{}`: {error}",
                stage.name
            ))
        })
    }

    fn dylint_test_target_dir(&self, stage: &Stage) -> Result<String, SoldrError> {
        stage
            .command
            .windows(2)
            .find(|pair| pair[0] == "--target-dir")
            .map(|pair| pair[1].clone())
            .ok_or_else(|| {
                SoldrError::Other(format!(
                    "soldr ci-test: frozen Dylint test stage `{}` has no --target-dir",
                    stage.name
                ))
            })
    }
}

impl Drop for StageCommandFactory {
    fn drop(&mut self) {
        let Ok(report) = summarize_compiler_report(&self.ci_test_report_path) else {
            eprintln!(
                "soldr ci-test: compiler-unit report has no compiler events (all units may be Fresh): {}",
                self.ci_test_report_path.display()
            );
            return;
        };
        let summary_path = self.ci_test_report_path.with_extension("summary.json");
        if let Err(error) = write_compiler_run_report(&summary_path, &report) {
            eprintln!(
                "warning: soldr ci-test: could not write compiler-unit summary {}: {error}",
                summary_path.display()
            );
        }
        eprintln!(
            "soldr ci-test: compiler-unit report: {} executions, {} identities, {} duplicate executions; {}",
            report.compiler_executions,
            report.unique_identities,
            report.duplicate_executions,
            summary_path.display()
        );
        if report.duplicate_executions != 0 {
            for duplicate in report.duplicates {
                eprintln!(
                    "warning: soldr ci-test: duplicate compiler identity {} executed {} times: {}",
                    duplicate.identity.digest,
                    duplicate.executions,
                    serde_json::to_string(&duplicate.identity).unwrap_or_default()
                );
            }
        }
    }
}

fn summarize_compiler_report(path: &std::path::Path) -> std::io::Result<CompilerRunReport> {
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

fn write_compiler_run_report(
    path: &std::path::Path,
    report: &CompilerRunReport,
) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

#[derive(Debug, Serialize)]
struct CompilerRunReport {
    schema_version: u32,
    compiler_executions: usize,
    unique_identities: usize,
    duplicate_executions: usize,
    duplicates: Vec<DuplicateCompilerIdentity>,
}

#[derive(Debug, Serialize)]
struct DuplicateCompilerIdentity {
    identity: CompilerIdentity,
    executions: usize,
    stages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompilerEvent {
    stage: Option<String>,
    identity: CompilerIdentity,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CompilerIdentity {
    digest: String,
    #[serde(flatten)]
    fields: BTreeMap<String, serde_json::Value>,
}

fn prepend_command_path(command: &mut Command, prefixes: &[PathBuf]) -> Result<(), SoldrError> {
    if prefixes.is_empty() {
        return Ok(());
    }
    let mut entries = prefixes.to_vec();
    let configured = command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new("PATH"))
        .and_then(|(_, value)| value.map(std::ffi::OsString::from));
    if let Some(path) = configured.or_else(|| std::env::var_os("PATH")) {
        entries.extend(std::env::split_paths(&path));
    }
    let joined = std::env::join_paths(entries).map_err(|error| {
        SoldrError::Other(format!("soldr ci-test: invalid Dylint PATH: {error}"))
    })?;
    command.env("PATH", joined);
    Ok(())
}

#[cfg(test)]
#[path = "execute_tests.rs"]
mod tests;
