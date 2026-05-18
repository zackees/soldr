use super::cargo_front_door::{
    cargo_args_specify_target, cargo_args_use_reserved_no_cache, cargo_profile,
    cargo_target_triple, first_cargo_subcommand, low_disk_warning_for_free_bytes,
    low_disk_warning_for_path, selected_cargo_args,
};
use super::gc::{gc_purge_worker_count_for, parse_gc_purge_answer};
use super::rust_plan::{
    allowed_artifact_classes, build_rust_artifact_plan, build_thin_manifest,
    cargo_metadata_passthrough_args, compute_plan_inputs_hash, dropped_artifact_classes,
    evaluate_warm_restore_skip, parse_rust_artifact_cache_tar_threads,
    resolve_bundle_walk_thread_count, should_skip_warm_restore, walk_bundle_files,
    warm_restore_sentinel_path, warm_restore_skip_enabled, write_thin_manifest,
    write_warm_restore_sentinel, CargoMetadata, CargoMetadataPackage, RustArtifactPlan,
    RustArtifactPlanContext, RustPlanInputs, RustPlanPackages, RustToolchainIdentity,
    ThinSliceManifest, WarmRestoreSentinel, WarmRestoreSkipInputs, BUNDLE_WALK_THREAD_CAP,
};
use super::wrapper::stderr_indicates_unknown_session;
use super::zccache::{
    is_sccache_wrapper, rustc_wrapper_mode_from_env_var, RustcWrapperMode, ZccacheBuildSession,
};
use super::{
    extract_as_pin, normalize_version, parse_tool_spec, rustup_resolution_failure,
    should_self_relocate_for_invocation, should_trampoline, Cli, Commands, GcSubcommand,
    LOW_DISK_WARNING_THRESHOLD_BYTES, RUSTC_WRAPPER_OVERRIDE_ENV_VAR, SKIP_WARM_RESTORE_ENV_VAR,
    THIN_MANIFEST_FILENAME, WARM_RESTORE_MAX_AGE_SECONDS,
};
use clap::Parser;
use soldr_fetch::VersionSpec;
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;
use tempfile::TempDir;

/// Serialises tests that mutate process-wide environment variables so
/// they do not race with each other under parallel `cargo test`. The
/// guard objects below restore the previous value on drop, but two
/// tests touching the same key concurrently would still observe each
/// other's mid-test state without this lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that sets or removes an environment variable for the
/// duration of a test and restores the previous value on drop. Modelled
/// after the same helper in `soldr-core`'s test module.
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

#[test]
fn gc_cli_parses_summary_and_purge_modes() {
    let summary = Cli::try_parse_from(["soldr", "gc", "--json"]).unwrap();
    match summary.command {
        Commands::Gc {
            command: None,
            json,
            ..
        } => assert!(json, "gc --json should parse as summary JSON"),
        _ => panic!("expected gc summary command"),
    }

    let purge = Cli::try_parse_from([
        "soldr",
        "gc",
        "purge",
        "--all",
        "--older-than",
        "30d",
        "--larger-than",
        "1GB",
    ])
    .unwrap();
    match purge.command {
        Commands::Gc {
            command:
                Some(GcSubcommand::Purge {
                    all,
                    older_than,
                    larger_than,
                    ..
                }),
            ..
        } => {
            assert!(all);
            assert_eq!(older_than, "30d");
            assert_eq!(larger_than, "1GB");
        }
        _ => panic!("expected gc purge command"),
    }
}

#[test]
fn gc_purge_prompt_defaults_enter_to_yes() {
    for input in ["", "\n", "y", "Y", "yes", " YES "] {
        assert!(parse_gc_purge_answer(input), "expected {input:?} to accept");
    }
    for input in ["n", "no", "anything else"] {
        assert!(!parse_gc_purge_answer(input), "expected {input:?} to skip");
    }
}

#[test]
fn gc_purge_worker_count_is_bounded() {
    assert_eq!(gc_purge_worker_count_for(0), 1);
    assert_eq!(gc_purge_worker_count_for(1), 1);
    assert_eq!(gc_purge_worker_count_for(2), 2);
    assert_eq!(gc_purge_worker_count_for(16), 4);
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
fn rustc_wrapper_override_defaults_to_managed_zccache() {
    assert_eq!(
        rustc_wrapper_mode_from_env_var(None),
        RustcWrapperMode::ManagedZccache
    );
}

#[test]
fn rustc_wrapper_override_disables_wrapper_for_empty_or_none() {
    for value in ["", " ", "none", "NONE"] {
        assert_eq!(
            rustc_wrapper_mode_from_env_var(Some(OsStr::new(value))),
            RustcWrapperMode::Disabled,
            "expected {value:?} to disable wrapper injection"
        );
    }
}

#[test]
fn rustc_wrapper_override_uses_custom_wrapper_name() {
    assert_eq!(
        rustc_wrapper_mode_from_env_var(Some(OsStr::new("sccache"))),
        RustcWrapperMode::Custom("sccache".into())
    );
}

#[test]
fn self_relocate_gate_targets_managed_cacheable_cargo_builds() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _wrapper = EnvVarGuard::remove(RUSTC_WRAPPER_OVERRIDE_ENV_VAR);

    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--as".into(),
        env!("CARGO_PKG_VERSION").into(),
        "cargo".into(),
        "test".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "--version".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--no-cache".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "version".into(),
    ]));

    let _custom = EnvVarGuard::set(RUSTC_WRAPPER_OVERRIDE_ENV_VAR, "sccache");
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "build".into(),
    ]));
}

#[test]
fn sccache_wrapper_detection_accepts_binary_names_and_paths() {
    assert!(is_sccache_wrapper(OsStr::new("sccache")));
    assert!(is_sccache_wrapper(OsStr::new("sccache.exe")));
    assert!(is_sccache_wrapper(OsStr::new("/tmp/tools/sccache")));
    assert!(!is_sccache_wrapper(OsStr::new("zccache")));
    assert!(!is_sccache_wrapper(OsStr::new("sccache-proxy")));
}

#[test]
fn parse_tool_spec_defaults_to_latest_version() {
    let (tool, version) = parse_tool_spec("maturin");
    assert_eq!(tool, "maturin");
    assert!(matches!(version, VersionSpec::Latest));
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

#[test]
fn rust_artifact_plan_selects_external_packages_and_path_exclusions() {
    let root = std::env::temp_dir().join(format!("soldr-rust-plan-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::create_dir_all(root.join("local_dep/src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();
    std::fs::write(
        root.join("local_dep/Cargo.toml"),
        "[package]\nname='local_dep'\n",
    )
    .unwrap();

    let metadata = CargoMetadata {
        workspace_root: root.clone(),
        target_directory: root.join("target"),
        workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
        packages: vec![
            CargoMetadataPackage {
                id: "path+file:///repo/app#app@0.1.0".to_string(),
                source: None,
            },
            CargoMetadataPackage {
                id: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0".to_string(),
                source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
            },
            CargoMetadataPackage {
                id: "path+file:///repo/local_dep#local_dep@0.1.0".to_string(),
                source: None,
            },
        ],
    };
    let toolchain = RustToolchainIdentity {
        rustc: "rustc 1.0.0-test".to_string(),
        cargo: "cargo 1.0.0-test".to_string(),
        channel: "test".to_string(),
        host: "x86_64-unknown-test".to_string(),
    };
    let session = ZccacheBuildSession {
        binary_path: "zccache".into(),
        cache_dir: root.join("cache"),
        session_id: "session-1".to_string(),
        session_log_path: root.join("cache/logs/last-session.log"),
        journal_path: root.join("cache/logs/last-session.jsonl"),
        session_stats_path: root.join("cache/logs/last-session-stats.json"),
    };
    let args = vec![
        "build".to_string(),
        "--release".to_string(),
        "--features".to_string(),
        "serde/derive".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];

    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        &args,
        "thin",
        Some("thin-v1"),
        &session,
        None,
    )
    .expect("build rust artifact plan");

    assert_eq!(plan.schema_version, 1);
    assert_eq!(plan.mode, "thin");
    assert_eq!(plan.cache_profile, Some("thin-v1"));
    assert_eq!(plan.profile, "release");
    assert_eq!(plan.target_triple, "x86_64-unknown-linux-gnu");
    assert_eq!(plan.packages.workspace_package_ids.len(), 1);
    assert_eq!(plan.packages.selected_package_ids.len(), 1);
    assert!(plan.packages.selected_package_ids[0].contains("serde"));
    assert_eq!(plan.packages.excluded_path_package_ids.len(), 1);
    assert!(plan.allowed_artifact_classes.contains(&"cargo_fingerprint"));
    assert!(plan.dropped_artifact_classes.is_empty());
    assert_eq!(plan.cache_schema_version, 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rust_artifact_plan_helpers_parse_mode_profile_target_and_metadata_args() {
    let args = vec![
        "+stable".to_string(),
        "build".to_string(),
        "--locked".to_string(),
        "--features=fast".to_string(),
        "--target".to_string(),
        "wasm32-unknown-unknown".to_string(),
        "--profile".to_string(),
        "release-lto".to_string(),
        "--".to_string(),
        "--ignored".to_string(),
    ];

    assert_eq!(cargo_profile(&args), "release-lto");
    assert_eq!(
        cargo_target_triple(&args, "x86_64-unknown-linux-gnu"),
        "wasm32-unknown-unknown"
    );
    assert_eq!(
        selected_cargo_args(&args, &["--features"]),
        vec!["--features=fast".to_string()]
    );
    assert_eq!(allowed_artifact_classes("full", None), Vec::<&str>::new());
    assert_eq!(
        cargo_metadata_passthrough_args(&args)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        vec!["--locked".to_string(), "--features=fast".to_string()]
    );
}

/// `thin-v1` is the legacy slice. It must continue to ship the
/// historically-included library-output classes so rollout day 0 is a
/// no-op for callers that did not opt in to `thin-v2`.
#[test]
fn allowed_artifact_classes_thin_v1_keeps_legacy_set() {
    let allowed = allowed_artifact_classes("thin", Some("thin-v1"));
    for expected in [
        "rlib",
        "rmeta",
        "dep_info",
        "proc_macro",
        "cargo_fingerprint",
        "build_script_metadata",
        "build_script_output",
    ] {
        assert!(
            allowed.contains(&expected),
            "thin-v1 must keep {expected} in the allowlist; got {allowed:?}"
        );
    }
    assert!(dropped_artifact_classes("thin", Some("thin-v1")).is_empty());
}

/// `thin-v2` aggressively prunes the slice. The categories listed in
/// `docs/THIN_TARGET_CACHE_PRUNING.md` Section 3.2 must NOT appear in the
/// allowlist, and the new fingerprint split (`cargo_fingerprint_meta`,
/// dropping `cargo_fingerprint_outputs`) must be honored.
#[test]
fn allowed_artifact_classes_thin_v2_drops_heavy_categories() {
    let allowed = allowed_artifact_classes("thin", Some("thin-v2"));

    // Drop list per design Section 3.2.
    for forbidden in [
        "rlib",
        "rmeta",
        "proc_macro",
        "incremental",
        "cargo_fingerprint",
        "cargo_fingerprint_outputs",
        "build_script_build",
        "dwo",
        "pdb",
        "dsym",
    ] {
        assert!(
            !allowed.contains(&forbidden),
            "thin-v2 must drop {forbidden} from the allowlist; got {allowed:?}"
        );
    }

    // Keep list per design Section 3.1.
    for required in [
        "cargo_fingerprint_meta",
        "dep_info",
        "build_script_metadata",
        "build_script_output",
    ] {
        assert!(
            allowed.contains(&required),
            "thin-v2 must keep {required} in the allowlist; got {allowed:?}"
        );
    }

    // The drop list is surfaced as data so zccache can short-circuit.
    let dropped = dropped_artifact_classes("thin", Some("thin-v2"));
    for forbidden in [
        "incremental",
        "rlib",
        "rmeta",
        "proc_macro",
        "build_script_build",
        "dwo",
        "pdb",
        "dsym",
        "cargo_fingerprint_outputs",
    ] {
        assert!(
            dropped.contains(&forbidden),
            "thin-v2 must publish {forbidden} in dropped_artifact_classes; got {dropped:?}"
        );
    }
}

/// Bumping `cache_schema_version` from 1 to 2 is the contract zccache
/// uses to decide whether the new fingerprint split is in effect.
#[test]
fn rust_artifact_plan_bumps_cache_schema_version_for_thin_v2() {
    let root = std::env::temp_dir().join(format!(
        "soldr-rust-plan-thinv2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos(),
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("app/src")).unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();

    let metadata = CargoMetadata {
        workspace_root: root.clone(),
        target_directory: root.join("target"),
        workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
        packages: vec![CargoMetadataPackage {
            id: "path+file:///repo/app#app@0.1.0".to_string(),
            source: None,
        }],
    };
    let toolchain = RustToolchainIdentity {
        rustc: "rustc 1.0.0-test".to_string(),
        cargo: "cargo 1.0.0-test".to_string(),
        channel: "test".to_string(),
        host: "x86_64-unknown-test".to_string(),
    };
    let session = ZccacheBuildSession {
        binary_path: "zccache".into(),
        cache_dir: root.join("cache"),
        session_id: "session-thinv2".to_string(),
        session_log_path: root.join("cache/logs/last-session.log"),
        journal_path: root.join("cache/logs/last-session.jsonl"),
        session_stats_path: root.join("cache/logs/last-session-stats.json"),
    };

    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        &["build".to_string()],
        "thin",
        Some("thin-v2"),
        &session,
        None,
    )
    .expect("build rust artifact plan");

    assert_eq!(plan.schema_version, 1, "outer schema is unchanged");
    assert_eq!(
        plan.cache_schema_version, 2,
        "thin-v2 bumps the cache-side schema so zccache can branch on it"
    );
    assert_eq!(plan.cache_profile, Some("thin-v2"));
    assert!(plan.allowed_artifact_classes.contains(&"dep_info"));
    assert!(!plan.allowed_artifact_classes.contains(&"rlib"));

    let _ = std::fs::remove_dir_all(&root);
}

/// The manifest must enumerate every regular file in the bundle, with
/// relative POSIX-style paths and either a size or `null`. It must NOT
/// list directories or its own filename.
#[test]
fn thin_manifest_enumerates_only_files_actually_present() {
    let bundle = tempfile::tempdir().expect("tempdir for bundle");
    let bundle_path = bundle.path();

    // Build a representative bundle layout: nested dir + a file at root +
    // an empty subdir (must not appear in the manifest).
    std::fs::create_dir_all(bundle_path.join("debug/.fingerprint/foo-abc")).unwrap();
    std::fs::create_dir_all(bundle_path.join("debug/deps")).unwrap();
    std::fs::create_dir_all(bundle_path.join("debug/empty_subdir")).unwrap();
    std::fs::write(
        bundle_path.join("debug/.fingerprint/foo-abc/invoked.timestamp"),
        "",
    )
    .unwrap();
    std::fs::write(
        bundle_path.join("debug/.fingerprint/foo-abc/dep-lib-foo"),
        b"abc123",
    )
    .unwrap();
    std::fs::write(bundle_path.join("debug/deps/foo-abc.d"), b"foo.rs:\n").unwrap();
    std::fs::write(bundle_path.join("CACHEDIR.TAG"), b"Signature: 8a4773\n").unwrap();

    let manifest = build_thin_manifest(bundle_path, "thin-v2").expect("build manifest");

    assert_eq!(manifest.schema_version, 2);
    assert_eq!(manifest.cache_profile, "thin-v2");

    let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    // Sorted, POSIX-style, no manifest self-reference, no empty dir.
    assert_eq!(
        paths,
        vec![
            "CACHEDIR.TAG",
            "debug/.fingerprint/foo-abc/dep-lib-foo",
            "debug/.fingerprint/foo-abc/invoked.timestamp",
            "debug/deps/foo-abc.d",
        ],
    );
    // Sizes are populated for files that exist on disk.
    let by_path: std::collections::HashMap<_, _> = manifest
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.size_bytes))
        .collect();
    assert_eq!(
        by_path.get("debug/.fingerprint/foo-abc/dep-lib-foo"),
        Some(&Some(6))
    );
    assert_eq!(
        by_path.get("debug/.fingerprint/foo-abc/invoked.timestamp"),
        Some(&Some(0))
    );
}

/// The on-disk manifest emitted by `write_thin_manifest` must round-trip
/// through serde so downstream verifiers can deserialize it without
/// surprises (no field renames, no missing fields).
#[test]
fn thin_manifest_round_trips_through_serde() {
    let bundle = tempfile::tempdir().expect("tempdir for manifest round-trip");
    let bundle_path = bundle.path();
    std::fs::create_dir_all(bundle_path.join("debug/deps")).unwrap();
    std::fs::write(
        bundle_path.join("debug/deps/example.d"),
        b"example: src/lib.rs\n",
    )
    .unwrap();

    write_thin_manifest(bundle_path, Some("thin-v2")).expect("write manifest");

    let manifest_path = bundle_path.join(THIN_MANIFEST_FILENAME);
    assert!(
        manifest_path.is_file(),
        "manifest must land at the well-known path"
    );

    let raw = std::fs::read_to_string(&manifest_path).expect("read manifest");
    let parsed: ThinSliceManifest = serde_json::from_str(&raw).expect("deserialize manifest");

    assert_eq!(parsed.schema_version, 2);
    assert_eq!(parsed.cache_profile, "thin-v2");
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].path, "debug/deps/example.d");

    // Serializing the parsed value back must produce a JSON document that
    // deserializes to an equal value (canonical round-trip).
    let serialized = serde_json::to_string(&parsed).expect("serialize manifest");
    let reparsed: ThinSliceManifest =
        serde_json::from_str(&serialized).expect("re-deserialize manifest");
    assert_eq!(parsed, reparsed);
}

/// A second `write_thin_manifest` call into the same bundle directory
/// must not list the previously-written manifest among its own entries.
#[test]
fn thin_manifest_does_not_self_reference_on_repeat_save() {
    let bundle = tempfile::tempdir().expect("tempdir for repeat save");
    let bundle_path = bundle.path();
    std::fs::write(bundle_path.join("only.txt"), b"hello").unwrap();

    write_thin_manifest(bundle_path, Some("thin-v2")).expect("first manifest write");
    write_thin_manifest(bundle_path, Some("thin-v2")).expect("second manifest write");

    let raw =
        std::fs::read_to_string(bundle_path.join(THIN_MANIFEST_FILENAME)).expect("read manifest");
    let parsed: ThinSliceManifest = serde_json::from_str(&raw).expect("parse manifest");

    let paths: Vec<&str> = parsed.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["only.txt"]);
}

#[test]
fn known_subcommand_registry_recognizes_phase_two_tools() {
    for sub in ["nextest", "deny", "audit", "llvm-cov"] {
        let spec = soldr_fetch::lookup_by_cargo_subcommand(sub)
            .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
        assert_eq!(spec.cargo_subcommand, Some(sub));
        assert!(spec.crate_name.starts_with("cargo-"));
    }
}

#[test]
fn known_subcommand_registry_recognizes_phase_three_tools() {
    for sub in ["udeps", "semver-checks", "expand", "watch"] {
        let spec = soldr_fetch::lookup_by_cargo_subcommand(sub)
            .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
        assert_eq!(spec.cargo_subcommand, Some(sub));
        assert!(spec.crate_name.starts_with("cargo-"));
    }
}

#[test]
fn top_level_tools_are_not_cargo_subcommands() {
    for crate_name in [
        "cross",
        "mdbook",
        "cbindgen",
        "wasm-pack",
        "trunk",
        "sccache",
    ] {
        let spec = soldr_fetch::lookup_by_crate(crate_name)
            .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
        assert_eq!(spec.cargo_subcommand, None);
    }
}

#[test]
fn soldr_itself_is_registered_for_self_trampoline() {
    let spec = soldr_fetch::lookup_by_crate("soldr")
        .expect("soldr should be registered in known_tools for --as trampoline");
    assert_eq!(spec.binary_name, "soldr");
    assert_eq!(spec.repo, Some(("zackees", "soldr")));
    assert_eq!(spec.cargo_subcommand, None);
}

#[test]
fn extract_as_pin_extracts_space_separated_flag_before_subcommand() {
    let (version, rest) = extract_as_pin(&[
        "--as".into(),
        "0.5.2".into(),
        "cargo".into(),
        "build".into(),
    ])
    .unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
}

#[test]
fn extract_as_pin_extracts_equals_form() {
    let (version, rest) =
        extract_as_pin(&["--as=0.5.2".into(), "cargo".into(), "build".into()]).unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
}

#[test]
fn extract_as_pin_preserves_other_leading_flags() {
    let (version, rest) = extract_as_pin(&[
        "--no-cache".into(),
        "--as".into(),
        "0.5.2".into(),
        "cargo".into(),
    ])
    .unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["--no-cache".to_string(), "cargo".into()]);
}

#[test]
fn extract_as_pin_ignores_flag_after_subcommand() {
    let args = vec!["cargo".into(), "--as".into(), "0.5.2".into()];
    let (version, rest) = extract_as_pin(&args).unwrap();
    assert_eq!(version, None);
    assert_eq!(rest, args);
}

#[test]
fn extract_as_pin_ignores_flag_after_passthrough_separator() {
    let args = vec!["cargo".into(), "--".into(), "--as".into(), "0.5.2".into()];
    let (version, rest) = extract_as_pin(&args).unwrap();
    assert_eq!(version, None);
    assert_eq!(rest, args);
}

#[test]
fn extract_as_pin_rejects_missing_value() {
    let err = extract_as_pin(&["--as".into()]).unwrap_err();
    assert!(err.to_string().contains("requires a version"));
}

#[test]
fn extract_as_pin_rejects_empty_value() {
    let err = extract_as_pin(&["--as".into(), "".into()]).unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
    let err2 = extract_as_pin(&["--as=".into()]).unwrap_err();
    assert!(err2.to_string().contains("requires a version"));
}

#[test]
fn extract_as_pin_rejects_duplicate_flag() {
    let err = extract_as_pin(&["--as".into(), "0.5.2".into(), "--as=0.4.0".into()]).unwrap_err();
    assert!(err.to_string().contains("more than once"));
}

#[test]
fn normalize_version_strips_leading_v() {
    assert_eq!(normalize_version("0.5.2"), "0.5.2");
    assert_eq!(normalize_version("v0.5.2"), "0.5.2");
    assert_eq!(normalize_version("  v0.5.2 "), "0.5.2");
}

#[test]
fn should_trampoline_matches_current_version_as_no_op() {
    assert!(!should_trampoline(env!("CARGO_PKG_VERSION")));
    assert!(!should_trampoline(&format!(
        "v{}",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(should_trampoline("0.0.0-not-this-version"));
}

#[test]
fn rustup_resolution_failure_appends_ci_guidance() {
    let error = rustup_resolution_failure(
        "rustc",
        b"error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed",
    );

    let rendered = error.to_string();
    assert!(rendered.contains("failed to resolve rustc via rustup: error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed"));
    assert!(rendered.contains("pins Rust in rust-toolchain.toml"));
    assert!(rendered.contains("generic stable toolchain"));
    assert!(rendered.contains("RUSTUP_TOOLCHAIN"));
    assert!(rendered.contains("setup-soldr action path"));
}

/// Regression test for the zccache v1.4.0 wire-compat bug. zccache
/// v1.4.0 deserializes the plan with `#[serde(deny_unknown_fields)]`
/// and does NOT know about `cache_profile` / `dropped_artifact_classes`.
/// Therefore the default `thin-v1` (and `full`) JSON must look exactly
/// like the pre-PR plan: neither field may appear in the JSON. The
/// thin-v2 opt-in is allowed (and required) to surface them.
#[test]
fn rust_artifact_plan_thin_v1_json_omits_new_fields_for_zccache_compat() {
    let plan = RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v1"),
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "f".to_string(),
            rustflags_hash: "r".to_string(),
            env_hash: "e".to_string(),
            lockfile_hash: "l".to_string(),
            cargo_config_hash: "c".to_string(),
            manifest_hashes: vec![],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![],
            workspace_package_ids: vec![],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec!["cargo_fingerprint"],
        dropped_artifact_classes: vec![],
        cache_schema_version: 1,
        journal_log_path: None,
    };

    let json = serde_json::to_string(&plan).expect("serialize thin-v1 plan");
    assert!(
        !json.contains("\"cache_profile\""),
        "thin-v1 plan must NOT serialize cache_profile (zccache v1.4.0 \
         rejects unknown fields); got: {json}"
    );
    assert!(
        !json.contains("\"dropped_artifact_classes\""),
        "thin-v1 plan must NOT serialize dropped_artifact_classes; got: {json}"
    );
}

/// `full` mode also predates the new fields and zccache's strict
/// deserializer rejects them, so `cache_profile == None` plus an empty
/// drop list must serialize without either field.
#[test]
fn rust_artifact_plan_full_mode_json_omits_new_fields() {
    let plan = RustArtifactPlan {
        schema_version: 1,
        mode: "full".to_string(),
        cache_profile: None,
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "f".to_string(),
            rustflags_hash: "r".to_string(),
            env_hash: "e".to_string(),
            lockfile_hash: "l".to_string(),
            cargo_config_hash: "c".to_string(),
            manifest_hashes: vec![],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![],
            workspace_package_ids: vec![],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec![],
        dropped_artifact_classes: vec![],
        cache_schema_version: 1,
        journal_log_path: None,
    };

    let json = serde_json::to_string(&plan).expect("serialize full plan");
    assert!(!json.contains("\"cache_profile\""), "got: {json}");
    assert!(
        !json.contains("\"dropped_artifact_classes\""),
        "got: {json}"
    );
}

/// thin-v2 is the opt-in that ships the new wire fields. zccache
/// builds that consume thin-v2 must see both `cache_profile` and the
/// non-empty `dropped_artifact_classes` list.
#[test]
fn rust_artifact_plan_thin_v2_json_includes_new_fields() {
    let plan = RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v2"),
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0".to_string(),
            cargo: "cargo 1.0.0".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "f".to_string(),
            rustflags_hash: "r".to_string(),
            env_hash: "e".to_string(),
            lockfile_hash: "l".to_string(),
            cargo_config_hash: "c".to_string(),
            manifest_hashes: vec![],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec![],
            workspace_package_ids: vec![],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec!["dep_info"],
        dropped_artifact_classes: vec!["rlib", "rmeta"],
        cache_schema_version: 2,
        journal_log_path: None,
    };

    let json = serde_json::to_string(&plan).expect("serialize thin-v2 plan");
    assert!(
        json.contains("\"cache_profile\":\"thin-v2\""),
        "thin-v2 must serialize cache_profile; got: {json}"
    );
    assert!(
        json.contains("\"dropped_artifact_classes\""),
        "thin-v2 must serialize dropped_artifact_classes; got: {json}"
    );
}

fn warm_restore_test_plan() -> RustArtifactPlan {
    RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v1"),
        workspace_root: "/tmp/ws".to_string(),
        target_dir: "/tmp/ws/target".to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.0.0-test".to_string(),
            cargo: "cargo 1.0.0-test".to_string(),
            channel: "stable".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "test".to_string(),
        inputs: RustPlanInputs {
            features_hash: "F".to_string(),
            rustflags_hash: "R".to_string(),
            env_hash: "E".to_string(),
            lockfile_hash: "L".to_string(),
            cargo_config_hash: "C".to_string(),
            manifest_hashes: vec!["M1".to_string(), "M2".to_string()],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["serde@1.0.0".to_string()],
            workspace_package_ids: vec!["app@0.1.0".to_string()],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec!["rlib", "rmeta"],
        dropped_artifact_classes: vec![],
        cache_schema_version: 1,
        journal_log_path: Some("/tmp/journal".to_string()),
    }
}

fn warm_restore_test_sentinel(plan: &RustArtifactPlan) -> WarmRestoreSentinel {
    WarmRestoreSentinel {
        schema_version: 1,
        plan_inputs_hash: compute_plan_inputs_hash(plan),
        target_dir: plan.target_dir.clone(),
        github_run_id: "111".to_string(),
        github_job: "test".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-1".to_string(),
        saved_at_unix_seconds: 1_000_000,
    }
}

/// The sentinel hash must change whenever any plan input cargo would
/// consult to decide freshness changes. Otherwise the warm-restore
/// short-circuit could fire across step pairs that are not actually
/// equivalent.
#[test]
fn plan_inputs_hash_changes_when_inputs_change() {
    let plan_a = warm_restore_test_plan();
    let mut plan_b = warm_restore_test_plan();
    plan_b.inputs.lockfile_hash = "different".to_string();
    assert_ne!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_b),
    );

    let mut plan_c = warm_restore_test_plan();
    plan_c.toolchain.rustc = "rustc 9.9.9".to_string();
    assert_ne!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_c),
    );

    let mut plan_d = warm_restore_test_plan();
    plan_d.target_triple = "aarch64-apple-darwin".to_string();
    assert_ne!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_d),
    );
}

/// Cosmetic plan fields (the journal path, the schema version we
/// already pin to 1) must not leak into the sentinel hash, so an
/// unrelated path swap does not invalidate the warm-restore optim.
#[test]
fn plan_inputs_hash_ignores_cosmetic_fields() {
    let plan_a = warm_restore_test_plan();
    let mut plan_b = warm_restore_test_plan();
    plan_b.journal_log_path = Some("/tmp/other-journal".to_string());
    plan_b.workspace_root = "/different/ws".to_string();
    assert_eq!(
        compute_plan_inputs_hash(&plan_a),
        compute_plan_inputs_hash(&plan_b),
    );
}

/// Happy path: sentinel proves the same plan was just saved into the
/// same target dir from the same CI job/attempt — restore is skipped.
#[test]
fn warm_restore_skip_fires_on_exact_match() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_some(), "expected skip; got {result:?}");
}

/// Plain "no sentinel on disk" must fall through to the normal restore.
#[test]
fn warm_restore_skip_falls_through_when_sentinel_missing() {
    let plan = warm_restore_test_plan();
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: "111",
        github_job: "test",
        github_run_attempt: "1",
        now_unix_seconds: 1_000_000,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    assert!(evaluate_warm_restore_skip(None, &inputs).is_none());
}

/// Sentinel from a prior re-run attempt must NOT short-circuit into a
/// fresh attempt — the action restored the cache from scratch and the
/// `target/` mtimes are no longer guaranteed to be the live ones.
#[test]
fn warm_restore_skip_rejects_mismatched_run_attempt() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: "2", // different attempt
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Sentinel from a different job in the same workflow must not bleed
/// across job boundaries even when the run id matches.
#[test]
fn warm_restore_skip_rejects_mismatched_job() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: "other-job",
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Sentinel for an unrelated target dir (e.g. a sibling workspace
/// also writing into the shared bundle dir) must not short-circuit.
#[test]
fn warm_restore_skip_rejects_mismatched_target_dir() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: "/tmp/different-target",
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Once a plan input changes (lockfile bump, new manifest, etc.) the
/// sentinel hash diverges and restore must run normally.
#[test]
fn warm_restore_skip_rejects_mismatched_inputs_hash() {
    let plan = warm_restore_test_plan();
    let mut sentinel = warm_restore_test_sentinel(&plan);
    sentinel.plan_inputs_hash = "stale-hash".to_string();
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Stale sentinels (older than the configured window) must not
/// short-circuit. Otherwise a leftover sentinel from a previous
/// workflow run could cause skipping in a fresh job that happened to
/// inherit the same env identifiers.
#[test]
fn warm_restore_skip_rejects_stale_sentinel() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let now = sentinel.saved_at_unix_seconds + WARM_RESTORE_MAX_AGE_SECONDS + 1;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// A future-version sentinel (say after a soldr upgrade that bumps
/// the schema) must be ignored, never crash, and force a normal
/// restore on the next invocation.
#[test]
fn warm_restore_skip_rejects_unknown_schema_version() {
    let plan = warm_restore_test_plan();
    let mut sentinel = warm_restore_test_sentinel(&plan);
    sentinel.schema_version = 99;
    let now = sentinel.saved_at_unix_seconds + 60;
    let inputs_hash = compute_plan_inputs_hash(&plan);
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &sentinel.github_run_id,
        github_job: &sentinel.github_job,
        github_run_attempt: &sentinel.github_run_attempt,
        now_unix_seconds: now,
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
    assert!(result.is_none());
}

/// Sentinel must round-trip as JSON without dropping fields, so
/// disk-roundtrip behavior is observable here too (the
/// filesystem-bound caller relies on serde to be exact).
#[test]
fn warm_restore_sentinel_round_trips_json() {
    let plan = warm_restore_test_plan();
    let sentinel = warm_restore_test_sentinel(&plan);
    let json = serde_json::to_string(&sentinel).expect("serialize sentinel");
    let parsed: WarmRestoreSentinel = serde_json::from_str(&json).expect("parse sentinel back");
    assert_eq!(parsed, sentinel);
}

/// Build a `RustArtifactPlanContext` whose plan-derived fields match
/// `plan` and whose filesystem-touching paths live under `tempdir`. The
/// other fields are filled with deterministic placeholders so tests can
/// inspect them without caring about the daemon plumbing they would
/// drive in production.
fn warm_restore_test_context(
    plan: &RustArtifactPlan,
    tempdir: &TempDir,
) -> RustArtifactPlanContext {
    let root = tempdir.path();
    RustArtifactPlanContext {
        path: root.join("plan.json"),
        zccache_binary: root.join("zccache"),
        cache_dir: root.join("cache"),
        zccache_daemon_cache_dir: root.join("daemon"),
        session_id: "session-test".to_string(),
        journal_path: root.join("journal"),
        backend: "fs".to_string(),
        cache_profile: Some("thin-v1"),
        plan_inputs_hash: compute_plan_inputs_hash(plan),
        target_dir: plan.target_dir.clone(),
    }
}

/// With the gating env var enabled, `write_warm_restore_sentinel` must
/// materialise a JSON sentinel under the plan's cache dir whose fields
/// reflect the plan inputs and the current GitHub Actions env. This is
/// the producer half of the warm-restore short-circuit.
#[test]
fn write_warm_restore_sentinel_emits_matching_json_when_enabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-42");
    let _job = EnvVarGuard::set("GITHUB_JOB", "test-job");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "3");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);

    write_warm_restore_sentinel(&ctx);

    let sentinel_path = warm_restore_sentinel_path(&ctx);
    let raw =
        std::fs::read_to_string(&sentinel_path).expect("sentinel file should exist after write");
    let sentinel: WarmRestoreSentinel =
        serde_json::from_str(&raw).expect("sentinel JSON should parse");

    assert_eq!(sentinel.schema_version, 1);
    assert_eq!(sentinel.plan_inputs_hash, ctx.plan_inputs_hash);
    assert_eq!(sentinel.target_dir, ctx.target_dir);
    assert_eq!(sentinel.github_run_id, "run-42");
    assert_eq!(sentinel.github_job, "test-job");
    assert_eq!(sentinel.github_run_attempt, "3");
    assert_eq!(sentinel.session_id, ctx.session_id);
}

/// When the gating env var is explicitly opted out (falsy value), the
/// producer must be a strict no-op so the short-circuit cannot
/// accidentally fire on the next invocation. No sentinel file should
/// appear on disk.
#[test]
fn write_warm_restore_sentinel_is_noop_when_disabled() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);

    write_warm_restore_sentinel(&ctx);

    let sentinel_path = warm_restore_sentinel_path(&ctx);
    assert!(
        !sentinel_path.exists(),
        "no sentinel should be written when {SKIP_WARM_RESTORE_ENV_VAR} is set to a falsy value"
    );
}

/// Full filesystem round-trip: write a sentinel that exactly matches
/// the current plan and CI env, then ask `should_skip_warm_restore`
/// whether it should fire. The short-circuit must return `Some` with
/// a non-empty operator-visible reason string.
#[test]
fn should_skip_warm_restore_returns_some_on_full_match() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    let sentinel_path = warm_restore_sentinel_path(&ctx);
    std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
        .expect("create sentinel parent");
    let sentinel = WarmRestoreSentinel {
        schema_version: 1,
        plan_inputs_hash: ctx.plan_inputs_hash.clone(),
        target_dir: ctx.target_dir.clone(),
        github_run_id: "run-7".to_string(),
        github_job: "build".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-prev".to_string(),
        saved_at_unix_seconds: super::rust_plan::current_unix_seconds(),
    };
    std::fs::write(
        &sentinel_path,
        serde_json::to_string(&sentinel).expect("serialize sentinel"),
    )
    .expect("write sentinel");

    let result = should_skip_warm_restore(&ctx);
    let reason = result.expect("expected Some(reason) on full match");
    assert!(
        !reason.is_empty(),
        "skip reason should be non-empty for operator visibility"
    );
}

/// A sentinel left behind by a previous invocation with a different
/// `plan_inputs_hash` (e.g. after a lockfile bump) must not fire the
/// short-circuit even when the file is otherwise present and fresh.
#[test]
fn should_skip_warm_restore_returns_none_on_hash_mismatch() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    let sentinel_path = warm_restore_sentinel_path(&ctx);
    std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
        .expect("create sentinel parent");
    let sentinel = WarmRestoreSentinel {
        schema_version: 1,
        plan_inputs_hash: "stale-hash-from-previous-step".to_string(),
        target_dir: ctx.target_dir.clone(),
        github_run_id: "run-7".to_string(),
        github_job: "build".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-prev".to_string(),
        saved_at_unix_seconds: super::rust_plan::current_unix_seconds(),
    };
    std::fs::write(
        &sentinel_path,
        serde_json::to_string(&sentinel).expect("serialize sentinel"),
    )
    .expect("write sentinel");

    assert!(should_skip_warm_restore(&ctx).is_none());
}

/// When no sentinel file exists at all, the short-circuit must fall
/// through without panicking on the missing-file IO error.
#[test]
fn should_skip_warm_restore_returns_none_when_sentinel_missing() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    assert!(!warm_restore_sentinel_path(&ctx).exists());

    assert!(should_skip_warm_restore(&ctx).is_none());
}

/// With the gating env var explicitly opted out (`"0"`), the
/// short-circuit must stay off even when a perfectly-matching sentinel
/// exists. This is the safety property that lets operators disable the
/// feature on demand without having to clear stale sentinel files.
#[test]
fn should_skip_warm_restore_returns_none_when_disabled_even_with_match() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");
    let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
    let _job = EnvVarGuard::set("GITHUB_JOB", "build");
    let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

    let tempdir = TempDir::new().expect("create tempdir");
    let plan = warm_restore_test_plan();
    let ctx = warm_restore_test_context(&plan, &tempdir);
    let sentinel_path = warm_restore_sentinel_path(&ctx);
    std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
        .expect("create sentinel parent");
    let sentinel = WarmRestoreSentinel {
        schema_version: 1,
        plan_inputs_hash: ctx.plan_inputs_hash.clone(),
        target_dir: ctx.target_dir.clone(),
        github_run_id: "run-7".to_string(),
        github_job: "build".to_string(),
        github_run_attempt: "1".to_string(),
        session_id: "session-prev".to_string(),
        saved_at_unix_seconds: super::rust_plan::current_unix_seconds(),
    };
    std::fs::write(
        &sentinel_path,
        serde_json::to_string(&sentinel).expect("serialize sentinel"),
    )
    .expect("write sentinel");

    assert!(should_skip_warm_restore(&ctx).is_none());
}

/// After the #229 validation flip, an unset env var must enable the
/// short-circuit by default. This locks in the default-on contract so
/// future refactors cannot regress it without updating the test.
#[test]
fn warm_restore_skip_enabled_defaults_on() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _skip = EnvVarGuard::remove(SKIP_WARM_RESTORE_ENV_VAR);

    assert!(
        warm_restore_skip_enabled(),
        "warm-restore skip must default to enabled when {SKIP_WARM_RESTORE_ENV_VAR} is unset"
    );
}

/// The default-on flip preserves an explicit opt-out path: each of the
/// recognised falsy spellings (`0`, `false`, `no`, `off`, empty string,
/// case-insensitive) must disable the short-circuit.
#[test]
fn warm_restore_skip_enabled_respects_explicit_falsy() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    for value in ["0", "false", "FALSE", "No", "off", "OFF", "", "  0  "] {
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, value);
        assert!(
            !warm_restore_skip_enabled(),
            "warm-restore skip must be disabled when {SKIP_WARM_RESTORE_ENV_VAR} is set to {value:?}"
        );
    }
}

// -------- stderr_indicates_unknown_session (issue #265) --------

#[test]
fn unknown_session_detector_rejects_empty_stderr() {
    assert!(!stderr_indicates_unknown_session(b""));
}

#[test]
fn unknown_session_detector_matches_exact_zccache_line() {
    let stderr = b"zccache error: unknown session: abc-123\n";
    assert!(stderr_indicates_unknown_session(stderr));
}

#[test]
fn unknown_session_detector_matches_substring_mid_line() {
    // The marker can appear anywhere in the stream, not necessarily at
    // the start of a line.
    let stderr = b"prelude blah blah unknown session: 0000 trailing\n";
    assert!(stderr_indicates_unknown_session(stderr));
}

#[test]
fn unknown_session_detector_ignores_unrelated_session_mentions() {
    // The word "session" alone is not enough; we only treat the literal
    // "unknown session:" marker as a resync trigger.
    let stderr = b"zccache info: session started\nzccache info: session ok\n";
    assert!(!stderr_indicates_unknown_session(stderr));
}

#[test]
fn unknown_session_detector_tolerates_non_utf8_bytes() {
    // Surround the marker with raw non-UTF-8 byte sequences; the
    // detector must not panic and must still find the literal needle.
    let mut stderr: Vec<u8> = vec![0xFF, 0xFE, 0x80, 0x81];
    stderr.extend_from_slice(b"zccache error: unknown session: deadbeef\n");
    stderr.extend_from_slice(&[0xC3, 0x28, 0xA0]);
    assert!(stderr_indicates_unknown_session(&stderr));
}

#[test]
fn unknown_session_detector_rejects_partial_marker() {
    // "unknown sessio" (missing the trailing "n:") must NOT match — we
    // only resync on the exact daemon-emitted marker.
    let stderr = b"unknown sessio\n";
    assert!(!stderr_indicates_unknown_session(stderr));
}

#[test]
fn tar_threads_unset_or_blank_yields_none() {
    assert!(parse_rust_artifact_cache_tar_threads("").unwrap().is_none());
    assert!(parse_rust_artifact_cache_tar_threads("   ")
        .unwrap()
        .is_none());
}

#[test]
fn tar_threads_auto_is_normalized_lowercase() {
    assert_eq!(
        parse_rust_artifact_cache_tar_threads("auto").unwrap(),
        Some("auto".to_string())
    );
    assert_eq!(
        parse_rust_artifact_cache_tar_threads("  AUTO ").unwrap(),
        Some("auto".to_string())
    );
}

#[test]
fn tar_threads_positive_integer_passes_through() {
    for raw in ["1", "4", "8", "16"] {
        assert_eq!(
            parse_rust_artifact_cache_tar_threads(raw).unwrap(),
            Some(raw.to_string())
        );
    }
}

#[test]
fn tar_threads_rejects_zero_negative_and_garbage() {
    for raw in ["0", "-1", "1.5", "twelve", "auto4", "4 threads"] {
        let err = parse_rust_artifact_cache_tar_threads(raw)
            .expect_err(&format!("expected error for {raw:?}"));
        let msg = err.to_string();
        assert!(
            msg.contains("SOLDR_TARGET_CACHE_TAR_THREADS"),
            "error for {raw:?} must mention the env var, got {msg}"
        );
    }
}

/// Unset / `auto` / case-variants of `auto` must all yield `None`, which
/// signals "use rayon's global thread pool" to `walk_bundle_files`.
#[test]
fn bundle_walk_thread_count_auto_yields_none() {
    for raw in ["", "  ", "auto", "AUTO", " Auto "] {
        assert_eq!(
            resolve_bundle_walk_thread_count(raw).unwrap(),
            None,
            "raw {raw:?} should resolve to None (auto)"
        );
    }
}

/// An explicit `1` must turn into `Some(1)` so the walk takes the
/// sequential fallback path (no rayon overhead).
#[test]
fn bundle_walk_thread_count_one_forces_sequential() {
    assert_eq!(resolve_bundle_walk_thread_count("1").unwrap(), Some(1));
}

/// In-range explicit counts pass through unmodified; values above the
/// internal cap are clamped down to `BUNDLE_WALK_THREAD_CAP`.
#[test]
fn bundle_walk_thread_count_clamps_to_cap() {
    assert_eq!(resolve_bundle_walk_thread_count("2").unwrap(), Some(2));
    assert_eq!(
        resolve_bundle_walk_thread_count("8").unwrap(),
        Some(BUNDLE_WALK_THREAD_CAP)
    );
    // 64 → capped at BUNDLE_WALK_THREAD_CAP.
    assert_eq!(
        resolve_bundle_walk_thread_count("64").unwrap(),
        Some(BUNDLE_WALK_THREAD_CAP)
    );
    assert_eq!(
        resolve_bundle_walk_thread_count("9999").unwrap(),
        Some(BUNDLE_WALK_THREAD_CAP)
    );
}

/// Garbage values inherited from the parser must still propagate as
/// errors here so callers on the bare `RUSTC_WRAPPER` passthrough path
/// (which bypasses the cargo front-door validation) get a clear message
/// instead of a silent default.
#[test]
fn bundle_walk_thread_count_rejects_garbage() {
    for raw in ["0", "twelve", "1.5"] {
        let err = resolve_bundle_walk_thread_count(raw)
            .expect_err(&format!("expected error for {raw:?}"));
        assert!(
            err.to_string().contains("SOLDR_TARGET_CACHE_TAR_THREADS"),
            "error must reference the env var name"
        );
    }
}

/// Build a bundle layout with a handful of files at varying depths and
/// verify that the walker returns one entry per regular file with the
/// correct relative path string (forward-slashed, root-relative).
fn populate_walk_bundle_fixture(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("debug/deps")).unwrap();
    std::fs::create_dir_all(root.join("debug/build")).unwrap();
    std::fs::write(root.join("debug/deps/a.rlib"), b"alpha").unwrap();
    std::fs::write(root.join("debug/deps/b.rmeta"), b"beta!!").unwrap();
    std::fs::write(root.join("debug/build/c.txt"), b"gamma").unwrap();
    std::fs::write(root.join("top.txt"), b"delta-delta").unwrap();
}

/// The sequential path (`Some(1)`) must enumerate every file with the
/// expected relative paths and sizes. This is the baseline against which
/// the parallel walks are compared for determinism.
#[test]
fn walk_bundle_files_sequential_lists_every_file_with_size() {
    let bundle = tempfile::tempdir().expect("tempdir");
    populate_walk_bundle_fixture(bundle.path());

    let mut entries =
        walk_bundle_files(bundle.path(), Some(1)).expect("sequential walk must succeed");
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let observed: Vec<_> = entries
        .iter()
        .map(|e| (e.path.as_str(), e.size_bytes))
        .collect();
    assert_eq!(
        observed,
        vec![
            ("debug/build/c.txt", Some(5)),
            ("debug/deps/a.rlib", Some(5)),
            ("debug/deps/b.rmeta", Some(6)),
            ("top.txt", Some(11)),
        ]
    );
}

/// Output of the walk must be byte-identical (after the caller's
/// canonical sort) regardless of whether the metadata phase ran
/// sequentially, on rayon's global pool, or on a scoped explicit pool.
/// This is the determinism acceptance criterion from issue #272.
#[test]
fn walk_bundle_files_parallel_matches_sequential_after_sort() {
    let bundle = tempfile::tempdir().expect("tempdir");
    populate_walk_bundle_fixture(bundle.path());

    let mut sequential =
        walk_bundle_files(bundle.path(), Some(1)).expect("sequential walk must succeed");
    sequential.sort_by(|a, b| a.path.cmp(&b.path));

    for thread_count in [None, Some(2), Some(BUNDLE_WALK_THREAD_CAP)] {
        let mut parallel = walk_bundle_files(bundle.path(), thread_count)
            .unwrap_or_else(|e| panic!("walk failed with thread_count {thread_count:?}: {e}"));
        parallel.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            parallel, sequential,
            "thread_count {thread_count:?} produced a different file list after canonical sort"
        );
    }
}

/// A missing root is not an error — the bundle may legitimately not
/// exist yet (e.g. zccache restore produced nothing). The walk must
/// return an empty vec rather than propagating a `NotFound` IO error.
#[test]
fn walk_bundle_files_missing_root_returns_empty() {
    let bundle = tempfile::tempdir().expect("tempdir");
    let missing = bundle.path().join("never-created");
    for thread_count in [Some(1), None, Some(4)] {
        let entries = walk_bundle_files(&missing, thread_count)
            .unwrap_or_else(|e| panic!("missing root must not error ({thread_count:?}): {e}"));
        assert!(
            entries.is_empty(),
            "missing root walk with {thread_count:?} should be empty, got {entries:?}"
        );
    }
}
