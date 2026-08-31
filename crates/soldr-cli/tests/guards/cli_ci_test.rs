//! End-to-end acceptance contract for the frozen `soldr ci-test` surface
//! (soldr#2867).
//!
//! These tests intentionally ask only for an explanation.  A plan must be
//! cheap and side-effect free: it is the integration seam that lets CI
//! validate its chosen domains and commands without compiling this workspace
//! (or installing Dylint/nextest) a second time.

use crate::common::*;
use serde_json::Value;
use std::process::{Command, Output};

const DYLINT_RELEASE: &str = "1.89.0-nightly";
const DYLINT_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn dylint_channel() -> String {
    format!(
        "nightly-2026-05-28-{}",
        soldr_cli::pyo3_detect::host_triple()
    )
}

fn configure_dylint_identity(command: &mut Command) {
    let channel = dylint_channel();
    command
        .env("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", &channel)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE", DYLINT_RELEASE)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH", DYLINT_COMMIT)
        .env(
            "SOLDR_DYLINT_PREPARED_IDENTITY",
            format!("{channel}|{DYLINT_RELEASE}|{DYLINT_COMMIT}"),
        );
}

/// Invoke the side-effect-free plan surface with a deterministic Dylint
/// identity.  The exact host stable toolchain remains observable: hard-coding
/// it here would hide a real domain-selection regression whenever the
/// repository pin moves.
fn explain_plan(extra: &[&str]) -> Output {
    let mut command = isolated_soldr_command();
    command
        .current_dir(workspace_root())
        .args(["ci-test", "--explain-plan", "--format", "json"])
        .args(extra)
        .env_remove("CARGO_BUILD_JOBS")
        .env_remove("SOLDR_JOBS")
        .env_remove("NEXTEST_TEST_THREADS");
    configure_dylint_identity(&mut command);
    command.output().expect("run soldr ci-test --explain-plan")
}

fn plan_json(extra: &[&str]) -> Value {
    let output = explain_plan(extra);
    assert!(
        output.status.success(),
        "ci-test explain-plan failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "ci-test --explain-plan --format json must emit JSON: {error}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn object<'a>(value: &'a Value, key: &str) -> &'a serde_json::Map<String, Value> {
    value
        .get(key)
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("missing object field {key:?} in plan: {value}"))
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("missing array field {key:?} in plan: {value}"))
}

fn stage_names(plan: &Value) -> Vec<&str> {
    array(plan, "stages")
        .iter()
        .map(|stage| {
            stage
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("every stage needs a stable name: {stage}"))
        })
        .collect()
}

fn find_stage<'a>(plan: &'a Value, name: &str) -> &'a Value {
    array(plan, "stages")
        .iter()
        .find(|stage| stage.get("name").and_then(Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("missing required {name:?} stage in {plan}"))
}

#[test]
fn ci_test_is_a_native_builtin_with_a_versioned_complete_plan_schema() {
    let plan = plan_json(&[]);

    assert_eq!(
        plan["schema_version"], 2,
        "the explain-plan schema is a public contract"
    );
    assert_eq!(plan["command"], "ci-test");

    // Every item below is frozen before the command starts compiler work.  Do
    // not collapse these into opaque strings: consumers must be able to tell
    // which compile domain owns an invalidation without parsing prose.
    assert!(plan.get("workspace_root").and_then(Value::as_str).is_some());
    let workspace_metadata = object(&plan, "workspace_metadata");
    for field in ["manifest_path", "lockfile_path", "fingerprint"] {
        assert!(
            workspace_metadata
                .get(field)
                .and_then(Value::as_str)
                .is_some(),
            "workspace metadata must freeze {field:?} so a plan reveals metadata drift"
        );
    }
    assert!(workspace_metadata
        .get("cargo_config")
        .and_then(Value::as_array)
        .is_some());
    assert!(plan.get("host_triple").and_then(Value::as_str).is_some());
    assert!(plan.get("scope").and_then(Value::as_object).is_some());
    assert!(plan.get("cook").is_some());
    let resource_limits = object(&plan, "resource_limits");
    assert_eq!(resource_limits["cargo_build_jobs"], "1");
    assert_eq!(resource_limits["soldr_jobs"], "1");
    assert_eq!(resource_limits["nextest_test_threads"], "1");
    let dylint_target_trees = object(&plan, "dylint_target_trees");
    for field in ["libraries", "analysis", "tests"] {
        assert!(
            dylint_target_trees
                .get(field)
                .and_then(Value::as_str)
                .is_some(),
            "the pinned nightly must name its separate {field:?} target tree"
        );
    }
    assert!(plan
        .get("compiler_execution_groups")
        .and_then(Value::as_array)
        .is_some());
    let observability = object(&plan, "observability");
    for field in [
        "freshness_authority",
        "zccache_counters",
        "stage_wall_time",
        "stage_bytes",
    ] {
        assert!(
            observability.get(field).and_then(Value::as_str).is_some(),
            "observability must freeze {field:?}: {observability:?}"
        );
    }

    let domains = array(&plan, "domains");
    let names: Vec<_> = domains
        .iter()
        .map(|domain| {
            let object = domain
                .as_object()
                .unwrap_or_else(|| panic!("domain must be an object: {domain}"));
            for field in [
                "family",
                "toolchain",
                "target_triple",
                "target_directory",
                "profile",
                "wrapper_identity",
            ] {
                assert!(
                    object.get(field).and_then(Value::as_str).is_some(),
                    "domain must freeze {field:?}: {object:?}"
                );
            }
            assert!(object.get("rustflags").is_some());
            assert!(object
                .get("cargo_config")
                .and_then(Value::as_array)
                .is_some());
            assert!(
                object.get("compiler_release").is_some()
                    || object.get("family").and_then(Value::as_str) != Some("dylint-nightly"),
                "the Dylint nightly must carry its exact compiler release"
            );
            assert!(
                object.get("compiler_commit").is_some()
                    || object.get("family").and_then(Value::as_str) != Some("dylint-nightly"),
                "the Dylint nightly must carry its exact compiler commit"
            );
            object
                .get("id")
                .and_then(Value::as_str)
                .expect("domain needs a stable name")
        })
        .collect();
    assert!(
        names.contains(&"stable"),
        "stable compiler domain missing: {domains:?}"
    );
    for domain in ["dylint-libraries", "dylint-analysis", "dylint-ui-tests"] {
        assert!(
            names.contains(&domain),
            "Dylint's pinned nightly domain {domain:?} is missing: {domains:?}"
        );
    }
    assert!(
        names.contains(&"rustdoc"),
        "doctests must be represented as their own rustdoc family: {domains:?}"
    );
}

#[test]
fn no_cache_explain_plan_reports_the_effective_disabled_wrapper() {
    let mut command = isolated_soldr_command();
    command.current_dir(workspace_root()).args([
        "--no-cache",
        "ci-test",
        "--explain-plan",
        "--format",
        "json",
    ]);
    configure_dylint_identity(&mut command);
    let output = command.output().expect("run no-cache explain plan");
    assert!(
        output.status.success(),
        "no-cache explain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).expect("no-cache plan JSON");
    assert!(array(&plan, "domains").iter().all(|domain| {
        domain.get("wrapper_identity").and_then(Value::as_str) == Some("disabled (--no-cache)")
    }));
}

#[test]
fn ci_test_prescribes_the_ci_dag_and_exactly_one_nextest_test_compilation() {
    let plan = plan_json(&[]);
    let host = plan["host_triple"]
        .as_str()
        .expect("host triple is required in the frozen plan");
    let stages = stage_names(&plan);
    assert_eq!(
        stages,
        [
            "rustfmt",
            "lint-ci",
            "clippy",
            "dylint-library-ban_raw_process_creation",
            "dylint-library-ban_raw_network_access",
            "dylint-library-ban_raw_local_socket_name",
            "dylint-library-ban_raw_ipc_transport",
            "dylint-library-ban_platform_cfg_outside_boundary",
            "dylint-library-ban_raw_env_flag",
            "dylint-workspace",
            "dylint-test-ban_raw_process_creation",
            "dylint-test-ban_raw_network_access",
            "dylint-test-ban_raw_local_socket_name",
            "dylint-test-ban_raw_ipc_transport",
            "dylint-test-ban_platform_cfg_outside_boundary",
            "dylint-test-ban_raw_env_flag",
            "nextest-compile",
            "nextest",
            "doctests",
            "cargo-deny-bans",
            "cargo-audit",
            "cargo-machete",
        ],
        "the prescribed host-validation order is a frozen native-ci contract"
    );

    let nextest_compile = find_stage(&plan, "nextest-compile");
    assert_eq!(nextest_compile["domain"], "stable");
    assert_eq!(
        nextest_compile["command"],
        serde_json::json!([
            "soldr",
            "cargo",
            "nextest",
            "run",
            "--no-run",
            "--workspace",
            "--lib",
            "--tests",
            "--target",
            host,
            "--test-threads",
            "1"
        ]),
        "the sole test-profile compile feeds Nextest execution; do not insert a dev-profile warm-up"
    );
    assert_eq!(nextest_compile["executes_compiler"], true);
    assert_eq!(
        nextest_compile["depends_on"],
        serde_json::json!(["clippy"]),
        "Nextest compilation must become ready immediately after Clippy"
    );
    let nextest = find_stage(&plan, "nextest");
    assert_eq!(nextest["domain"], "stable");
    assert_eq!(nextest["executes_compiler"], false);
    assert_eq!(
        nextest["depends_on"],
        serde_json::json!(["nextest-compile"])
    );
    let run_args = nextest["command"].as_array().expect("Nextest run argv");
    let compile_args = nextest_compile["command"]
        .as_array()
        .expect("Nextest compile argv");
    let compile_without_no_run: Vec<_> = compile_args
        .iter()
        .filter(|arg| arg.as_str() != Some("--no-run"))
        .cloned()
        .collect();
    let run_args = run_args.to_vec();
    assert_eq!(
        compile_without_no_run, run_args,
        "Nextest execution must select exactly the binaries compiled by nextest-compile"
    );
    assert!(
        !array(&plan, "stages").iter().any(|stage| {
            stage.get("kind").and_then(Value::as_str) == Some("compiler-and-test")
                && stage
                    .get("command")
                    .and_then(Value::as_array)
                    .is_some_and(|argv| {
                        argv.iter().any(|arg| arg.as_str() == Some("--profile=dev"))
                    })
        }),
        "a dev-profile test warm-up would compile every test a second time"
    );

    let clippy = find_stage(&plan, "clippy");
    assert_eq!(clippy["domain"], "stable");
    assert_eq!(
        clippy["command"],
        serde_json::json!([
            "soldr",
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--target",
            host,
            "--",
            "-D",
            "warnings"
        ])
    );
    let dylint = find_stage(&plan, "dylint-workspace");
    assert_eq!(dylint["domain"], "dylint-analysis");
    let libraries = [
        "dylint-library-ban_raw_process_creation",
        "dylint-library-ban_raw_network_access",
        "dylint-library-ban_raw_local_socket_name",
        "dylint-library-ban_raw_ipc_transport",
        "dylint-library-ban_platform_cfg_outside_boundary",
        "dylint-library-ban_raw_env_flag",
    ];
    for (index, stage_name) in libraries.iter().enumerate() {
        let expected = if index == 0 {
            "clippy"
        } else {
            libraries[index - 1]
        };
        assert_eq!(
            find_stage(&plan, stage_name)["depends_on"],
            serde_json::json!([expected]),
            "Dylint libraries share one target tree and must remain serial"
        );
    }
    assert_eq!(
        dylint["depends_on"],
        serde_json::json!([libraries[libraries.len() - 1]])
    );
    let ui_tests = [
        "dylint-test-ban_raw_process_creation",
        "dylint-test-ban_raw_network_access",
        "dylint-test-ban_raw_local_socket_name",
        "dylint-test-ban_raw_ipc_transport",
        "dylint-test-ban_platform_cfg_outside_boundary",
        "dylint-test-ban_raw_env_flag",
    ];
    for (index, stage_name) in ui_tests.iter().enumerate() {
        let expected = if index == 0 {
            "dylint-workspace"
        } else {
            ui_tests[index - 1]
        };
        assert_eq!(
            find_stage(&plan, stage_name)["depends_on"],
            serde_json::json!([expected]),
            "Dylint UI tests share one target tree and must remain serial"
        );
    }
    let doctests = find_stage(&plan, "doctests");
    assert_eq!(doctests["domain"], "rustdoc");
    assert_eq!(
        doctests["depends_on"],
        serde_json::json!(["nextest", ui_tests[ui_tests.len() - 1]]),
        "doctests are the join after both compiler-bearing branches"
    );
    assert_eq!(
        doctests["command"],
        serde_json::json!([
            "soldr",
            "cargo",
            "test",
            "--workspace",
            "--doc",
            "--target",
            host
        ])
    );

    for stage in array(&plan, "stages") {
        assert!(
            stage.get("working_directory").and_then(Value::as_str).is_some(),
            "each command must freeze its working directory so nextest config discovery survives re-orchestration: {stage}"
        );
        let metrics = object(stage, "metrics");
        assert!(metrics.contains_key("wall_time_ms"));
        assert!(metrics.contains_key("bytes"));
        assert!(metrics.contains_key("zccache_counters"));
    }

    let subsumed = array(&plan, "subsumed_steps");
    assert!(
        subsumed.iter().any(|step| {
            step.get("name").and_then(Value::as_str) == Some("cargo check")
                && step.get("subsumed_by").and_then(Value::as_str) == Some("clippy")
        }),
        "cargo check must be reported as subsumed by clippy, never scheduled as compiler work: {subsumed:?}"
    );
}

#[test]
fn ci_test_preserves_scope_and_exposes_incompatible_overrides_as_domains_or_errors() {
    let scoped = plan_json(&["--package", "soldr-cli", "--all-features"]);
    let scope = object(&scoped, "scope");
    assert_eq!(
        scope.get("packages"),
        Some(&serde_json::json!(["soldr-cli"]))
    );
    assert_eq!(scope.get("all_features"), Some(&Value::Bool(true)));
    assert_eq!(scope.get("no_default_features"), Some(&Value::Bool(false)));
    for stage_name in [
        "clippy",
        "nextest-compile",
        "nextest",
        "doctests",
        "dylint-workspace",
    ] {
        let command = find_stage(&scoped, stage_name)["command"]
            .as_array()
            .expect("scoped stage command");
        assert!(command.iter().any(|arg| arg == "--package"));
        assert!(command.iter().any(|arg| arg == "soldr-cli"));
        assert!(
            !command.iter().any(|arg| arg == "--workspace"),
            "package scope must not retain workspace-wide selection: {command:?}"
        );
    }

    let no_default_features = plan_json(&["--no-default-features"]);
    assert_eq!(
        object(&no_default_features, "scope").get("no_default_features"),
        Some(&Value::Bool(true)),
        "--no-default-features is an allowed Cargo scope selector"
    );

    // Cargo configs and copied CI commands commonly spell the native host
    // explicitly. That must retain the stable domain rather than produce a
    // false incompatible-override error or a redundant target tree.
    let host = scoped["host_triple"]
        .as_str()
        .expect("host triple in explain plan")
        .to_owned();
    let explicit_host = plan_json(&["--target", &host]);
    assert!(
        array(&explicit_host, "domains").iter().all(|domain| {
            domain.get("target_triple").and_then(Value::as_str) == Some(host.as_str())
        }),
        "an explicit host target must preserve the frozen stable host domains: {explicit_host}"
    );

    // An explicit host-plan override must never silently share the stable
    // target tree. The native surface rejects it diagnostically; callers that
    // need an additional domain use the explicit cargo front door.
    //
    // The triple is chosen against the host rather than hard-coded. It used to
    // be a literal `aarch64-unknown-linux-gnu`, which is a *foreign* target on
    // every lane except the one where it is the host — and there it collided
    // with the assertion directly above, which requires an explicit host
    // target to be accepted. The two cannot both hold for the same triple, so
    // the aarch64 Linux target-run lane failed with "target override must not
    // silently reuse host artifacts" while doing exactly the right thing.
    let foreign = soldr_cli::core::CANONICAL_TARGETS
        .iter()
        .copied()
        .find(|candidate| *candidate != host)
        .expect("the canonical list has more than one target");
    assert_ne!(
        foreign, host,
        "the override case needs a target that is NOT the host, or it tests \
         the accepted-host path above instead"
    );
    let overridden = explain_plan(&["--target", foreign]);
    assert!(
        !overridden.status.success(),
        "target override must not silently reuse host artifacts"
    );
    let stderr = String::from_utf8_lossy(&overridden.stderr);
    assert!(
        stderr.contains("--target") && stderr.contains("frozen host-validation domain"),
        "an incompatible target override needs a domain-specific diagnostic, not an opaque failure: {stderr}"
    );
}

#[test]
fn ci_test_human_explain_plan_renders_the_same_named_domains_and_stages() {
    let mut command = isolated_soldr_command();
    command
        .current_dir(workspace_root())
        .args(["ci-test", "--explain-plan"]);
    configure_dylint_identity(&mut command);
    let output = command.output().expect("run human ci-test plan");
    assert!(
        output.status.success(),
        "human plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for required in [
        "soldr ci-test plan v2",
        "stable",
        "dylint-libraries",
        "dylint-analysis",
        "dylint-ui-tests",
        "rustdoc",
        "rustfmt",
        "nextest",
        "doctests",
    ] {
        assert!(
            stdout.contains(required),
            "human plan omitted {required:?}: {stdout}"
        );
    }
}

#[test]
fn ci_test_help_and_plan_never_take_fetch_or_bare_cargo_paths() {
    let help = isolated_soldr_command()
        .arg("--help")
        .output()
        .expect("run soldr --help");
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("ci-test"),
        "ci-test must be clap-captured before External can fetch a crate"
    );

    let output = explain_plan(&[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "native plan must succeed: {stderr}"
    );
    assert!(
        !stderr.contains("fetching ci-test") && !stderr.contains("cargo ci-test"),
        "ci-test is native orchestration, not a fetched tool or a bare-cargo passthrough: {stderr}"
    );
}

#[test]
fn typo_of_cargo_test_suggests_test_not_the_new_ci_test_verb() {
    let output = isolated_soldr_command()
        .arg("tset")
        // Strict mode makes an accidental external-tool fall-through fail
        // before any network download; the assertion is only about the hint.
        .env("SOLDR_TRUST_MODE", "strict")
        .output()
        .expect("run typo through soldr");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("test"),
        "a typo of cargo's test shorthand must retain its existing hint: {stderr}"
    );
    assert!(
        !stderr.contains("ci-test"),
        "adding ci-test must not steal fuzzy suggestions for cargo test: {stderr}"
    );
}
