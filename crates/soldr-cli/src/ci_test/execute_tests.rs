use super::*;

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

fn fixture_branch<'a>(
    libraries: &'a [Stage],
    workspace: &'a Stage,
    ui_tests: &'a [Stage],
) -> DylintBranch<'a> {
    DylintBranch::new(
        libraries.iter().collect(),
        workspace,
        ui_tests.iter().collect(),
    )
    .expect("fixture Dylint branch")
}

#[test]
fn nonzero_stage_code_stops_the_dag() {
    assert_eq!(failure_code(0), None);
    assert_eq!(failure_code(73), Some(73));
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

#[test]
fn nextest_execution_and_dylint_really_start_before_either_branch_can_finish() {
    if !posix_fixture_available() {
        return;
    }
    let directory = tempfile::tempdir().expect("barrier directory");
    let nextest = test_stage("nextest");
    let libraries = [test_stage("dylint-library-one")];
    let workspace = test_stage("dylint-workspace");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        (
            "nextest".into(),
            "touch \"$FIXTURE_DIR/nextest-started\"; i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/dylint-started\" ] && exit 0; i=$((i + 1)); sleep 0.02; done; exit 73".into(),
        ),
        (
            "dylint-library-one".into(),
            "touch \"$FIXTURE_DIR/dylint-started\"; i=0; while [ \"$i\" -lt 100 ]; do [ -f \"$FIXTURE_DIR/nextest-started\" ] && exit 0; i=$((i + 1)); sleep 0.02; done; exit 74".into(),
        ),
        ("dylint-workspace".into(), "exit 0".into()),
        ("dylint-test-one".into(), "exit 0".into()),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };

    let code = supervise_nextest_and_dylint(
        &spawner,
        &nextest,
        fixture_branch(&libraries, &workspace, &ui_tests),
        &NoopVerifier,
    )
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
    let libraries = [test_stage("dylint-library-one")];
    let workspace = test_stage("dylint-workspace");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        ("nextest".into(), "exit 73".into()),
        (
            "dylint-library-one".into(),
            "touch \"$FIXTURE_DIR/dylint-started\"; sleep 30".into(),
        ),
        (
            "dylint-workspace".into(),
            "touch \"$FIXTURE_DIR/should-not-start\"".into(),
        ),
        ("dylint-test-one".into(), "exit 0".into()),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let code = supervise_nextest_and_dylint(
        &spawner,
        &nextest,
        fixture_branch(&libraries, &workspace, &ui_tests),
        &NoopVerifier,
    )
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
    let libraries = [test_stage("dylint-library-one")];
    let workspace = test_stage("dylint-workspace");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        (
            "nextest".into(),
            "touch \"$FIXTURE_DIR/nextest-started\"; sleep 30".into(),
        ),
        ("dylint-library-one".into(), "exit 74".into()),
        (
            "dylint-workspace".into(),
            "touch \"$FIXTURE_DIR/should-not-start\"".into(),
        ),
        ("dylint-test-one".into(), "exit 0".into()),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let code = supervise_nextest_and_dylint(
        &spawner,
        &nextest,
        fixture_branch(&libraries, &workspace, &ui_tests),
        &NoopVerifier,
    )
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
    let libraries = [test_stage("dylint-library-one")];
    let workspace = test_stage("dylint-workspace");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        (
            "nextest".into(),
            "sleep 0.8; touch \"$FIXTURE_DIR/nextest-done\"".into(),
        ),
        ("dylint-library-one".into(), "exit 0".into()),
        ("dylint-workspace".into(), "exit 0".into()),
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

    let code = supervise_nextest_and_dylint(
        &spawner,
        &nextest,
        fixture_branch(&libraries, &workspace, &ui_tests),
        &NoopVerifier,
    )
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
    let libraries = [test_stage("dylint-library-one")];
    let workspace = test_stage("dylint-workspace");
    let ui_tests = [test_stage("dylint-test-one")];
    let scripts = BTreeMap::from([
        ("nextest".into(), "sleep 30".into()),
        ("dylint-library-one".into(), "__spawn_error__".into()),
        ("dylint-workspace".into(), "exit 0".into()),
        ("dylint-test-one".into(), "exit 0".into()),
    ]);
    let spawner = ScriptSpawner {
        directory: directory.path(),
        scripts,
    };
    let started = Instant::now();

    let error = supervise_nextest_and_dylint(
        &spawner,
        &nextest,
        fixture_branch(&libraries, &workspace, &ui_tests),
        &NoopVerifier,
    )
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
