//! Unit tests for the private embedded zccache cache-root layout
//! ([`super::private_zccache_cache_root`]) and the legacy-root migration
//! family. Lives in a sibling file referenced via `#[path]` so
//! `zccache_embedded.rs` stays under the 1000-LOC ceiling.

use super::zccache_embedded_process_tests::{bounded_output, CompilerProbeOutput};
use super::*;

fn shutdown_report(
    pending_writes_drained: bool,
    index_writer_drained: bool,
    outcome: FlushStepOutcome,
) -> zccache::embedded::DetailedShutdownReport {
    zccache::embedded::DetailedShutdownReport {
        mode: ShutdownMode::Graceful,
        flushed: zccache::embedded::DetailedFlushReport {
            pending_writes_drained,
            index_writer_drained,
            steps: vec![zccache::embedded::FlushStepReport {
                step: "persist indexes".to_owned(),
                outcome,
            }],
            artifact_entries: 1,
            metadata_entries: 1,
        },
    }
}

#[test]
fn shutdown_requires_a_complete_cache_checkpoint() {
    let complete = shutdown_report(true, true, FlushStepOutcome::Completed);
    ensure_complete_shutdown(&complete).expect("complete checkpoint");

    let incomplete = shutdown_report(true, false, FlushStepOutcome::TimedOut);
    let error = ensure_complete_shutdown(&incomplete).expect_err("incomplete checkpoint");
    let message = error.to_string();
    assert!(message.contains("cache checkpoint incomplete"));
    assert!(message.contains("index_writer_drained=false"));
    assert!(message.contains("TimedOut"));
}

fn validate_compiler_probe(
    path: &std::path::Path,
    output: Result<CompilerProbeOutput, std::io::Error>,
) -> Result<String, String> {
    let output = output.map_err(|error| {
        format!(
            "Rust compiler prerequisite failed: path={} spawn_error={error}",
            path.display()
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.success {
        return Err(format!(
            "Rust compiler prerequisite failed: path={} exit_code={:?}\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            output.exit_code,
            stdout,
            stderr
        ));
    }
    let version = stdout.trim();
    let mut lines = version.lines();
    let has_rustc_version = lines.next().is_some_and(|line| line.starts_with("rustc "));
    let has_host = lines.any(|line| line.starts_with("host: "));
    if !has_rustc_version || !has_host {
        return Err(format!(
            "Rust compiler prerequisite failed: path={} exit_code={:?} unexpected rustc -vV output\nstdout:\n{}\nstderr:\n{}",
            path.display(),
            output.exit_code,
            stdout,
            stderr
        ));
    }
    Ok(version.to_string())
}

fn probe_working_compiler(path: &std::path::Path) -> Result<String, String> {
    let mut command = std::process::Command::new(path);
    command.arg("-vV");
    let output = bounded_output(command).map(CompilerProbeOutput::from);
    validate_compiler_probe(path, output)
}

fn test_daemon_identity() -> DaemonProcess {
    use running_process::broker::protocol::Endpoint;
    DaemonProcess::current_process(
        Endpoint {
            namespace_id: "soldr-zccache-test".to_string(),
            path: "soldr-zccache-test.sock".to_string(),
        },
        None,
    )
    .expect("current test process identity")
}

#[test]
fn identity_is_portable_across_cache_roots() {
    let identity = derive_identity();
    let cold = SoldrPaths::with_root(std::path::PathBuf::from("/tmp/cache-cold"));
    let warm = SoldrPaths::with_root(std::path::PathBuf::from("/tmp/cache-warm"));

    let cold_root = private_zccache_cache_root(&cold, &identity);
    let warm_root = private_zccache_cache_root(&warm, &identity);
    assert_eq!(
        cold_root
            .strip_prefix(&cold.cache)
            .expect("cold cache prefix"),
        warm_root
            .strip_prefix(&warm.cache)
            .expect("warm cache prefix"),
        "save/load roots must select the same archived private subtree",
    );
}

#[test]
fn identity_survives_soldr_upgrades() {
    assert_eq!(derive_identity().instance_id, "embedded-v1");
}

#[test]
fn embedded_root_rejects_a_cross_product_link() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("selected-product"));
    let daemon = test_daemon_identity();
    let stable = private_zccache_cache_root(&paths, &derive_identity());
    std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
    let external = temp.path().join("other-product");
    std::fs::create_dir_all(&external).unwrap();
    let sentinel = external.join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    // A cross-product link at the stable root: junction on Windows
    // (privilege-free), symlink on Unix.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/c", "mklink", "/J"])
            .arg(&stable)
            .arg(&external);
        let output = running_process::run_std_command_bounded(
            command,
            Some(std::time::Duration::from_secs(30)),
            64 * 1024,
        )
        .unwrap();
        assert_eq!(output.exit_code, 0, "mklink /J must create the junction");
    } else {
        crate::platform::fs::links::create(&external.display().to_string(), &stable, true)
            .expect("create directory symlink");
    }
    assert!(prepare_embedded_cache_root(&paths, &daemon, &stable).is_err());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    assert!(!external.join("logs").exists());
}

#[test]
fn embedded_version_root_rejects_a_cross_product_link() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("selected-product"));
    let daemon = test_daemon_identity();
    let stable = private_zccache_cache_root(&paths, &derive_identity());
    std::fs::create_dir_all(&stable).unwrap();
    let version_root = stable.join(zccache::core::config::versioned_subdir());
    let external = temp.path().join("other-product-version");
    std::fs::create_dir_all(&external).unwrap();
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/c", "mklink", "/J"])
            .arg(&version_root)
            .arg(&external);
        let output = running_process::run_std_command_bounded(
            command,
            Some(std::time::Duration::from_secs(30)),
            64 * 1024,
        )
        .unwrap();
        assert_eq!(output.exit_code, 0, "mklink /J must create the junction");
    } else {
        crate::platform::fs::links::create(&external.display().to_string(), &version_root, true)
            .expect("create directory symlink");
    }
    assert!(prepare_embedded_cache_root(&paths, &daemon, &stable).is_err());
    assert!(!external.join("logs").exists());
}

#[test]
fn exact_same_root_legacy_identity_wins_over_newer_siblings() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let daemon = test_daemon_identity();
    let stable = private_zccache_cache_root(&paths, &derive_identity());
    let exact =
        private_zccache_cache_root(&paths, &derive_legacy_identity(&paths, &daemon.exe_path));
    let sibling = stable
        .parent()
        .expect("stable parent")
        .join("11111111111111111111111111111111");
    std::fs::create_dir_all(&exact).expect("create exact legacy root");
    std::fs::write(exact.join("selected"), b"exact").expect("write exact marker");
    std::fs::create_dir_all(&sibling).expect("create sibling legacy root");
    std::fs::write(sibling.join("selected"), b"sibling").expect("write sibling marker");

    migrate_legacy_cache_root(&paths, &daemon, &stable).expect("migrate exact root");

    assert_eq!(
        std::fs::read(stable.join("selected")).expect("read migrated marker"),
        b"exact"
    );
    assert!(sibling.is_dir(), "unselected sibling must remain untouched");
}

#[test]
fn relocated_legacy_cache_uses_uniquely_newest_backend() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("relocated"));
    let daemon = test_daemon_identity();
    let stable = private_zccache_cache_root(&paths, &derive_identity());
    let parent = stable.parent().expect("stable parent");
    let older = parent.join("11111111111111111111111111111111");
    let newer = parent.join("22222222222222222222222222222222");
    std::fs::create_dir_all(&older).expect("create older legacy root");
    std::fs::write(older.join("selected"), b"older").expect("write older marker");
    std::thread::sleep(std::time::Duration::from_millis(25));
    std::fs::create_dir_all(&newer).expect("create newer legacy root");
    std::fs::write(newer.join("selected"), b"newer").expect("write newer marker");

    migrate_legacy_cache_root(&paths, &daemon, &stable).expect("migrate newest root");

    assert_eq!(
        std::fs::read(stable.join("selected")).expect("read migrated marker"),
        b"newer"
    );
    assert!(
        older.is_dir(),
        "unselected older root must remain untouched"
    );
}

#[test]
fn tied_legacy_candidates_are_rejected_loudly() {
    let parent = std::path::PathBuf::from("cache/zccache/daemon-state");
    let tied = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);
    let result = select_legacy_candidate(
        &parent,
        vec![
            (tied, parent.join("11111111111111111111111111111111")),
            (tied, parent.join("22222222222222222222222222222222")),
        ],
    );
    assert!(
        matches!(
            result,
            Err(EmbeddedServiceError::AmbiguousLegacyCache { .. })
        ),
        "equal newest mtimes must not choose an arbitrary backend: {result:?}"
    );
}

#[test]
fn save_load_restores_the_selected_private_subtree() {
    use crate::cache_lib::save::{
        load, save, LoadOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let cold = SoldrPaths::with_root(temp.path().join("cache-cold"));
    let warm = SoldrPaths::with_root(temp.path().join("cache-warm"));
    let identity = derive_identity();
    let cold_object = private_zccache_cache_root(&cold, &identity)
        .join("artifacts")
        .join("probe-object");
    std::fs::create_dir_all(cold_object.parent().expect("object parent"))
        .expect("create cold object directory");
    std::fs::write(&cold_object, b"portable-cache-object").expect("write cold object");

    let archive = temp.path().join("cache.tar.zst");
    save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cold.cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: Some(1),
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save cold cache");
    load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&warm.cache),
        workspace: None,
        threads: Some(1),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load warm cache");

    let warm_object = private_zccache_cache_root(&warm, &identity)
        .join("artifacts")
        .join("probe-object");
    assert_eq!(
        std::fs::read(warm_object).expect("read restored object"),
        b"portable-cache-object",
    );
}

// The executable fake-compiler probe moved to
// `tests/daemon_zccache_embedded.rs` (`#![cfg(unix)]`) — it needs a
// Unix shebang and 0o755 (#2493).

#[test]
fn unusable_proxy_probe_reports_complete_diagnostics() {
    let compiler = std::path::Path::new("rustc-proxy");
    let error = validate_compiler_probe(
        compiler,
        Ok(CompilerProbeOutput {
            success: false,
            exit_code: Some(1),
            stdout: b"proxy stdout".to_vec(),
            stderr: b"compiler component is not applicable".to_vec(),
        }),
    )
    .expect_err("unusable proxy must fail");
    assert!(error.contains("path=rustc-proxy"));
    assert!(error.contains("exit_code=Some(1)"));
    assert!(error.contains("proxy stdout"));
    assert!(error.contains("compiler component is not applicable"));
}

#[test]
fn successful_non_compiler_probe_is_rejected() {
    let compiler = std::path::Path::new("not-rustc");
    let error = validate_compiler_probe(
        compiler,
        Ok(CompilerProbeOutput {
            success: true,
            exit_code: Some(0),
            stdout: b"some unrelated executable\n".to_vec(),
            stderr: b"unexpected shim diagnostics".to_vec(),
        }),
    )
    .expect_err("non-rustc output must fail");
    assert!(error.contains("path=not-rustc"));
    assert!(error.contains("unexpected rustc -vV output"));
    assert!(error.contains("some unrelated executable"));
    assert!(error.contains("unexpected shim diagnostics"));
}

#[test]
fn missing_compiler_probe_reports_path_and_spawn_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let compiler = temp.path().join("missing-compiler");
    let error = probe_working_compiler(&compiler).expect_err("missing compiler must fail");
    assert!(error.contains(&format!("path={}", compiler.display())));
    assert!(error.contains("spawn_error="));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rustc_hit_survives_full_and_ci_save_load_relocation() {
    use crate::cache_lib::save::{
        load, save, LoadOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
    };

    let current_dir = std::env::current_dir().expect("resolve test working directory");
    let repo_workspace = current_dir
        .ancestors()
        .find(|candidate| candidate.join("rust-toolchain.toml").is_file())
        .expect("find repository rust-toolchain.toml from test working directory");
    let pinned_toolchain = crate::core::read_rust_toolchain_manifest(repo_workspace)
        .expect("read repository rust-toolchain.toml")
        .channel
        .expect("repository rust-toolchain.toml must declare a channel");
    let rustc = crate::test_support::rustc_from_env_or_path();
    let compiler_version =
        probe_working_compiler(&rustc).unwrap_or_else(|error| panic!("{error}"));
    eprintln!(
        "using verified compiler {}: {}",
        rustc.display(),
        compiler_version.lines().next().unwrap_or("unknown version")
    );
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("workspace");
    std::fs::create_dir_all(project.join("src")).expect("create source directory");
    std::fs::write(
        project.join("src/lib.rs"),
        "pub fn portable_cache_answer() -> u32 { 1651 }\n",
    )
    .expect("write source");

    let rustc_args = vec![
        rustc.display().to_string(),
        "--edition".into(),
        "2021".into(),
        "--crate-type".into(),
        "lib".into(),
        "--crate-name".into(),
        "soldr_portable_cache".into(),
        "--emit=dep-info,metadata,link".into(),
        "-C".into(),
        "embed-bitcode=no".into(),
        "-C".into(),
        "metadata=z1651".into(),
        "-C".into(),
        "extra-filename=-z1651".into(),
        "--out-dir".into(),
        "target/debug/deps".into(),
        "src/lib.rs".into(),
    ];
    let mut compile_env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key != "RUSTUP_TOOLCHAIN")
        .collect();
    compile_env.push(("RUSTUP_TOOLCHAIN".into(), pinned_toolchain));
    let request = || CompileRequest {
        args: rustc_args.clone(),
        cwd: project.display().to_string(),
        env: compile_env.clone(),
        stdin: Vec::new(),
        lifecycle: None,
        ipc_busy_retries: 0,
    };
    let daemon = test_daemon_identity();
    let cold = SoldrPaths::with_root(temp.path().join("cold-root"));
    let cold_service = SoldrZccacheService::start(&cold, &daemon)
        .await
        .expect("start cold embedded service");
    let first = cold_service.compile(request()).await.expect("cold compile");
    assert_eq!(
        first.exit_code,
        0,
        "cold rustc failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(!first.cached, "first compile must populate the cache");
    assert_eq!(first.cache_outcome, 2, "first compile must be a miss");
    let flush = cold_service
        .flush()
        .await
        .expect("flush cold service before inspecting durable state");
    assert!(
        flush.is_complete(),
        "cold service durability barrier must complete before archive: {flush:?}"
    );
    let cold_stats = cold_service
        .inner
        .stats()
        .await
        .expect("read cold service stats");
    assert!(
        cold_stats.dep_graph_contexts > 0 && cold_stats.artifact_count > 0,
        "cold compile must populate depgraph and artifact state: {cold_stats:?}"
    );
    cold_service
        .shutdown(ShutdownMode::Graceful)
        .await
        .expect("shutdown cold service");

    for profile in [SaveProfile::Full, SaveProfile::Ci] {
        let archive = temp.path().join(format!("{}.tar.zst", profile.as_str()));
        save(&SaveOptions {
            workspace: None,
            cache_dir: Some(&cold.cache),
            out: &archive,
            zstd_level: DEFAULT_ZSTD_LEVEL,
            threads: Some(2),
            mtimes_only: false,
            profile,
        })
        .unwrap_or_else(|error| panic!("save {} profile: {error}", profile.as_str()));

        let warm =
            SoldrPaths::with_root(temp.path().join(format!("warm-{}-root", profile.as_str())));
        load(&LoadOptions {
            archive: &archive,
            cache_dir: Some(&warm.cache),
            workspace: None,
            threads: Some(2),
            mtimes_only: false,
            profile_extract: false,
            auto_defender_exclude: false,
        })
        .unwrap_or_else(|error| panic!("load {} profile: {error}", profile.as_str()));

        if project.join("target").exists() {
            std::fs::remove_dir_all(project.join("target"))
                .expect("remove compiler outputs before restored hit");
        }
        let warm_service = SoldrZccacheService::start(&warm, &daemon)
            .await
            .unwrap_or_else(|error| panic!("start {} restored service: {error}", profile.as_str()));
        let restored_stats = warm_service.inner.stats().await.unwrap_or_else(|error| {
            panic!("read {} restored service stats: {error}", profile.as_str())
        });
        assert!(
            restored_stats.dep_graph_contexts > 0 && restored_stats.artifact_count > 0,
            "{} restore must load depgraph and artifact state: {restored_stats:?}",
            profile.as_str()
        );
        let restored = warm_service
            .compile(request())
            .await
            .unwrap_or_else(|error| panic!("{} restored compile: {error}", profile.as_str()));
        assert_eq!(
            restored.exit_code,
            0,
            "{} restored rustc failed: {}",
            profile.as_str(),
            String::from_utf8_lossy(&restored.stderr)
        );
        assert!(
            restored.cached,
            "{} save/load into another root must produce a real rustc cache hit; pre-compile stats: {restored_stats:?}",
            profile.as_str(),
        );
        assert_eq!(
            restored.cache_outcome,
            1,
            "{} restored compile must report Hit",
            profile.as_str()
        );
        warm_service
            .shutdown(ShutdownMode::Graceful)
            .await
            .unwrap_or_else(|error| {
                panic!("shutdown {} restored service: {error}", profile.as_str())
            });
    }
}

#[test]
fn private_root_is_stable_per_backend_identity() {
    let paths = SoldrPaths::with_root(std::path::PathBuf::from("/tmp/soldr"));
    let first = HostIdentity {
        product: "soldr".into(),
        instance_id: "backend-a".into(),
        workspace_id: "workspace-a".into(),
    };
    let second = HostIdentity {
        product: "soldr".into(),
        instance_id: "backend-b".into(),
        workspace_id: "workspace-b".into(),
    };
    assert_eq!(
        private_zccache_cache_root(&paths, &first),
        paths.cache.join("zccache/daemon-state/backend-a")
    );
    assert_ne!(
        private_zccache_cache_root(&paths, &first),
        private_zccache_cache_root(&paths, &second)
    );
}
