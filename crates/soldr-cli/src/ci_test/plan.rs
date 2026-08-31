use super::model::{
    CiTestPlan, CompileDomain, CompilerExecutionGroup, CookDecision, DylintTargetTrees, Invocation,
    PlanScope, ResourceLimits, Stage, SubsumedStep, WorkspaceMetadata, PLAN_SCHEMA_VERSION,
};
use crate::core::{SoldrError, TargetTriple};
use std::path::{Path, PathBuf};

const DYLINTS: &[&str] = &[
    "ban_raw_process_creation",
    "ban_raw_network_access",
    "ban_raw_local_socket_name",
    "ban_raw_ipc_transport",
    "ban_platform_cfg_outside_boundary",
    "ban_raw_env_flag",
];

pub(crate) async fn freeze(
    invocation: &Invocation,
    cache_enabled: bool,
) -> Result<CiTestPlan, SoldrError> {
    let cwd = std::env::current_dir()?;
    let root = crate::cook::resolve_manifest_dir(&cwd)?;
    reject_non_host_target(&root)?;
    let host = TargetTriple::host()?.triple().to_string();
    if let Some(target) = &invocation.requested_target {
        if target != &host {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: --target {target:?} is incompatible with the frozen host-validation domain ({host}); use `soldr cargo ...` for a separate target domain"
            )));
        }
    }
    let manifest = crate::core::read_rust_toolchain_manifest(&root)?;
    let stable_toolchain = manifest.channel.ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr ci-test: {} has no pinned [toolchain].channel",
            root.join("rust-toolchain.toml").display()
        ))
    })?;
    let nightly = resolve_dylint_plan(&root, &host).await?;
    let target_root = cargo_target_root(&root)?;
    let stable_target = target_root.join(&host);
    let config = cargo_config_paths(&root);
    let rustflags = effective_rustflags();
    let wrapper = wrapper_identity(cache_enabled);
    // Absence is load-bearing: ci-test schedules work, but it does not invent a
    // one-thread Cargo/compiler policy. Explicit values are frozen verbatim.
    let cargo_build_jobs = inherited_limit("CARGO_BUILD_JOBS");
    let soldr_jobs = inherited_limit("SOLDR_JOBS");
    let nextest_test_threads = effective_limit("NEXTEST_TEST_THREADS", "1");
    let dylint_key = canonical_channel(&nightly.channel, &host);
    let dylint_libraries = target_root
        .join("dylint")
        .join("libraries")
        .join(&dylint_key);
    let dylint_analysis = target_root.join("dylint").join("target").join(&dylint_key);
    let dylint_tests = target_root.join("dylint").join("tests").join(&dylint_key);
    // soldr#2936: the census is taken while the plan is frozen, i.e. before a
    // single stage compiles, so a workspace that is about to link 98 test
    // binaries says so up front rather than after the disk is gone. Reading
    // the filesystem (not `cargo metadata`) keeps it in the same idiom as the
    // manifest and `.cargo/config.toml` reads above, and costs no subprocess.
    let test_target_count = super::test_targets::count_workspace_test_targets(&root);
    let test_target_warn_threshold = super::test_targets::warn_threshold();
    let scope_args = invocation.scope.cargo_args();
    let mut stages = Vec::new();
    let mut fmt = vec!["fmt".into()];
    if invocation.scope.packages.is_empty() {
        fmt.push("--all".into());
    }
    fmt.extend(fmt_scope(&invocation.scope));
    fmt.extend(["--".into(), "--check".into()]);
    stages.push(stage(
        "rustfmt",
        "stable",
        PREFLIGHT,
        cargo_command(&fmt, &[]),
        &[],
        &root,
    ));
    stages.push(stage(
        "lint-ci",
        "policy",
        PREFLIGHT,
        vec!["soldr".into(), "lint".into(), "ci".into()],
        &[],
        &root,
    ));
    let mut clippy = vec!["clippy".into()];
    clippy.extend(workspace_selection(&invocation.scope));
    clippy.extend(["--all-targets".into(), "--target".into(), host.clone()]);
    clippy.extend(scope_args.clone());
    clippy.extend(["--".into(), "-D".into(), "warnings".into()]);
    stages.push(stage(
        "clippy",
        "stable",
        COMPILER,
        cargo_command(&clippy, &[]),
        &["rustfmt", "lint-ci"],
        &root,
    ));
    let library_names: Vec<String> = DYLINTS
        .iter()
        .map(|lint| format!("dylint-library-{lint}"))
        .collect();
    for (index, lint) in DYLINTS.iter().enumerate() {
        let dependency = if index == 0 {
            "clippy"
        } else {
            library_names[index - 1].as_str()
        };
        stages.push(stage(
            &library_names[index],
            "dylint-libraries",
            COMPILER,
            cargo_command(
                &[
                    "build",
                    "--manifest-path",
                    "Cargo.toml",
                    "--target-dir",
                    dylint_libraries.to_string_lossy().as_ref(),
                    "--profile",
                    "release",
                ],
                &[],
            ),
            &[dependency],
            &root.join("dylints").join(lint),
        ));
    }
    let mut dylint = vec![
        "dylint".into(),
        "--no-build".into(),
        "--all".into(),
        "--".into(),
    ];
    dylint.extend(workspace_selection(&invocation.scope));
    dylint.push("--all-targets".into());
    dylint.extend(scope_args.clone());
    stages.push(stage(
        "dylint-workspace",
        "dylint-analysis",
        COMPILER,
        cargo_command(&dylint, &[]),
        &[library_names
            .last()
            .expect("the frozen Dylint inventory is non-empty")],
        &root,
    ));
    let dylint_test_names: Vec<String> = DYLINTS
        .iter()
        .map(|lint| format!("dylint-test-{lint}"))
        .collect();
    for (index, lint) in DYLINTS.iter().enumerate() {
        let dependency = if index == 0 {
            "dylint-workspace"
        } else {
            dylint_test_names[index - 1].as_str()
        };
        stages.push(stage(
            &dylint_test_names[index],
            "dylint-ui-tests",
            COMPILER,
            cargo_command(
                &[
                    "test",
                    "--manifest-path",
                    "Cargo.toml",
                    "--target-dir",
                    dylint_tests.to_string_lossy().as_ref(),
                ],
                &[],
            ),
            &[dependency],
            &root.join("dylints").join(lint),
        ));
    }
    let mut nextest_args = workspace_selection(&invocation.scope);
    nextest_args.extend([
        "--lib".into(),
        "--tests".into(),
        "--target".into(),
        host.clone(),
        "--test-threads".into(),
        nextest_test_threads.clone(),
    ]);
    nextest_args.extend(scope_args.clone());

    // Compile every test binary before the Dylint branch begins. The
    // following `nextest` stage has the identical target selection, so Cargo
    // observes those binaries as Fresh and only executes them while Dylint
    // performs its independent nightly compilation.
    let mut nextest_compile = vec!["nextest".into(), "run".into(), "--no-run".into()];
    nextest_compile.extend(nextest_args.clone());
    stages.push(stage(
        "nextest-compile",
        "stable",
        COMPILER,
        cargo_command(&nextest_compile, &[]),
        &["clippy"],
        &root,
    ));
    let mut nextest = vec!["nextest".into(), "run".into()];
    nextest.extend(nextest_args);
    stages.push(stage(
        "nextest",
        "stable",
        TEST,
        cargo_command(&nextest, &[]),
        &["nextest-compile"],
        &root,
    ));
    let mut doctest = vec!["test".into()];
    doctest.extend(workspace_selection(&invocation.scope));
    doctest.extend(["--doc".into(), "--target".into(), host.clone()]);
    doctest.extend(scope_args.clone());
    stages.push(stage(
        "doctests",
        "rustdoc",
        COMPILER_AND_TEST,
        cargo_command(&doctest, &[]),
        &[
            "nextest",
            dylint_test_names
                .last()
                .expect("the frozen Dylint test inventory is non-empty"),
        ],
        &root,
    ));
    for (name, args) in [
        ("cargo-deny-bans", vec!["deny", "check", "bans"]),
        ("cargo-audit", vec!["audit"]),
        ("cargo-machete", vec!["machete"]),
    ] {
        stages.push(stage(
            name,
            "policy",
            POLICY,
            cargo_command(&args, &[]),
            &["doctests"],
            &root,
        ));
    }
    Ok(CiTestPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        command: "ci-test",
        workspace_root: root.display().to_string(),
        workspace_metadata: WorkspaceMetadata {
            manifest_path: root.join("Cargo.toml").display().to_string(),
            lockfile_path: root.join("Cargo.lock").display().to_string(),
            cargo_config: config.clone(),
            fingerprint: workspace_metadata_fingerprint(&root, &config)?,
        },
        host_triple: host.clone(),
        scope: PlanScope {
            packages: invocation.scope.packages.clone(),
            features: invocation.scope.features.clone(),
            all_features: invocation.scope.all_features,
            no_default_features: invocation.scope.no_default_features,
        },
        domains: vec![
            CompileDomain {
                id: "stable",
                family: "stable",
                toolchain: stable_toolchain.clone(),
                compiler_release: None,
                compiler_commit: None,
                target_triple: host.clone(),
                target_directory: stable_target.display().to_string(),
                profile: "test",
                rustflags: rustflags.clone(),
                cargo_config: config.clone(),
                wrapper_identity: wrapper.clone(),
            },
            CompileDomain {
                id: "dylint-libraries",
                family: "dylint-nightly",
                toolchain: nightly.channel.clone(),
                compiler_release: Some(nightly.compiler_release.clone()),
                compiler_commit: Some(nightly.compiler_commit.clone()),
                target_triple: host.clone(),
                target_directory: dylint_libraries.display().to_string(),
                profile: "release",
                rustflags: rustflags.clone(),
                cargo_config: config.clone(),
                wrapper_identity: wrapper.clone(),
            },
            CompileDomain {
                id: "dylint-analysis",
                family: "dylint-nightly",
                toolchain: nightly.channel.clone(),
                compiler_release: Some(nightly.compiler_release.clone()),
                compiler_commit: Some(nightly.compiler_commit.clone()),
                target_triple: host.clone(),
                target_directory: dylint_analysis.display().to_string(),
                profile: "dev/check",
                rustflags: rustflags.clone(),
                cargo_config: config.clone(),
                wrapper_identity: wrapper.clone(),
            },
            CompileDomain {
                id: "dylint-ui-tests",
                family: "dylint-nightly",
                toolchain: nightly.channel.clone(),
                compiler_release: Some(nightly.compiler_release.clone()),
                compiler_commit: Some(nightly.compiler_commit.clone()),
                target_triple: host.clone(),
                target_directory: dylint_tests.display().to_string(),
                profile: "test",
                rustflags: rustflags.clone(),
                cargo_config: config.clone(),
                wrapper_identity: wrapper.clone(),
            },
            CompileDomain {
                id: "rustdoc",
                family: "rustdoc",
                toolchain: stable_toolchain,
                compiler_release: None,
                compiler_commit: None,
                target_triple: host,
                target_directory: stable_target.display().to_string(),
                profile: "test",
                rustflags,
                cargo_config: config,
                wrapper_identity: wrapper,
            },
        ],
        stages,
        subsumed_steps: vec![SubsumedStep {
            name: "cargo check",
            subsumed_by: "clippy",
            reason: "clippy covers the canonical workspace/all-targets host scope",
        }],
        cook: CookDecision {
            action: "not-run",
            reason: "ci-test observes Cargo freshness and does not insert a dev-profile warm-up",
        },
        resource_limits: ResourceLimits {
            cargo_build_jobs,
            soldr_jobs,
            nextest_test_threads: Some(nextest_test_threads),
        },
        test_target_count,
        test_target_warn_threshold,
        dylint_target_trees: DylintTargetTrees {
            libraries: dylint_libraries.display().to_string(),
            analysis: dylint_analysis.display().to_string(),
            tests: dylint_tests.display().to_string(),
        },
        compiler_execution_groups: vec![
            group("stable-clippy", "stable", vec!["clippy".into()]),
            group("dylint-libraries", "dylint-libraries", library_names),
            group(
                "dylint-workspace",
                "dylint-analysis",
                vec!["dylint-workspace".into()],
            ),
            group("dylint-ui-tests", "dylint-ui-tests", dylint_test_names),
            group("nextest-compile", "stable", vec!["nextest-compile".into()]),
            group("doctests", "rustdoc", vec!["doctests".into()]),
        ],
        observability: super::model::Observability {
            freshness_authority: "cargo",
            zccache_counters: "reported by the Soldr child build logs where observable",
            stage_wall_time: "reported after each executed stage",
            stage_bytes: "reported where the child exposes byte counters",
        },
    })
}

#[derive(Clone, Copy)]
struct StageExecution {
    kind: &'static str,
    concurrency_group: Option<&'static str>,
    executes_compiler: bool,
}

const PREFLIGHT: StageExecution = StageExecution {
    kind: "non-compiling",
    concurrency_group: Some("preflight"),
    executes_compiler: false,
};
const COMPILER: StageExecution = StageExecution {
    kind: "compiler",
    concurrency_group: None,
    executes_compiler: true,
};
const COMPILER_AND_TEST: StageExecution = StageExecution {
    kind: "compiler-and-test",
    concurrency_group: None,
    executes_compiler: true,
};
const TEST: StageExecution = StageExecution {
    kind: "test",
    concurrency_group: None,
    executes_compiler: false,
};
const POLICY: StageExecution = StageExecution {
    kind: "policy",
    concurrency_group: Some("policy"),
    executes_compiler: false,
};

fn stage(
    name: &str,
    domain: &'static str,
    execution: StageExecution,
    command: Vec<String>,
    depends_on: &[&str],
    root: &Path,
) -> Stage {
    Stage {
        name: name.into(),
        domain,
        kind: execution.kind,
        command,
        working_directory: root.display().to_string(),
        depends_on: depends_on.iter().map(|value| (*value).into()).collect(),
        concurrency_group: execution.concurrency_group,
        executes_compiler: execution.executes_compiler,
        metrics: super::model::StageMetrics {
            wall_time_ms: None,
            bytes: None,
            zccache_counters: None,
        },
    }
}

fn group(id: &'static str, domain: &'static str, stages: Vec<String>) -> CompilerExecutionGroup {
    CompilerExecutionGroup {
        id,
        domain,
        stages,
        fresh_dirty: "Cargo-reported at execution time",
    }
}

fn cargo_command(fixed: &[impl AsRef<str>], scope: &[String]) -> Vec<String> {
    let mut command = vec!["soldr".into(), "cargo".into()];
    command.extend(fixed.iter().map(|value| value.as_ref().into()));
    command.extend(scope.iter().cloned());
    command
}

fn fmt_scope(scope: &super::model::Scope) -> Vec<String> {
    let mut args = Vec::new();
    for package in &scope.packages {
        args.extend(["--package".into(), package.clone()]);
    }
    args
}

fn workspace_selection(scope: &super::model::Scope) -> Vec<String> {
    if scope.packages.is_empty() {
        vec!["--workspace".into()]
    } else {
        Vec::new()
    }
}

fn cargo_target_root(root: &Path) -> Result<PathBuf, SoldrError> {
    if let Some(value) = std::env::var_os("CARGO_TARGET_DIR").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return Ok(if path.is_absolute() {
            path
        } else {
            root.join(path)
        });
    }
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = root.join(name);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: toml::Value = toml::from_str(&contents).map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: failed to parse {}: {error}",
                path.display()
            ))
        })?;
        if let Some(value) = parsed
            .get("build")
            .and_then(|build| build.get("target-dir"))
            .and_then(toml::Value::as_str)
        {
            let path = PathBuf::from(value);
            return Ok(if path.is_absolute() {
                path
            } else {
                root.join(path)
            });
        }
    }
    Ok(root.join("target"))
}

fn cargo_config_paths(root: &Path) -> Vec<String> {
    [".cargo/config.toml", ".cargo/config"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_file())
        .map(|path| path.display().to_string())
        .collect()
}

fn effective_rustflags() -> Option<String> {
    ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn wrapper_identity(cache_enabled: bool) -> String {
    if !cache_enabled {
        return "disabled (--no-cache)".into();
    }
    std::env::var("SOLDR_RUSTC_WRAPPER")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "soldr-managed-zccache".into())
}

/// soldr#2945: the per-lint `rust-toolchain.toml` loop this used to run —
/// keyed on the hard-coded `DYLINTS` name list — is now the shared, glob-aware
/// reader in `crate::dylint_libraries`, which reads
/// `workspace.metadata.dylint.libraries`, distinguishes no libraries from
/// all-inherit, and reports disagreeing pins itself.
/// `DYLINTS` survives only because the stage graph above still names one build
/// and one UI-test stage per lint. Only the env-override refusal below is
/// ci-test-specific.
async fn resolve_dylint_plan(
    root: &Path,
    host: &str,
) -> Result<crate::dylint_toolchain::DylintToolchainPlan, SoldrError> {
    let (channel, libraries) = match crate::dylint_libraries::toolchain_state(root)? {
        crate::dylint_libraries::LibraryToolchainState::NoLibraries => {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: {} declares no Dylint libraries under \
                 workspace.metadata.dylint.libraries",
                root.join("Cargo.toml").display()
            )));
        }
        crate::dylint_libraries::LibraryToolchainState::InheritRoot { libraries } => {
            let channel = crate::dylint_libraries::inherited_root_channel(root, &libraries)?;
            (channel, libraries)
        }
        crate::dylint_libraries::LibraryToolchainState::Pinned { channel, libraries } => {
            (channel, libraries)
        }
    };
    let library_count = libraries.len();
    // ci-test is deliberately stricter than the `soldr dylint` front door.
    // There, an explicit `+toolchain` or SOLDR_DYLINT_TOOLCHAIN is a developer
    // poking at one lint and is honoured. Here the plan is a frozen,
    // reproducible DAG whose three Dylint compile domains are keyed on the one
    // nightly the lint libraries themselves declare, so an override naming a
    // different nightly would silently key the target trees to a compiler the
    // libraries were not built for. That is a plan-integrity failure, not a
    // preference — so it is refused before the plan is frozen.
    for key in [
        crate::dylint_toolchain::TOOLCHAIN_ENV_VAR,
        crate::dylint_toolchain::CONFIGURED_TOOLCHAIN_ENV_VAR,
    ] {
        let Some(override_channel) = non_empty_env(key) else {
            continue;
        };
        if canonical_channel(&override_channel, host) != canonical_channel(&channel, host) {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: configured Dylint toolchain {override_channel} ({key}) conflicts with the {library_count} lint libraries resolved to {channel}"
            )));
        }
    }
    let plan = crate::dylint_toolchain::resolve_plan(Some(&channel), root).await?;
    if canonical_channel(&plan.channel, host) != canonical_channel(&channel, host) {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: configured Dylint toolchain {} conflicts with the {library_count} lint libraries resolved to {channel}",
            plan.channel
        )));
    }
    Ok(plan)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn inherited_limit(name: &str) -> Option<String> {
    // Keep even an empty/invalid explicit value. Cargo/the daemon owns
    // validation; treating it as absent would silently change caller intent.
    std::env::var(name).ok()
}

fn effective_limit(name: &str, default: &str) -> String {
    non_empty_env(name).unwrap_or_else(|| default.into())
}

fn workspace_metadata_fingerprint(root: &Path, config: &[String]) -> Result<String, SoldrError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"soldr-ci-test-workspace-metadata-v1\0");
    for path in std::iter::once(root.join("Cargo.toml"))
        .chain(std::iter::once(root.join("Cargo.lock")))
        .chain(config.iter().map(PathBuf::from))
    {
        hasher.update(path.display().to_string().as_bytes());
        hasher.update(&[0]);
        match std::fs::read(&path) {
            Ok(contents) => {
                hasher.update(&contents);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update(b"<missing>");
            }
            Err(error) => {
                return Err(SoldrError::Other(format!(
                    "soldr ci-test: cannot read workspace metadata {}: {error}",
                    path.display()
                )));
            }
        }
        hasher.update(&[0]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn canonical_channel(channel: &str, host: &str) -> String {
    if channel.ends_with(host) {
        channel.into()
    } else {
        format!("{channel}-{host}")
    }
}

fn reject_non_host_target(root: &Path) -> Result<(), SoldrError> {
    if let Some(value) = std::env::var_os("CARGO_BUILD_TARGET").filter(|value| !value.is_empty()) {
        return Err(SoldrError::Other(format!(
            "soldr ci-test: CARGO_BUILD_TARGET={} creates a non-host compile domain; unset it or use `soldr cargo ...` explicitly",
            PathBuf::from(value).display()
        )));
    }
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = root.join(name);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: toml::Value = toml::from_str(&contents).map_err(|error| {
            SoldrError::Other(format!(
                "soldr ci-test: failed to parse {}: {error}",
                path.display()
            ))
        })?;
        if let Some(target) = parsed
            .get("build")
            .and_then(|build| build.get("target"))
            .and_then(toml::Value::as_str)
        {
            return Err(SoldrError::Other(format!(
                "soldr ci-test: {} sets build.target={target:?}, which creates a non-host compile domain; use `soldr cargo ...` explicitly",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_channel_adds_the_host_once() {
        assert_eq!(
            canonical_channel("nightly-2026-05-28", "x86_64-unknown-linux-gnu"),
            "nightly-2026-05-28-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            canonical_channel(
                "nightly-2026-05-28-x86_64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu"
            ),
            "nightly-2026-05-28-x86_64-unknown-linux-gnu"
        );
    }
}
