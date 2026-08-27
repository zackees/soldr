use super::*;

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
fn parallel_failure_cancels_the_sibling_process_tree() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // The fixture uses the POSIX shell. Windows process-tree
        // cancellation is covered by soldr-platform's native tests.
        return;
    }

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

    fn child(script: &str) -> Child {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        crate::cargo_front_door::configure_cargo_child_for_timeout(&mut command);
        command
            .env(
                crate::cargo_front_door::INHERIT_PARENT_PROCESS_GROUP_ENV,
                "1",
            )
            .spawn()
            .expect("spawn cancellation fixture")
    }

    let failed = test_stage("failed");
    let sibling = test_stage("sibling");
    let started = std::time::Instant::now();
    let mut children = vec![(&failed, child("exit 73")), (&sibling, child("sleep 30"))];

    assert_eq!(wait_parallel(&mut children).unwrap(), 73);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the sibling process tree was not canceled promptly"
    );
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
