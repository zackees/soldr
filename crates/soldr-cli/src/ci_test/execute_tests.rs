use super::*;
use std::collections::BTreeMap;

fn test_stage(name: &str) -> Stage {
    Stage {
        name: name.into(),
        domain: "policy",
        kind: "test",
        command: Vec::new(),
        working_directory: String::new(),
        depends_on: Vec::new(),
        concurrency_group: Some("test"),
        executes_compiler: false,
        metrics: super::super::model::StageMetrics {
            wall_time_ms: None,
            bytes: None,
            zccache_counters: None,
        },
    }
}

fn shell_child(script: &str, fixture_dir: Option<&std::path::Path>) -> Child {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    if let Some(directory) = fixture_dir {
        command.env("FIXTURE_DIR", directory);
    }
    crate::cargo_front_door::configure_cargo_child_for_timeout(&mut command);
    command
        .env(
            crate::cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV,
            "1",
        )
        .spawn()
        .expect("spawn process fixture")
}

fn posix_fixture_available() -> bool {
    crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
}

struct ScriptSpawner<'a> {
    directory: &'a std::path::Path,
    scripts: BTreeMap<String, String>,
}

impl StageSpawner for ScriptSpawner<'_> {
    fn spawn_stage(&self, stage: &Stage) -> Result<Child, SoldrError> {
        let script = self.scripts.get(&stage.name).ok_or_else(|| {
            SoldrError::Other(format!("missing fixture script for {}", stage.name))
        })?;
        if script == "__spawn_error__" {
            return Err(SoldrError::Other(format!(
                "fixture spawn failure for {}",
                stage.name
            )));
        }
        Ok(shell_child(script, Some(self.directory)))
    }
}

struct NoopVerifier;

impl DylintBranchVerifier for NoopVerifier {
    fn libraries_complete(&self) -> Result<(), SoldrError> {
        Ok(())
    }

    fn analysis_complete(&self) -> Result<(), SoldrError> {
        Ok(())
    }

    fn ui_tests_complete(&self) -> Result<(), SoldrError> {
        Ok(())
    }
}

fn fixture_branch(ui_tests: &[Stage]) -> DylintBranch<'_> {
    DylintBranch::new(ui_tests.iter().collect()).expect("fixture Dylint branch")
}

fn fixture_compile_branch<'a>(libraries: &'a [Stage], workspace: &'a Stage) -> DylintBranch<'a> {
    DylintBranch::compilation(libraries.iter().collect(), workspace)
        .expect("fixture Dylint compilation branch")
}

#[test]
fn nonzero_stage_code_stops_the_dag() {
    assert_eq!(failure_code(0), None);
    assert_eq!(failure_code(73), Some(73));
}

#[test]
fn tail_dependency_validation_pins_every_stage_to_the_shared_join() {
    let last_ui = "dylint-test-final";
    let dependencies = ["nextest", last_ui];
    let mut stages = [
        test_stage("doctests"),
        test_stage("cargo-deny-bans"),
        test_stage("cargo-audit"),
        test_stage("cargo-machete"),
    ];
    for stage in &mut stages {
        stage.depends_on = dependencies.iter().map(|value| (*value).into()).collect();
    }

    validate_tail_dependencies(&stages, last_ui).expect("shared join is valid");

    stages[2].depends_on = vec!["doctests".into()];
    let error = validate_tail_dependencies(&stages, last_ui)
        .expect_err("the historical serial policy edge must be rejected");
    assert!(error.to_string().contains("cargo-audit"), "{error}");
}

#[test]
fn dylint_tool_path_keeps_the_exact_toolchain_path() {
    let tool_bin = PathBuf::from("managed-dylint-bin");
    let nightly_bin = PathBuf::from("exact-nightly-bin");
    let mut command = Command::new("unused");
    command.env(
        "PATH",
        std::env::join_paths([nightly_bin.as_path()]).expect("nightly path"),
    );

    prepend_command_path(&mut command, std::slice::from_ref(&tool_bin)).unwrap();

    let path = command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new("PATH"))
        .and_then(|(_, value)| value)
        .expect("configured path");
    let entries: Vec<_> = std::env::split_paths(path).collect();
    assert_eq!(entries, [tool_bin, nightly_bin]);
}

#[test]
fn stage_children_cannot_checkpoint_the_shared_ci_test_daemon() {
    let mut command = Command::new("unused");
    command.env(crate::zccache::SOLDR_CACHE_LIFECYCLE_ENV_VAR, "command");
    command.env(
        crate::zccache::SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR,
        "30",
    );

    configure_stage_cache_lifecycle(&mut command);

    let lifecycle = command
        .get_envs()
        .find(|(key, _)| {
            *key == std::ffi::OsStr::new(crate::zccache::SOLDR_CACHE_LIFECYCLE_ENV_VAR)
        })
        .and_then(|(_, value)| value);
    assert_eq!(lifecycle, Some(std::ffi::OsStr::new("job")));
    assert!(command.get_envs().any(|(key, value)| {
        key == std::ffi::OsStr::new(crate::zccache::SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR)
            && value.is_none()
    }));
}

#[test]
fn unset_job_limits_are_not_stamped_on_stage_children() {
    let mut command = Command::new("unused");

    apply_stage_resource_limits(&mut command, None, None);

    assert!(!command.get_envs().any(|(key, _)| {
        key == std::ffi::OsStr::new("CARGO_BUILD_JOBS") || key == std::ffi::OsStr::new("SOLDR_JOBS")
    }));
}

#[test]
fn explicit_job_limits_are_stamped_without_normalization() {
    let mut command = Command::new("unused");

    apply_stage_resource_limits(&mut command, Some("02"), Some("7"));

    let env: BTreeMap<_, _> = command
        .get_envs()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned())))
        .collect();
    assert_eq!(
        env.get(std::ffi::OsStr::new("CARGO_BUILD_JOBS")),
        Some(&std::ffi::OsString::from("02"))
    );
    assert_eq!(
        env.get(std::ffi::OsStr::new("SOLDR_JOBS")),
        Some(&std::ffi::OsString::from("7"))
    );
}

#[test]
fn nextest_execution_injects_the_cargo_restoring_test_runner() {
    let directory = tempfile::tempdir().unwrap();
    let runner = directory.path().join("soldr-ci-test-runner");
    std::fs::write(&runner, b"runner").unwrap();
    let mut command = Command::new("unused");

    configure_nextest_test_cargo_runner(
        &mut command,
        &test_stage("nextest"),
        "x86_64-unknown-linux-gnu",
        Some(&runner),
    )
    .unwrap();

    assert!(command.get_envs().any(|(key, value)| {
        key == std::ffi::OsStr::new("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER")
            && value == Some(runner.as_os_str())
    }));
}

#[test]
fn cargo_restoring_runner_is_not_injected_into_nextest_compilation() {
    let directory = tempfile::tempdir().unwrap();
    let runner = directory.path().join("soldr-ci-test-runner");
    std::fs::write(&runner, b"runner").unwrap();
    let mut command = Command::new("unused");

    configure_nextest_test_cargo_runner(
        &mut command,
        &test_stage("nextest-compile"),
        "x86_64-unknown-linux-gnu",
        Some(&runner),
    )
    .unwrap();

    assert!(!command
        .get_envs()
        .any(|(key, _)| key.to_string_lossy().ends_with("_RUNNER")));
}

#[test]
fn parallel_failure_cancels_the_sibling_process_tree() {
    if !posix_fixture_available() {
        // The fixture uses the POSIX shell. Windows process-tree
        // cancellation is covered by soldr-platform's native tests.
        return;
    }

    let failed = test_stage("failed");
    let sibling = test_stage("sibling");
    let started = std::time::Instant::now();
    let mut children = vec![
        (&failed, shell_child("exit 73", None)),
        (&sibling, shell_child("sleep 30", None)),
    ];

    assert_eq!(wait_parallel(&mut children).unwrap(), 73);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the sibling process tree was not canceled promptly"
    );
}

/// RED for soldr#3024: after the Nextest + Dylint join, doctests and the
/// independent policy tools must all enter the tail before any one can finish.
#[test]
fn doctests_and_policy_tail_really_overlap() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("barrier directory");
    let stages = [
        test_stage("doctests"),
        test_stage("cargo-deny-bans"),
        test_stage("cargo-audit"),
        test_stage("cargo-machete"),
    ];
    let scripts = stages
        .iter()
        .map(|stage| {
            let marker = format!("{}-started", stage.name);
            let peers = stages
                .iter()
                .map(|peer| format!("[ -f \"$FIXTURE_DIR/{}-started\" ]", peer.name))
                .collect::<Vec<_>>()
                .join(" && ");
            (
                stage.name.clone(),
                format!(
                    "touch \"$FIXTURE_DIR/{marker}\"; i=0; while [ \"$i\" -lt 100 ]; do {peers} && exit 0; i=$((i + 1)); sleep 0.02; done; exit 73"
                ),
            )
        })
        .collect();
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let stage_refs = stages.iter().collect::<Vec<_>>();

    assert_eq!(run_stage_group(&spawner, &stage_refs).unwrap(), 0);
    for stage in &stages {
        assert!(directory
            .path()
            .join(format!("{}-started", stage.name))
            .is_file());
    }
}

#[test]
fn failed_policy_tail_stage_cancels_doctest_process_tree() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("cancellation directory");
    let stages = [test_stage("doctests"), test_stage("cargo-audit")];
    let scripts = BTreeMap::from([
        (
            "doctests".into(),
            "touch \"$FIXTURE_DIR/doctests-started\"; sleep 30; touch \"$FIXTURE_DIR/doctests-finished\"".into(),
        ),
        (
            "cargo-audit".into(),
            "i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/doctests-started\" ] && exit 73; i=$((i + 1)); sleep 0.02; done; exit 74".into(),
        ),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let stage_refs = stages.iter().collect::<Vec<_>>();
    let started = Instant::now();

    assert_eq!(run_stage_group(&spawner, &stage_refs).unwrap(), 73);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!directory.path().join("doctests-finished").exists());
}

/// RED for soldr#3024: compiler-bearing stable and nightly branches may run
/// concurrently because the daemon's shared/exclusive admission sees both of
/// them. The join must happen before Fresh Nextest starts, so ordinary test
/// processes never overlap the exclusive `soldr_cli --test` nightly link.
#[test]
fn nextest_compilation_and_dylint_compilation_really_overlap() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("barrier directory");
    let nextest_compile = test_stage("nextest-compile");
    let libraries = [test_stage("dylint-library-one")];
    let workspace = test_stage("dylint-workspace");
    let scripts = BTreeMap::from([
        (
            "nextest-compile".into(),
            "touch \"$FIXTURE_DIR/nextest-compile-started\"; i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/dylint-compile-started\" ] && exit 0; i=$((i + 1)); sleep 0.02; done; exit 73".into(),
        ),
        (
            "dylint-library-one".into(),
            "touch \"$FIXTURE_DIR/dylint-compile-started\"; i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/nextest-compile-started\" ] && exit 0; i=$((i + 1)); sleep 0.02; done; exit 74".into(),
        ),
        ("dylint-workspace".into(), "exit 0".into()),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };

    let code = supervise_parallel_stage_and_dylint(
        &spawner,
        &nextest_compile,
        fixture_compile_branch(&libraries, &workspace),
        &NoopVerifier,
    )
    .expect("parallel compilation supervisor");

    assert_eq!(code, 0);
    assert!(directory.path().join("nextest-compile-started").is_file());
    assert!(directory.path().join("dylint-compile-started").is_file());
}

#[test]
fn nextest_execution_and_dylint_ui_really_start_before_either_branch_can_finish() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("barrier directory");
    let nextest = test_stage("nextest");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        (
            "nextest".into(),
            "touch \"$FIXTURE_DIR/nextest-started\"; i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/dylint-started\" ] && exit 0; i=$((i + 1)); sleep 0.02; done; exit 73".into(),
        ),
        (
            "dylint-test-one".into(),
            "touch \"$FIXTURE_DIR/dylint-started\"; i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/nextest-started\" ] && exit 0; i=$((i + 1)); sleep 0.02; done; exit 74".into(),
        ),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };

    let code =
        supervise_nextest_and_dylint(&spawner, &nextest, fixture_branch(&ui_tests), &NoopVerifier)
            .expect("parallel branch supervisor");

    assert_eq!(code, 0);
    assert!(directory.path().join("nextest-started").is_file());
    assert!(directory.path().join("dylint-started").is_file());
}

#[test]
fn nextest_failure_cancels_dylint_and_starts_no_later_stage() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("cancellation directory");
    let nextest = test_stage("nextest");
    let ui_tests = [test_stage("dylint-test-one"), test_stage("dylint-test-two")];
    let scripts = BTreeMap::from([
        ("nextest".into(), "exit 73".into()),
        (
            "dylint-test-one".into(),
            "touch \"$FIXTURE_DIR/dylint-started\"; sleep 30".into(),
        ),
        (
            "dylint-test-two".into(),
            "touch \"$FIXTURE_DIR/should-not-start\"".into(),
        ),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let code =
        supervise_nextest_and_dylint(&spawner, &nextest, fixture_branch(&ui_tests), &NoopVerifier)
            .expect("Nextest failure result");

    assert_eq!(code, 73);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!directory.path().join("should-not-start").exists());
}

#[test]
fn dylint_failure_cancels_nextest_and_starts_no_later_stage() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("cancellation directory");
    let nextest = test_stage("nextest");
    let ui_tests = [test_stage("dylint-test-one"), test_stage("dylint-test-two")];
    let scripts = BTreeMap::from([
        (
            "nextest".into(),
            "touch \"$FIXTURE_DIR/nextest-started\"; sleep 30".into(),
        ),
        ("dylint-test-one".into(), "exit 74".into()),
        (
            "dylint-test-two".into(),
            "touch \"$FIXTURE_DIR/should-not-start\"".into(),
        ),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let code =
        supervise_nextest_and_dylint(&spawner, &nextest, fixture_branch(&ui_tests), &NoopVerifier)
            .expect("Dylint failure result");

    assert_eq!(code, 74);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!directory.path().join("should-not-start").exists());
}

#[test]
fn branch_join_waits_for_both_terminal_stages() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("join directory");
    let nextest = test_stage("nextest");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        (
            "nextest".into(),
            "sleep 0.8; touch \"$FIXTURE_DIR/nextest-done\"".into(),
        ),
        (
            "dylint-test-one".into(),
            "touch \"$FIXTURE_DIR/dylint-done\"".into(),
        ),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let code =
        supervise_nextest_and_dylint(&spawner, &nextest, fixture_branch(&ui_tests), &NoopVerifier)
            .expect("branch join");

    assert_eq!(code, 0);
    assert!(started.elapsed() >= Duration::from_millis(700));
    assert!(directory.path().join("nextest-done").is_file());
    assert!(directory.path().join("dylint-done").is_file());
}

#[test]
fn second_branch_spawn_failure_cancels_the_first_branch() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("spawn directory");
    let nextest = test_stage("nextest");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        ("nextest".into(), "sleep 30".into()),
        ("dylint-test-one".into(), "__spawn_error__".into()),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let error =
        supervise_nextest_and_dylint(&spawner, &nextest, fixture_branch(&ui_tests), &NoopVerifier)
            .expect_err("fixture spawn must fail");

    assert!(error.to_string().contains("fixture spawn failure"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn report_summary_groups_repeated_normalized_identities() {
    let directory = tempfile::tempdir().expect("report directory");
    let path = directory.path().join("events.jsonl");
    std::fs::write(
        &path,
        concat!(
            "{\"identity\":{\"digest\":\"same\"}}\n",
            "{\"identity\":{\"digest\":\"other\"}}\n",
            "{\"identity\":{\"digest\":\"same\"}}\n"
        ),
    )
    .expect("write report");
    let report = summarize_compiler_report(&path).expect("read report");
    assert_eq!(report.compiler_executions, 3);
    assert_eq!(report.unique_identities, 2);
    assert_eq!(report.duplicate_executions, 1);
    assert_eq!(report.duplicates[0].identity.digest, "same");
    assert_eq!(report.duplicates[0].executions, 2);
}

#[test]
fn material_artifacts_skip_bookkeeping_and_list_real_files_in_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let target = root.path().join("target");
    std::fs::create_dir_all(target.join("debug/deps")).expect("dirs");
    std::fs::write(target.join(".rustc_info.json"), "{}").expect("write");
    std::fs::write(target.join("CACHEDIR.TAG"), "tag").expect("write");
    std::fs::write(target.join("debug/.cargo-lock"), "").expect("write");
    assert!(material_artifacts(&target, 40).expect("scan").is_empty());
    assert!(material_artifacts(root.path().join("absent").as_path(), 40)
        .expect("absent is empty")
        .is_empty());

    std::fs::write(target.join("debug/deps/libfoo.rlib"), "rlib!").expect("write");
    std::fs::write(target.join("debug/build.log"), "xx").expect("write");
    let found = material_artifacts(&target, 40).expect("scan");
    let names: Vec<String> = found
        .iter()
        .map(|(path, bytes)| {
            format!(
                "{}={bytes}",
                path.strip_prefix(&target).expect("under target").display()
            )
        })
        .collect();
    assert_eq!(names, ["debug/build.log=2", "debug/deps/libfoo.rlib=5"]);
    assert_eq!(material_artifacts(&target, 1).expect("scan").len(), 1);
}
