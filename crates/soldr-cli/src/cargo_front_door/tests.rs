//! Unit tests for [`crate::cargo_front_door`]: the cargo argv parser, the
//! low-disk warning helper, and the cargo-subcommand sniffer.
//! Lives inside the `cargo_front_door/` module directory so `mod.rs`
//! stays comfortably under the 1000-LOC ceiling.

use super::*;
use crate::LOW_DISK_WARNING_THRESHOLD_BYTES;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serialises tests that mutate process-wide environment variables so
/// they don't race under parallel `cargo test`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn remove_env_vars(keys: &[&'static str]) -> Vec<EnvVarGuard> {
    keys.iter().map(|&key| EnvVarGuard::remove(key)).collect()
}

fn command_env_override(
    command: &std::process::Command,
    key: &'static str,
) -> Option<Option<OsString>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value.map(OsString::from))
}

crate::timed_test!(target_registry_memo_is_exported_for_missing_target_dir, {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    paths.ensure_dirs().expect("soldr dirs");
    let target = root.path().join("workspace").join("target");
    let canonical_target = std::fs::canonicalize(root.path())
        .expect("canonical temp root")
        .join("workspace")
        .join("target");
    assert!(!target.exists(), "test requires a clean target directory");

    let mut command = std::process::Command::new("cargo");
    apply_target_registry_memo(&mut command, &target, &paths);

    assert_eq!(
        command_env_override(
            &command,
            crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
        ),
        Some(Some(canonical_target.clone().into_os_string())),
        "the cargo child needs the memo marker before cargo creates target/",
    );
    assert!(
        !target.exists(),
        "memoization must not create target/ as a side effect"
    );

    let registry = crate::cache_lib::target_registry::TargetRegistry::open(
        &crate::cache_lib::data_db_path(&paths),
    )
    .expect("target registry");
    assert!(
        registry
            .get(&canonical_target)
            .expect("registry read")
            .is_some(),
        "the front door should record the future target path once"
    );
});

crate::timed_test!(target_registry_memo_canonicalizes_existing_ancestor, {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    paths.ensure_dirs().expect("soldr dirs");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("nested")).expect("workspace dirs");
    let lexical_target = workspace.join("nested").join("..").join("target");
    let canonical_target = std::fs::canonicalize(&workspace)
        .expect("canonical workspace")
        .join("target");
    assert!(!canonical_target.exists(), "test requires a clean target");

    let mut command = std::process::Command::new("cargo");
    apply_target_registry_memo(&mut command, &lexical_target, &paths);

    assert_eq!(
        command_env_override(
            &command,
            crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
        ),
        Some(Some(canonical_target.clone().into_os_string())),
    );
    let registry = crate::cache_lib::target_registry::TargetRegistry::open(
        &crate::cache_lib::data_db_path(&paths),
    )
    .expect("target registry");
    assert!(
        registry
            .get(&canonical_target)
            .expect("registry read")
            .is_some(),
        "the future target must use the same key it gets after creation"
    );
    assert_eq!(registry.len().expect("registry length"), 1);
});

#[test]
fn cargo_wait_timeout_uses_positive_env_override_only() {
    let _lock = ENV_LOCK.lock().unwrap();

    {
        let _guard = EnvVarGuard::set(CARGO_WAIT_TIMEOUT_ENV_VAR, "7");
        assert_eq!(cargo_wait_timeout(), Duration::from_secs(7));
    }

    for value in ["0", "-1", "not-a-number"] {
        let _guard = EnvVarGuard::set(CARGO_WAIT_TIMEOUT_ENV_VAR, value);
        assert_eq!(
            cargo_wait_timeout(),
            Duration::from_secs(DEFAULT_CARGO_WAIT_TIMEOUT_SECS)
        );
    }

    let _guard = EnvVarGuard::remove(CARGO_WAIT_TIMEOUT_ENV_VAR);
    assert_eq!(
        cargo_wait_timeout(),
        Duration::from_secs(DEFAULT_CARGO_WAIT_TIMEOUT_SECS)
    );
}

#[cfg(windows)]
#[test]
fn diagnostic_capture_does_not_wait_for_leaked_stderr_handle_after_cargo_exits() {
    let mut command = std::process::Command::new("cmd");
    command.args([
        "/C",
        "echo leaked diagnostic before exit 1>&2 & start /B ping -n 6 127.0.0.1 >nul",
    ]);

    let start = std::time::Instant::now();
    let (status, captured) =
        run_command_capturing_diagnostic_tail(&mut command).expect("run diagnostic capture");
    let elapsed = start.elapsed();

    assert!(
        status.success(),
        "fake cargo command should exit successfully"
    );
    assert!(
        captured.contains("leaked diagnostic before exit"),
        "diagnostic capture lost stderr: {captured:?}"
    );
    assert!(
        elapsed >= CAPTURE_PIPE_EOF_GRACE.saturating_sub(Duration::from_millis(250)),
        "test setup should keep stderr open long enough to exercise the bounded drain; elapsed={elapsed:?}",
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "diagnostic capture waited for a leaked inherited stderr handle instead of returning after cargo exited; elapsed={elapsed:?}",
    );
}

crate::timed_test!(timeout_error_mentions_cleanup_and_recovery, {
    let err = SoldrError::Other(format!(
        "cargo diagnostic capture timed out after 1 seconds (set {CARGO_WAIT_TIMEOUT_ENV_VAR} to override); killed child process tree"
    ));
    let cleanup = CargoAbortCleanupReport {
        orphan_rmetas_pruned: 2,
        incremental_dirs_removed: 1,
    };
    let msg = augment_aborted_cargo_error(err, cleanup, true).to_string();

    assert!(
        msg.contains("soldr cleanup after abort: pruned 2 orphan .rmeta file(s), removed 1 incremental/ dir(s)"),
        "timeout message should summarize cleanup: {msg}"
    );
    assert!(
        msg.contains("soldr --no-cache cargo clean -p <crate>"),
        "timeout message should include actionable recovery: {msg}"
    );
    assert!(
        msg.contains("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
        "timeout message should point at fail-fast compile diagnostics: {msg}"
    );
    assert!(
        msg.contains("soldr --no-cache cargo <same args>"),
        "timeout message should point at the cache bypass retry: {msg}"
    );
    assert!(
        msg.contains("ZCCACHE_DISABLE=1"),
        "timeout message should mention the zccache disable escape hatch: {msg}"
    );
    assert!(
        msg.contains("soldr logs paths"),
        "timeout message should point at durable log discovery: {msg}"
    );
});

crate::timed_test!(cargo_abort_log_records_timeout_cleanup_and_recovery, {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let cleanup = CargoAbortCleanupReport {
        orphan_rmetas_pruned: 2,
        incremental_dirs_removed: 1,
    };

    let path = append_cargo_abort_log(CargoAbortLogRequest {
        paths: &paths,
        session_id: 42,
        repo_root: Path::new("repo"),
        started_at_ms: 1_000,
        ended_at_ms: 2_500,
        args: &[
            String::from("build"),
            String::from("-p"),
            String::from("demo"),
        ],
        timeout: true,
        cleanup,
        message: "cargo timed out",
        auto_retry_planned: true,
    })
    .expect("append cargo abort log");

    assert_eq!(path, paths.cargo_abort_log());
    let log = std::fs::read_to_string(&path).expect("read cargo abort log");
    let lines: Vec<_> = log.lines().collect();
    assert_eq!(lines.len(), 1, "expected one jsonl record: {log}");
    let record: serde_json::Value =
        serde_json::from_str(lines[0]).expect("cargo abort log record is JSON");

    assert_eq!(record["schema_version"], serde_json::Value::from(1));
    assert_eq!(record["event"], serde_json::Value::from("cargo_abort"));
    assert_eq!(record["session_id"], serde_json::Value::from(42));
    assert_eq!(record["timeout"], serde_json::Value::from(true));
    assert_eq!(record["auto_retry_planned"], serde_json::Value::from(true));
    assert_eq!(record["elapsed_ms"], serde_json::Value::from(1_500));
    assert_eq!(
        record["cleanup"]["orphan_rmetas_pruned"],
        serde_json::Value::from(2)
    );
    assert_eq!(
        record["cleanup"]["incremental_dirs_removed"],
        serde_json::Value::from(1)
    );
    assert_eq!(
        record["recovery"]["retry_without_cache"]["argv"],
        serde_json::json!(["soldr", "--no-cache", "cargo", "build", "-p", "demo"])
    );
    assert_eq!(
        record["recovery"]["retry_with_zccache_disabled"]["env"]["ZCCACHE_DISABLE"],
        serde_json::Value::from("1")
    );
    assert_eq!(
        record["recovery"]["inspect_logs"],
        serde_json::json!(["soldr", "logs", "paths"])
    );
});

crate::timed_test!(
    cargo_timeout_retry_policy_is_compile_like_and_cache_enabled,
    {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::remove(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR);

        for verb in [
            "build", "b", "check", "c", "test", "t", "clippy", "doc", "d",
        ] {
            assert!(
                cargo_timeout_retry_allowed(true, &[String::from(verb)]),
                "{verb} should be eligible for a no-cache timeout retry"
            );
        }
        for verb in ["run", "r", "bench", "install", "metadata", "clean"] {
            assert!(
                !cargo_timeout_retry_allowed(true, &[String::from(verb)]),
                "{verb} should not be retried automatically"
            );
        }
        assert!(
            !cargo_timeout_retry_allowed(false, &[String::from("build")]),
            "already no-cache cargo runs should not recurse into another retry"
        );
    }
);

crate::timed_test!(cargo_timeout_retry_policy_honors_disable_env, {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::set(CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR, "1");

    assert!(
        !cargo_timeout_retry_allowed(true, &[String::from("build")]),
        "{CARGO_TIMEOUT_RETRY_DISABLE_ENV_VAR}=1 should disable automatic retry"
    );
});

crate::timed_test!(cargo_wait_heartbeat_names_context_and_override, {
    let msg = cargo_wait_heartbeat_message(
        "cargo diagnostic capture",
        Duration::from_secs(120),
        Duration::from_secs(1800),
    );

    assert_eq!(
        msg,
        format!(
            "soldr: cargo diagnostic capture still running after 120s (timeout 1800s; set {CARGO_WAIT_TIMEOUT_ENV_VAR} to override)"
        )
    );
});

crate::timed_test!(aborted_build_cleanup_removes_incremental_dirs, {
    let target = tempfile::tempdir().expect("temp target");
    let host_incremental = target.path().join("debug").join("incremental");
    let target_incremental = target
        .path()
        .join("x86_64-pc-windows-msvc")
        .join("release")
        .join("incremental");
    let deps = target.path().join("debug").join("deps");
    std::fs::create_dir_all(host_incremental.join("crate-a")).expect("host incremental");
    std::fs::create_dir_all(target_incremental.join("crate-b")).expect("target incremental");
    std::fs::create_dir_all(&deps).expect("deps");
    std::fs::write(deps.join("libkeep.rlib"), b"keep").expect("deps file");

    let removed = cleanup_target_incremental_dirs_after_aborted_build(target.path());

    assert_eq!(removed, 2);
    assert!(!host_incremental.exists());
    assert!(!target_incremental.exists());
    assert!(deps.join("libkeep.rlib").exists());
});

crate::timed_test!(aborted_build_cleanup_prunes_rmetas_and_incremental_dirs, {
    let root = tempfile::tempdir().expect("temp root");
    let target = root.path().join("target");
    let deps = target.join("debug").join("deps");
    let incremental = target.join("debug").join("incremental");
    std::fs::create_dir_all(&deps).expect("deps");
    std::fs::create_dir_all(incremental.join("crate-a")).expect("incremental");
    let orphan_rmeta = deps.join("libstale.rmeta");
    std::fs::write(&orphan_rmeta, b"stale").expect("orphan rmeta");

    let plan = crate::rust_plan::RustArtifactPlanContext {
        path: root.path().join("plan.json"),
        zccache_binary: root.path().join("zccache"),
        cache_dir: root.path().join("cache"),
        zccache_daemon_cache_dir: root.path().join("daemon-cache"),
        zccache_daemon_cache_dir_env: false,
        zccache_daemon_name: None,
        session_id: "test-session".to_string(),
        journal_path: root.path().join("journal.jsonl"),
        backend: "local".to_string(),
        cache_profile: None,
        plan_inputs_hash: "hash".to_string(),
        target_dir: target.display().to_string(),
    };
    let cache_plan = CargoCachePlan::for_test_with_rust_artifact_plan(plan);

    let cleanup = cleanup_after_aborted_cargo_run(&cache_plan, &[String::from("build")], true);

    assert_eq!(cleanup.orphan_rmetas_pruned, 1);
    assert_eq!(cleanup.incremental_dirs_removed, 1);
    assert!(!orphan_rmeta.exists());
    assert!(!incremental.exists());
});

#[test]
fn child_cargo_scrubs_soldr_cache_lifecycle_controls() {
    let mut command = std::process::Command::new("cargo");
    command.env(SOLDR_CACHE_LIFECYCLE_ENV_VAR, "command");
    command.env(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR, "1");

    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);

    assert_eq!(
        command_env_override(&command, SOLDR_CACHE_LIFECYCLE_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR),
        Some(None)
    );
}

#[test]
fn fresh_workspace_env_guard_removes_and_restores_soldr_workspace_state() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _zccache = EnvVarGuard::set(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");
    let _target_bundle = EnvVarGuard::set(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR, "/old/bundle");
    let _setup = EnvVarGuard::set("SETUP_SOLDR_WORKSPACE", "/old/workspace");
    let _cache_dir = EnvVarGuard::set("SOLDR_CACHE_DIR", "/intentional/cache");

    {
        let _guard = FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(false);

        assert!(std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR).is_none());
        assert!(std::env::var_os(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR).is_none());
        assert!(std::env::var_os("SETUP_SOLDR_WORKSPACE").is_none());
        assert_eq!(
            std::env::var_os("SOLDR_CACHE_DIR"),
            Some(OsString::from("/intentional/cache"))
        );
    }

    assert_eq!(
        std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(OsString::from("/old/zccache"))
    );
    assert_eq!(
        std::env::var_os(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR),
        Some(OsString::from("/old/bundle"))
    );
    assert_eq!(
        std::env::var_os("SETUP_SOLDR_WORKSPACE"),
        Some(OsString::from("/old/workspace"))
    );
}

#[test]
fn trusted_workspace_env_guard_leaves_inherited_soldr_state_available() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _zccache = EnvVarGuard::set(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");

    let _guard = FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(true);

    assert_eq!(
        std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(OsString::from("/old/zccache"))
    );
}

#[test]
fn child_cargo_scrubs_inherited_soldr_workspace_state() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _setup = EnvVarGuard::set("SETUP_SOLDR_WORKSPACE", "/old/workspace");
    let mut command = std::process::Command::new("cargo");
    command.env(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");
    command.env(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR, "/old/bundle");

    scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);

    assert_eq!(
        command_env_override(&command, crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, "SETUP_SOLDR_WORKSPACE"),
        Some(None)
    );
}

#[test]
fn low_disk_warning_formats_yellow_below_threshold() {
    let message = low_disk_warning_for_free_bytes(1536 * 1024 * 1024, true)
        .expect("expected low-disk warning below threshold");
    assert!(message.contains("\x1b[33mwarning\x1b[0m"));
    assert!(message.contains("1.5 GB free"));
    assert!(message.contains("Run `soldr gc`"));
}

#[test]
fn low_disk_warning_omits_at_threshold() {
    assert!(low_disk_warning_for_free_bytes(LOW_DISK_WARNING_THRESHOLD_BYTES, true).is_none());
}

#[test]
fn low_disk_probe_failure_is_nonfatal() {
    let warning = low_disk_warning_for_path(std::path::Path::new("."), true, |_| {
        Err(std::io::Error::other("probe failed"))
    });
    assert!(warning.is_none());
}

#[test]
fn cargo_args_detect_explicit_target_flag() {
    assert!(cargo_args_specify_target(&[
        "build".into(),
        "--target".into(),
        "x86_64-pc-windows-msvc".into(),
    ]));
    assert!(cargo_args_specify_target(&[
        "build".into(),
        "--target=x86_64-pc-windows-msvc".into(),
    ]));
}

#[test]
fn cargo_args_ignore_target_after_passthrough_separator() {
    assert!(!cargo_args_specify_target(&[
        "test".into(),
        "--".into(),
        "--target".into(),
        "ignored".into(),
    ]));
}

#[test]
fn cargo_args_reject_reserved_no_cache_before_passthrough_separator() {
    assert!(cargo_args_use_reserved_no_cache(&[
        "build".into(),
        "--no-cache".into(),
    ]));
    assert!(!cargo_args_use_reserved_no_cache(&[
        "test".into(),
        "--".into(),
        "--no-cache".into(),
    ]));
}

#[test]
fn first_cargo_subcommand_skips_leading_flags() {
    assert_eq!(
        first_cargo_subcommand(&["--verbose".into(), "nextest".into(), "run".into()]),
        Some("nextest")
    );
    assert_eq!(
        first_cargo_subcommand(&["nextest".into(), "run".into()]),
        Some("nextest")
    );
    assert_eq!(first_cargo_subcommand(&["--help".into()]), None);
    assert_eq!(first_cargo_subcommand(&[]), None);
}

#[test]
fn first_cargo_subcommand_stops_at_passthrough_separator() {
    assert_eq!(
        first_cargo_subcommand(&["--".into(), "nextest".into()]),
        None
    );
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn cargo_args_are_cacheable_for_direct_build() {
    assert!(cargo_args_are_cacheable(&argv(&["build"])));
    assert!(cargo_args_are_cacheable(&argv(&["build", "--release"])));
    assert!(cargo_args_are_cacheable(&argv(&["b"])));
}

#[test]
fn cargo_args_are_cacheable_for_chef_cook() {
    // soldr cook (issue #359) routes `cargo chef cook` through this front
    // door. The outer process orchestrates an inner `cargo build` against
    // a stub project, so we must seed RUSTC_WRAPPER for the inner build
    // to pick zccache up.
    assert!(cargo_args_are_cacheable(&argv(&["chef", "cook"])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "chef",
        "cook",
        "--release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&["chef", "prepare"])));
}

#[test]
fn cargo_args_are_not_cacheable_for_direct_clean() {
    assert!(!cargo_args_are_cacheable(&argv(&["clean"])));
    assert!(!cargo_args_are_cacheable(&argv(&["fmt"])));
}

#[test]
fn cargo_args_are_not_cacheable_for_direct_miri_driver_hooks() {
    // Miri owns its own interpreter/runtime driver path. Soldr still
    // exports RUSTC_WRAPPER to cache-enabled `cargo miri`, but does not
    // run target/rust-plan build hooks around the Miri driver itself.
    assert!(!cargo_args_are_cacheable(&argv(&["miri"])));
}

#[test]
fn cargo_args_are_cacheable_for_every_registry_inner_build_subcommand() {
    // Issue #824 raised against `cargo zigbuild` specifically, but the
    // sub-agent audit of the known_tools registry surfaced six others
    // that have the same shape: outer cargo invocation spawns (or is)
    // an inner cargo build / test / doc whose rustc invocations need
    // RUSTC_WRAPPER to be present in the env. This test pins the
    // expected classification so a future "add a tool, forget to set
    // wraps_inner_cargo_build" regression breaks the build.
    //
    // Source of truth: each ToolSpec's `wraps_inner_cargo_build` field
    // and the per-entry comment justifying the value. This test asserts
    // that classification flows all the way through the front-door
    // cacheable predicate.
    for sub in [
        "nextest",       // runs `cargo test`
        "llvm-cov",      // runs `cargo test`/`build`; chains RUSTC_WRAPPER itself
        "udeps",         // embeds cargo crate; inherits parent env
        "semver-checks", // runs `cargo doc` for baseline + current
        "expand",        // calls `cargo rustc` directly
        "chef",          // runs `cargo build` for the stub project
        "zigbuild",      // the #824 repro — wraps `cargo build` with zig linker
        "xwin",          // wraps `cargo build` with the msvc-on-linux toolchain
        "binstall",      // Compile-strategy fallback shells `cargo install`
    ] {
        assert!(
            cargo_args_are_cacheable(&argv(&[sub])),
            "subcommand {sub:?} must classify as cacheable so RUSTC_WRAPPER \
             propagates to its inner cargo (registry says \
             wraps_inner_cargo_build=true)",
        );
    }
}

#[test]
fn cargo_args_are_not_cacheable_for_static_analysis_tools() {
    // The three static-analysis tools in the registry don't spawn
    // rustc at all; engaging zccache would pay the session-start/stop
    // tax for zero hit value.
    for sub in ["deny", "audit", "machete"] {
        assert!(
            !cargo_args_are_cacheable(&argv(&[sub])),
            "subcommand {sub:?} must classify as non-cacheable (registry \
             says wraps_inner_cargo_build=false)",
        );
    }
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_short_exec_single_token() {
    assert!(cargo_args_are_cacheable(&argv(&["watch", "-x", "build"])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_short_exec_multi_token() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-x",
        "build --release",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_long_exec_equals_form() {
    assert!(cargo_args_are_cacheable(&argv(&["watch", "--exec=build"])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--exec=build --release",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_long_exec_space_form() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "--exec", "build",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_shell_form_strips_leading_cargo() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-s",
        "cargo build --release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--shell",
        "cargo build --release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--shell=cargo build --release",
    ])));
}

#[test]
fn cargo_args_are_not_cacheable_for_watch_with_uncacheable_inner() {
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "-x", "clean"])));
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "-x", "fmt"])));
}

#[test]
fn cargo_args_apply_rustfmt_shim_for_direct_and_watch_fmt() {
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&["fmt"])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch", "-x", "fmt",
    ])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "--exec=fmt --check",
    ])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "-s",
        "cargo fmt --check",
    ])));
    assert!(cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "--shell",
        "cargo +nightly fmt --check",
    ])));
}

#[test]
fn cargo_args_do_not_apply_rustfmt_shim_for_non_fmt_watch() {
    assert!(!cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch", "-x", "build",
    ])));
    assert!(!cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch",
        "-s",
        "cargo clean",
    ])));
    assert!(!cargo_args_should_apply_rustfmt_shim(&argv(&[
        "watch", "--", "-x", "fmt",
    ])));
}

#[test]
fn rustfmt_shim_env_is_applied_to_watch_fmt_when_cache_enabled() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guards = remove_env_vars(&["RUSTFMT"]);

    for args in [
        argv(&["watch", "-x", "fmt"]),
        argv(&["watch", "-s", "cargo fmt --check"]),
    ] {
        let mut command = std::process::Command::new("cargo");
        let guard = maybe_apply_rustfmt_zccache_shim(&mut command, &args, true)
            .expect("watch fmt should get a rustfmt shim");

        let rustfmt = command_env_override(&command, "RUSTFMT")
            .expect("RUSTFMT env override")
            .expect("RUSTFMT env value");
        let rustfmt_path = PathBuf::from(rustfmt);
        assert!(
            rustfmt_path.is_file(),
            "RUSTFMT shim should exist while guard is alive: {}",
            rustfmt_path.display()
        );
        let expected_name = format!("rustfmt{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(
            rustfmt_path.file_name().and_then(|name| name.to_str()),
            Some(expected_name.as_str())
        );
        assert_eq!(
            command_env_override(&command, crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR)
                .flatten()
                .as_deref(),
            Some(OsStr::new("1")),
            "cargo-watch must inherit the child-shim recursion sentinel"
        );
        drop(guard);
    }
}

#[test]
fn rustfmt_shim_env_is_not_applied_when_cache_is_disabled() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guards = remove_env_vars(&["RUSTFMT"]);
    let args = argv(&["watch", "-x", "fmt"]);
    let mut command = std::process::Command::new("cargo");

    assert!(maybe_apply_rustfmt_zccache_shim(&mut command, &args, false).is_none());
    assert!(
        command_env_override(&command, "RUSTFMT").is_none(),
        "cache-disabled cargo watch fmt should leave rustfmt direct"
    );
}

#[test]
fn cargo_args_are_cacheable_for_watch_when_any_inner_is_cacheable() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "-x", "build", "-x", "clean",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "-x", "clean", "-x", "build",
    ])));
}

#[test]
fn cargo_args_are_not_cacheable_for_bare_watch() {
    assert!(!cargo_args_are_cacheable(&argv(&["watch"])));
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "--clear"])));
}

#[test]
fn cargo_args_ignore_exec_after_passthrough_separator() {
    // Anything after `--` is not parsed as a watch-flag value.
    assert!(!cargo_args_are_cacheable(&argv(&[
        "watch", "--", "-x", "build",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_inner_release_flag() {
    // `-x 'build --release'` — tokens after `build` should not break the
    // detection, and the outer cacheable answer is still true.
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-x",
        "build --release --workspace",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_toolchain_pin() {
    // `+nightly` is a cargo toolchain shorthand that should be skipped when
    // locating the `watch` subcommand.
    assert!(cargo_args_are_cacheable(&argv(&[
        "+nightly", "watch", "-x", "build",
    ])));
}

// -------------------------------------------------------------------------
// Auto target-GC flag stripping (#485). The soldr-private flags get pulled
// out of the arg vector before cargo ever sees them. The env var path is
// covered separately because it touches process state.
// -------------------------------------------------------------------------

#[test]
fn strip_no_gc_target_flag_removes_combined_form() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&["build", "--no-gc-target", "--release"]));
    assert_eq!(cleaned, argv(&["build", "--release"]));
    assert!(opt.before);
    assert!(opt.after);
}

#[test]
fn strip_no_gc_target_flag_removes_before_only() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&["check", "--no-gc-target-before"]));
    assert_eq!(cleaned, argv(&["check"]));
    assert!(opt.before);
    assert!(!opt.after);
}

#[test]
fn strip_no_gc_target_flag_removes_after_only() {
    let (cleaned, opt) =
        strip_no_gc_target_flags(&argv(&["build", "--no-gc-target-after", "--workspace"]));
    assert_eq!(cleaned, argv(&["build", "--workspace"]));
    assert!(!opt.before);
    assert!(opt.after);
}

#[test]
fn strip_no_gc_target_flag_default_no_op() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&["build", "--release"]));
    assert_eq!(cleaned, argv(&["build", "--release"]));
    assert!(!opt.before);
    assert!(!opt.after);
}

#[test]
fn strip_no_gc_target_flag_passes_through_after_separator() {
    // Flags after `--` belong to the program cargo runs and must not be
    // touched. This mirrors how `--no-trampoline` is handled.
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&[
        "run",
        "--bin",
        "foo",
        "--",
        "--no-gc-target",
        "--no-gc-target-after",
    ]));
    assert_eq!(
        cleaned,
        argv(&[
            "run",
            "--bin",
            "foo",
            "--",
            "--no-gc-target",
            "--no-gc-target-after",
        ])
    );
    assert!(!opt.before);
    assert!(!opt.after);
}

#[test]
fn strip_no_gc_target_flag_handles_repeated_flags() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&[
        "build",
        "--no-gc-target-before",
        "--no-gc-target-after",
    ]));
    assert_eq!(cleaned, argv(&["build"]));
    assert!(opt.before);
    assert!(opt.after);
}

#[test]
fn env_disables_target_gc_truthy_values() {
    let _lock = ENV_LOCK.lock().unwrap();
    for value in ["1", "true", "yes", "anything"] {
        let _guard = EnvVarGuard::set(NO_GC_TARGET_ENV_VAR, value);
        let merged = GcTargetOptOut::default().merged_with_env();
        assert!(
            merged.before && merged.after,
            "env value {value:?} should force both opt-outs"
        );
    }
}

#[test]
fn env_disables_target_gc_falsey_values_dont_opt_out() {
    let _lock = ENV_LOCK.lock().unwrap();
    for value in ["", "0", "false", "False"] {
        let _guard = EnvVarGuard::set(NO_GC_TARGET_ENV_VAR, value);
        let merged = GcTargetOptOut::default().merged_with_env();
        assert!(
            !merged.before && !merged.after,
            "env value {value:?} must not opt out"
        );
    }
}

#[test]
fn env_disables_target_gc_preserves_explicit_flag_opt_outs() {
    // If --no-gc-target-before is on the cli, the env var being unset
    // must not silently re-enable the after pass.
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::remove(NO_GC_TARGET_ENV_VAR);
    let merged = GcTargetOptOut {
        before: true,
        after: false,
    }
    .merged_with_env();
    assert!(merged.before);
    assert!(!merged.after);
}

// Issue #755: cargo built-in verbs must not trigger the fuzzy "did you
// mean: cargo X?" hint. The External arm hands them straight to cargo;
// treating them as typos is misleading.
#[test]
fn suggest_cargo_subcommand_typo_skips_cargo_builtin_verbs() {
    for verb in crate::cli_args::CARGO_BUILTIN_VERBS {
        assert_eq!(
            suggest_cargo_subcommand_typo(verb),
            None,
            "cargo built-in verb {verb:?} must not be suggested as a typo of a known subcommand",
        );
    }
}

#[test]
fn suggest_cargo_subcommand_typo_still_catches_genuine_typos() {
    // Regression guard for issue #412: a clear typo of a registered
    // cargo subcommand (e.g. `ntest` → `nextest`) still gets the hint.
    assert_eq!(
        suggest_cargo_subcommand_typo("ntest").as_deref(),
        Some("nextest"),
        "fuzzy hint must still fire for genuine typos of known cargo subcommands",
    );
}

#[test]
fn suggest_cargo_subcommand_typo_returns_none_for_unrelated_input() {
    // Sanity check: random garbage that isn't close to any candidate
    // gets no suggestion at all.
    assert_eq!(
        suggest_cargo_subcommand_typo("completely-made-up-name"),
        None,
    );
}

// Issue #816: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS env-var handling.
// The bool guard parses truthy / falsy values consistently with the
// pattern other soldr env vars use.

#[test]
fn force_managed_cargo_subcommands_defaults_to_false_when_unset() {
    // Serialize on the env mutex used elsewhere in this file so we don't
    // race against other env-touching tests in the same binary.
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    // SAFETY: the test acquires ENV_LOCK to serialize against any other
    // test that mutates process env.
    unsafe {
        std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    }
    assert!(!force_managed_cargo_subcommands());
    if let Some(value) = prev {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        }
    }
}

#[test]
fn force_managed_cargo_subcommands_parses_falsey_strings_as_false() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    for falsey in ["", " ", "0", "false", "no", "off", "  off  "] {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, falsey);
        }
        assert!(
            !force_managed_cargo_subcommands(),
            "value {falsey:?} should parse as false",
        );
    }
    match prev {
        Some(value) => unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        },
        None => unsafe {
            std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
        },
    }
}

#[test]
fn force_managed_cargo_subcommands_parses_truthy_strings_as_true() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    for truthy in ["1", "true", "yes", "on", "anything-else"] {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, truthy);
        }
        assert!(
            force_managed_cargo_subcommands(),
            "value {truthy:?} should parse as true",
        );
    }
    match prev {
        Some(value) => unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        },
        None => unsafe {
            std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
        },
    }
}

#[test]
fn find_on_path_locates_executable_in_a_path_dir() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let exe_name = if cfg!(windows) {
        "soldr-test-find-on-path-fixture.exe"
    } else {
        "soldr-test-find-on-path-fixture"
    };
    let exe_path = dir.path().join(exe_name);
    std::fs::write(&exe_path, b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&exe_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe_path, perms).unwrap();
    }

    let prev_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::from(dir.path());
    if !prev_path.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        new_path.push(sep);
        new_path.push(&prev_path);
    }
    unsafe {
        std::env::set_var("PATH", &new_path);
    }

    // The probe name is intentionally unsuffixed on both platforms — on
    // Windows the PATHEXT sweep in find_on_path picks up the `.exe`.
    let resolved = find_on_path("soldr-test-find-on-path-fixture");
    unsafe {
        std::env::set_var("PATH", &prev_path);
    }

    let resolved_path = resolved.expect("fixture must be found on PATH");
    assert!(
        resolved_path.is_file(),
        "resolved path {resolved_path:?} must exist",
    );
    assert!(
        resolved_path
            .parent()
            .map(|p| p == dir.path())
            .unwrap_or(false),
        "resolved path {resolved_path:?} must live under the fixture dir {:?}",
        dir.path(),
    );
}

#[test]
fn find_on_path_returns_none_when_missing() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let prev_path = std::env::var_os("PATH").unwrap_or_default();
    let new_path: std::ffi::OsString = dir.path().into();
    unsafe {
        std::env::set_var("PATH", &new_path);
    }
    let resolved = find_on_path("definitely-not-on-path-soldr-test-816");
    unsafe {
        std::env::set_var("PATH", &prev_path);
    }
    assert_eq!(resolved, None);
}

// ---------------------------------------------------------------------------
// compute_subcommand_env_overrides — fixes ring's build.rs picking the
// GNU clang driver instead of clang-cl when cross-compiling to
// *-pc-windows-msvc via cargo-xwin.
// ---------------------------------------------------------------------------

fn argvec(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

crate::timed_test!(nextest_archive_blessed_target_detects_archive_only, {
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "nextest archive --target aarch64-apple-darwin --workspace"
        )),
        Some("aarch64-apple-darwin"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "--manifest-path Cargo.toml nextest archive --target=x86_64-apple-darwin"
        )),
        Some("x86_64-apple-darwin"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "nextest --color always archive --target=x86_64-apple-darwin"
        )),
        Some("x86_64-apple-darwin"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest run --archive-file dist/tests.tar.zst")),
        None,
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest run archive --target x86_64-apple-darwin")),
        None,
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec(
            "nextest archive --target x86_64-unknown-linux-musl"
        )),
        None,
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest archive --target x86_64-pc-windows-msvc")),
        Some("x86_64-pc-windows-msvc"),
    );
    assert_eq!(
        nextest_archive_blessed_target(&argvec("nextest archive --target aarch64-pc-windows-msvc")),
        Some("aarch64-pc-windows-msvc"),
    );
});

crate::timed_test!(cargo_global_args_insert_before_nextest_subcommand, {
    let args = argvec("--manifest-path Cargo.toml nextest archive --target x86_64-apple-darwin");
    let cargo_args = vec![
        "--config".to_string(),
        "target.x86_64-apple-darwin.mimalloc.rustc-link-lib=[\"static=mimalloc\"]".to_string(),
    ];

    let got = insert_cargo_global_args(&args, &cargo_args);

    assert_eq!(
        got,
        argvec("--manifest-path Cargo.toml --config target.x86_64-apple-darwin.mimalloc.rustc-link-lib=[\"static=mimalloc\"] nextest archive --target x86_64-apple-darwin")
    );
});

crate::timed_test!(nextest_archive_darwin_bootstrap_reuses_blessed_env, {
    let _lock = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let sdk = tmp.path().join("MacOSX.fake.sdk");
    let llvm_bin = tmp.path().join("llvm-bin");
    std::fs::create_dir_all(&sdk).unwrap();
    std::fs::create_dir_all(&llvm_bin).unwrap();

    let _sdkroot = EnvVarGuard::set("SDKROOT", &sdk);
    let _llvm = EnvVarGuard::set("SOLDR_LLVM_DIR", &llvm_bin);
    let _legacy_zig = EnvVarGuard::remove(crate::blessed_build::USE_LEGACY_ZIGBUILD_ENV_VAR);
    let _legacy_sys = EnvVarGuard::set(crate::blessed_build::USE_LEGACY_VENDORED_SYS_ENV_VAR, "1");
    let _system_cmake = EnvVarGuard::set(crate::blessed_build::USE_SYSTEM_CMAKE_ENV_VAR, "1");

    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let mut bin_dirs = Vec::new();
    let mut env = Vec::new();
    let mut cargo_args = Vec::new();

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(append_subcommand_transitive_bin_dirs(
            "nextest",
            &argvec("nextest archive --target x86_64-apple-darwin --workspace"),
            &paths,
            &mut bin_dirs,
            &mut env,
            &mut cargo_args,
        ))
        .unwrap();

    assert!(
        bin_dirs.iter().any(|dir| dir == &llvm_bin),
        "managed LLVM bin dir must be prepended for darwin nextest archive: {bin_dirs:?}"
    );
    assert!(
        cargo_args.is_empty(),
        "syslib overrides are opted out in this hermetic test"
    );

    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(
        map.get("SDKROOT").map(String::as_str),
        Some(sdk.to_str().unwrap()),
    );
    assert!(
        map.get("CC_x86_64_apple_darwin")
            .is_some_and(|value| value.contains("--target=x86_64-apple-darwin")
                && value.contains("-isysroot")),
        "CC_x86_64_apple_darwin must target the Apple SDK: {map:?}"
    );
    assert_eq!(
        map.get("CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER")
            .map(String::as_str),
        Some("clang"),
    );
    assert!(
        map.get("CARGO_TARGET_X86_64_APPLE_DARWIN_RUSTFLAGS")
            .is_some_and(|value| value.contains("-fuse-ld=lld")
                && value.contains("-mmacosx-version-min=11.0")),
        "darwin rustflags must route through clang/lld with the SDK: {map:?}"
    );
});

#[test]
fn known_cargo_build_target_uses_explicit_target_arg() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::remove("CARGO_BUILD_TARGET");

    assert_eq!(
        target::known_cargo_build_target(
            &argvec("build --release --target x86_64-apple-darwin"),
            None,
        ),
        Some("x86_64-apple-darwin".to_string()),
    );
    assert_eq!(
        target::known_cargo_build_target(
            &argvec("build --release --target=aarch64-apple-darwin"),
            None,
        ),
        Some("aarch64-apple-darwin".to_string()),
    );
}

#[test]
fn known_cargo_build_target_prefers_defaulted_target_then_env() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::set("CARGO_BUILD_TARGET", "x86_64-unknown-linux-musl");

    assert_eq!(
        target::known_cargo_build_target(&argvec("build"), Some("x86_64-pc-windows-msvc")),
        Some("x86_64-pc-windows-msvc".to_string()),
    );
    assert_eq!(
        target::known_cargo_build_target(&argvec("build"), None),
        Some("x86_64-unknown-linux-musl".to_string()),
    );
}

#[test]
fn extract_target_arg_handles_space_separated_form() {
    assert_eq!(
        extract_target_arg(&argvec(
            "xwin build --release --target aarch64-pc-windows-msvc"
        )),
        Some("aarch64-pc-windows-msvc"),
    );
}

#[test]
fn extract_target_arg_handles_equals_form() {
    assert_eq!(
        extract_target_arg(&argvec(
            "xwin build --release --target=x86_64-pc-windows-msvc"
        )),
        Some("x86_64-pc-windows-msvc"),
    );
}

#[test]
fn extract_target_arg_returns_none_when_absent() {
    assert_eq!(extract_target_arg(&argvec("xwin build --release")), None);
}

#[test]
fn xwin_arm64_msvc_target_injects_cc_clang_cl_env() {
    let env = compute_subcommand_env_overrides(&argvec(
        "xwin build --release --target aarch64-pc-windows-msvc",
    ));
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(
        map.get("CC_aarch64_pc_windows_msvc").map(String::as_str),
        Some("clang-cl"),
    );
    assert_eq!(
        map.get("CXX_aarch64_pc_windows_msvc").map(String::as_str),
        Some("clang-cl"),
    );
    assert_eq!(
        map.get("AR_aarch64_pc_windows_msvc").map(String::as_str),
        Some("llvm-lib"),
    );
}

#[test]
fn xwin_x64_msvc_target_injects_underscored_triple_keys() {
    let env = compute_subcommand_env_overrides(&argvec(
        "xwin build --target x86_64-pc-windows-msvc --release",
    ));
    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    assert_eq!(
        map.get("CC_x86_64_pc_windows_msvc").map(String::as_str),
        Some("clang-cl"),
    );
}

#[test]
fn zigbuild_does_not_inject_cc_overrides_even_for_msvc_target() {
    // cargo-zigbuild also builds for windows-msvc but uses zig as the
    // linker — cc-rs's clang / clang-cl distinction is xwin-specific.
    let env = compute_subcommand_env_overrides(&argvec(
        "zigbuild --target x86_64-pc-windows-msvc --release",
    ));
    assert!(
        env.is_empty(),
        "zigbuild lane shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn zigbuild_env_overrides_include_cc_and_linker_for_supported_target() {
    let _lock = ENV_LOCK.lock().unwrap();
    let expected_keys = [
        "CC_aarch64_unknown_linux_musl",
        "CXX_aarch64_unknown_linux_musl",
        "AR_aarch64_unknown_linux_musl",
        "RANLIB_aarch64_unknown_linux_musl",
        "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER",
    ];
    let _guards = remove_env_vars(&expected_keys);

    let dir = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(dir.path().join("soldr"));
    let mut env = Vec::new();

    append_zigbuild_env_overrides(&paths, "aarch64-unknown-linux-musl", &mut env).unwrap();

    let map: std::collections::HashMap<_, _> = env.into_iter().collect();
    for key in expected_keys {
        let value = map.get(key).unwrap_or_else(|| panic!("missing {key}"));
        assert!(
            value.contains("zigbuild-shims"),
            "{key} should point at generated zigbuild shim, got {value}"
        );
    }
}

#[test]
fn xwin_with_non_msvc_target_does_not_inject_anything() {
    let env =
        compute_subcommand_env_overrides(&argvec("xwin build --target x86_64-unknown-linux-gnu"));
    assert!(
        env.is_empty(),
        "non-msvc target shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn xwin_without_target_does_not_inject_anything() {
    // The injection is keyed on an explicit triple (so we can name the
    // env vars); skip when no triple is in the args.
    let env = compute_subcommand_env_overrides(&argvec("xwin build --release"));
    assert!(
        env.is_empty(),
        "no --target shouldn't inject xwin env: {env:?}"
    );
}

#[test]
fn xwin_download_subverb_does_not_inject() {
    // `cargo xwin download` only fetches the MSVC SDK; cc-rs never
    // runs, so no env injection is needed.
    let env =
        compute_subcommand_env_overrides(&argvec("xwin download --target aarch64-pc-windows-msvc"));
    assert!(
        env.is_empty(),
        "xwin download shouldn't inject env: {env:?}"
    );
}

#[test]
fn non_xwin_subcommand_does_not_inject_anything() {
    let env = compute_subcommand_env_overrides(&argvec(
        "build --target x86_64-pc-windows-msvc --release",
    ));
    assert!(
        env.is_empty(),
        "bare cargo build shouldn't inject xwin env: {env:?}"
    );
}

crate::timed_test!(zlib_ng_arm_wrapper_written_only_for_aarch64_msvc, {
    let dir = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(dir.path().join("soldr"));

    // Non-arm / non-msvc triples: no wrapper, no env.
    for triple in [
        "x86_64-pc-windows-msvc",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
    ] {
        let got = ensure_zlib_ng_arm_cmake_wrapper(&paths, triple).unwrap();
        assert!(got.is_none(), "{triple} must not get the wrapper");
    }

    // The arm-msvc lane gets the DASH-triple env var (the form the
    // cmake crate checks before cargo-xwin's underscore form) plus a
    // wrapper that chain-includes cargo-xwin's file and disables the
    // clang-cl-incompatible zlib-ng ARM toggles.
    let (key, value) = ensure_zlib_ng_arm_cmake_wrapper(&paths, "aarch64-pc-windows-msvc")
        .unwrap()
        .expect("aarch64-pc-windows-msvc gets the wrapper");
    assert_eq!(key, "CMAKE_TOOLCHAIN_FILE_aarch64-pc-windows-msvc");
    let body = std::fs::read_to_string(&value).expect("wrapper file exists");
    assert!(
        body.contains("$ENV{CMAKE_TOOLCHAIN_FILE_aarch64_pc_windows_msvc}"),
        "wrapper must chain-include cargo-xwin's underscore-form toolchain file: {body}"
    );
    for toggle in ["WITH_NEON OFF", "WITH_ARMV8 OFF", "WITH_ARMV6 OFF"] {
        assert!(body.contains(toggle), "wrapper must force {toggle}: {body}");
    }

    // Idempotent: second call rewrites the same path.
    let (key2, value2) = ensure_zlib_ng_arm_cmake_wrapper(&paths, "aarch64-pc-windows-msvc")
        .unwrap()
        .expect("second call still yields the wrapper");
    assert_eq!((key, value), (key2, value2));
});

crate::timed_test!(journal_miss_reasons_parse_jsonl_before_log_fallback, {
    let body = [
        r#"{"outcome":"hit","miss_reason":"ignored"}"#,
        r#"{"outcome":"miss","miss_reason":"context_not_found"}"#,
        r#"{"outcome":"miss","miss_reason":"context_not_found"}"#,
        r#"{"outcome":"link_miss","miss_reason":"no_artifact_for_key"}"#,
        r#"{"outcome":"miss"}"#,
        "not-json",
    ]
    .join("\n");

    let reasons = parse_build_miss_reasons_from_journal(&body);

    assert_eq!(reasons.len(), 3);
    assert_eq!(reasons[0].reason, "context_not_found");
    assert_eq!(reasons[0].count, 2);
    assert_eq!(reasons[1].reason, "no_artifact_for_key");
    assert_eq!(reasons[1].count, 1);
    assert_eq!(reasons[2].reason, "unknown");
    assert_eq!(reasons[2].count, 1);
});

crate::timed_test!(miss_reasons_do_not_fall_back_to_full_global_journal, {
    let root = tempfile::tempdir().expect("temp root");
    let global_journal = root.path().join("compile_journal.jsonl");
    std::fs::write(
        &global_journal,
        r#"{"outcome":"miss","miss_reason":"old_build"}"#,
    )
    .expect("write old global journal");
    let session_journal = root.path().join("last-session.jsonl");
    let session_log = root.path().join("last-session.log");

    let reasons = read_build_miss_reasons(None, &session_journal, &session_log);

    assert!(
        reasons.is_empty(),
        "missing archived tail must not parse unrelated global journal entries"
    );
});

crate::timed_test!(
    miss_reasons_fall_back_to_session_journal_when_tail_missing,
    {
        let root = tempfile::tempdir().expect("temp root");
        let session_journal = root.path().join("last-session.jsonl");
        let session_log = root.path().join("last-session.log");
        std::fs::write(
            &session_journal,
            r#"{"outcome":"miss","miss_reason":"session_build"}"#,
        )
        .expect("write session journal");

        let reasons = read_build_miss_reasons(None, &session_journal, &session_log);

        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].reason, "session_build");
        assert_eq!(reasons[0].count, 1);
    }
);

crate::timed_test!(compile_journal_tail_archive_keeps_current_build_only, {
    let root = tempfile::tempdir().expect("temp root");
    let source = root.path().join("compile_journal.jsonl");
    std::fs::write(&source, "old-build\n").expect("write old journal");
    let start_offset = std::fs::metadata(&source).expect("metadata").len();
    std::fs::write(&source, "old-build\nnew-build-1\nnew-build-2\n").expect("append journal");

    let archived = copy_session_artifact_tail(
        &source,
        &root.path().join("history").join("1"),
        "compile_journal.jsonl",
        start_offset,
    )
    .expect("archive path");

    let body = std::fs::read_to_string(archived).expect("archived body");
    assert_eq!(body, "new-build-1\nnew-build-2\n");
});

crate::timed_test!(
    compile_journal_tail_waits_for_expected_entries,
    Duration::from_secs(5),
    {
        let root = tempfile::tempdir().expect("temp root");
        let source = root.path().join("compile_journal.jsonl");
        std::fs::write(&source, "old-build\n").expect("write old journal");
        let start_offset = std::fs::metadata(&source).expect("metadata").len();
        let writer_source = source.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::write(&writer_source, "old-build\nnew-build-1\n").expect("write first tail");
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(&writer_source, "old-build\nnew-build-1\nnew-build-2\n")
                .expect("write second tail");
        });

        assert!(wait_for_compile_journal_tail(
            &source,
            start_offset,
            Some(2)
        ));
        assert_eq!(
            count_compile_journal_tail_entries(&source, start_offset),
            Some(2)
        );
    }
);

crate::timed_test!(build_session_fallback_persists_start_end_without_daemon, {
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");

    persist_build_session_start_fallback_inner(&paths, 99, &repo, 1_000).expect("start fallback");
    persist_build_session_end_fallback_inner(&paths, 99, 0, 1_250).expect("end fallback");

    let db_path = crate::cache_lib::data_db_path(&paths);
    let record = crate::daemon::db::get_build(&db_path, 99)
        .expect("read build")
        .expect("record");
    assert_eq!(record.repo_root, repo.display().to_string());
    assert_eq!(record.started_at_ms, 1_000);
    assert_eq!(record.ended_at_ms, Some(1_250));
    assert_eq!(record.total_wall_ms, Some(250));
    assert_eq!(record.exit_code, Some(0));

    let events = crate::daemon::db::list_events_for_session(&db_path, 99).expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, crate::daemon::db::EventKind::SessionStart);
    assert_eq!(events[1].kind, crate::daemon::db::EventKind::SessionEnd);
});
