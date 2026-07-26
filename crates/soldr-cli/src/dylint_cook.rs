//! Exact-nightly, check-shaped dependency preparation for Dylint.

use crate::cargo_front_door::{self, DYLINT_DEPENDENCY_COOK_FLAG};
use crate::core::{read_rust_toolchain_manifest, SoldrError};
use crate::dylint_toolchain::DylintToolchainPlan;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const MARKER_NAME: &str = ".soldr-dylint-cook-v1.json";
const LOCK_NAME: &str = ".soldr-dylint-cook.lock";
const WRAPPER_IDENTITY: &str = "soldr-dylint-dependency-cook-v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct DylintCookArgs {
    #[serde(skip)]
    json: bool,
    #[serde(skip)]
    plan_only: bool,
    release: bool,
    profile: Option<String>,
    target: Option<String>,
    workspace: bool,
    packages: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    all_targets: bool,
    tests: bool,
    benches: bool,
    examples: bool,
    locked: bool,
    frozen: bool,
    offline: bool,
    cargo_config: Vec<String>,
    toolchain: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CompilerPlan {
    channel: String,
    release: String,
    commit_hash: String,
    verified: bool,
}

#[derive(Debug, Clone, Serialize)]
struct BuildShape {
    operation: &'static str,
    profile: String,
    target: Option<String>,
    workspace: bool,
    packages: Vec<String>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
    all_targets: bool,
    tests: bool,
    benches: bool,
    examples: bool,
    locked: bool,
    frozen: bool,
    offline: bool,
    cargo_config: Vec<String>,
    wrapper_identity: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct DylintCookOutput {
    schema_version: u32,
    command: &'static str,
    compiler: CompilerPlan,
    target_directory: String,
    build_shape: BuildShape,
    cache_key: String,
    outcome: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DylintCookMarker {
    schema_version: u32,
    cache_key: String,
    compiler_commit: String,
    target_directory: String,
}

fn parse_args(args: &[String]) -> Result<DylintCookArgs, SoldrError> {
    let mut parsed = DylintCookArgs::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let next = |flag: &str, index: &mut usize| -> Result<String, SoldrError> {
            *index += 1;
            args.get(*index).cloned().ok_or_else(|| {
                SoldrError::Other(format!("soldr dylint cook: {flag} requires a value"))
            })
        };
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--plan-only" => parsed.plan_only = true,
            "--release" => parsed.release = true,
            "--workspace" | "--all" => parsed.workspace = true,
            "--all-features" => parsed.all_features = true,
            "--no-default-features" => parsed.no_default_features = true,
            "--all-targets" => parsed.all_targets = true,
            "--tests" => parsed.tests = true,
            "--benches" => parsed.benches = true,
            "--examples" => parsed.examples = true,
            "--locked" => parsed.locked = true,
            "--frozen" => parsed.frozen = true,
            "--offline" => parsed.offline = true,
            "--target" => parsed.target = Some(next("--target", &mut index)?),
            "--profile" => parsed.profile = Some(next("--profile", &mut index)?),
            "-p" | "--package" => parsed.packages.push(next("--package", &mut index)?),
            "--features" => extend_features(&mut parsed.features, &next("--features", &mut index)?),
            "--config" => parsed.cargo_config.push(next("--config", &mut index)?),
            "--toolchain" => parsed.toolchain = Some(next("--toolchain", &mut index)?),
            value if value.starts_with("--target=") => parsed.target = Some(value[9..].to_string()),
            value if value.starts_with("--profile=") => {
                parsed.profile = Some(value[10..].to_string())
            }
            value if value.starts_with("--package=") => {
                parsed.packages.push(value[10..].to_string())
            }
            value if value.starts_with("--features=") => {
                extend_features(&mut parsed.features, &value[11..])
            }
            value if value.starts_with("--config=") => {
                parsed.cargo_config.push(value[9..].to_string())
            }
            value if value.starts_with("--toolchain=") => {
                parsed.toolchain = Some(value[12..].to_string())
            }
            value if value.starts_with("+nightly-") => {
                if parsed.toolchain.replace(value[1..].to_string()).is_some() {
                    return Err(SoldrError::Other(
                        "soldr dylint cook: multiple toolchain selectors".into(),
                    ));
                }
            }
            other => {
                return Err(SoldrError::Other(format!(
                    "soldr dylint cook: unsupported option `{other}`"
                )))
            }
        }
        index += 1;
    }
    if parsed.release && parsed.profile.is_some() {
        return Err(SoldrError::Other(
            "soldr dylint cook: --release conflicts with --profile".into(),
        ));
    }
    if parsed.all_features && parsed.no_default_features {
        return Err(SoldrError::Other(
            "soldr dylint cook: --all-features conflicts with --no-default-features".into(),
        ));
    }
    parsed.features.sort();
    parsed.features.dedup();
    parsed.packages.sort();
    parsed.packages.dedup();
    Ok(parsed)
}

fn extend_features(features: &mut Vec<String>, value: &str) {
    features.extend(
        value
            .split([',', ' '])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
}

pub(crate) async fn run(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_help();
        return Ok(0);
    }
    let parsed = parse_args(args)?;
    let cwd = std::env::current_dir()?;
    let root = crate::cook::resolve_manifest_dir(&cwd)?;
    if !root.join(concat!("Car", "go.lock")).is_file() {
        return Err(SoldrError::Other(
            "soldr dylint cook: a lockfile is required for a restorable dependency layer".into(),
        ));
    }
    // Acquire before reading any manifest/toolchain/hash inputs: another cook
    // may be inside cargo-chef's in-place skeleton reconstruction even before
    // this invocation reaches its own snapshot.
    let source_lock = lock_workspace_source(&root)?;
    let configured = configured_library_toolchain(&root)?;
    let requested = reconcile_toolchain(parsed.toolchain.as_deref(), configured.as_deref())?;

    if parsed.plan_only {
        let plan = crate::dylint_toolchain::resolve_plan(requested, &root).await?;
        let verified = crate::dylint_toolchain::verify_if_installed(&plan)?;
        let output = build_output(&root, &parsed, &plan, verified)?;
        emit_output(&output, parsed.json)?;
        return Ok(0);
    }

    let plan = crate::dylint_toolchain::prepare(requested, &root).await?;
    crate::dylint_toolchain::verify_observed_identity(&plan)?;
    let mut output = build_output(&root, &parsed, &plan, true)?;
    let target_dir = PathBuf::from(&output.target_directory);
    std::fs::create_dir_all(&target_dir)?;
    let lock = lock_target(&target_dir)?;
    let expected = marker_for_output(&output);
    let marker_path = target_dir.join(MARKER_NAME);
    if read_marker(&marker_path).as_ref() == Some(&expected)
        && target_has_dependency_payload(&target_dir)
    {
        output.outcome = "skip";
        emit_output(&output, parsed.json)?;
        FileExt::unlock(&lock)?;
        FileExt::unlock(&source_lock)?;
        return Ok(0);
    }

    let recipe_dir = tempfile::tempdir()?;
    let recipe_path = recipe_dir.path().join("recipe.json");
    let source_snapshot = crate::cook::snapshot_project_source(&root)?;
    let result =
        run_check_shaped_cook(&recipe_path, &target_dir, &parsed, &plan, cache_enabled).await;
    crate::cook::restore_project_source(&root, &source_snapshot).map_err(|error| {
        SoldrError::Other(format!(
            "soldr dylint cook: failed to restore workspace sources: {error}"
        ))
    })?;
    let code = result?;
    if code != 0 {
        return Ok(code);
    }
    remove_workspace_dummy_artifacts(&recipe_path, &target_dir, &parsed, &plan).await?;
    write_marker(&marker_path, &expected)?;
    output.outcome = "miss";
    emit_output(&output, parsed.json)?;
    FileExt::unlock(&lock)?;
    FileExt::unlock(&source_lock)?;
    Ok(0)
}

fn print_help() {
    println!(
        "Prepare Dylint dependencies with an exact nightly check-shaped pass.\n\n\
Usage: soldr dylint cook [OPTIONS]\n\n\
Options:\n  --plan-only --json\n  --toolchain <NIGHTLY>\n  --target <TRIPLE>\n  \
--release | --profile <NAME>\n  --workspace | --package <NAME>\n  \
--features <LIST> | --all-features | --no-default-features\n  \
--all-targets --tests --benches --examples\n  --config <KEY=VALUE>\n  \
--locked --frozen --offline"
    );
}

fn reconcile_toolchain<'a>(
    explicit: Option<&'a str>,
    configured: Option<&'a str>,
) -> Result<Option<&'a str>, SoldrError> {
    if let (Some(explicit), Some(configured)) = (explicit, configured) {
        if canonical_channel(explicit) != canonical_channel(configured) {
            return Err(SoldrError::Other(format!(
                "soldr dylint cook: explicit `{explicit}` conflicts with custom-lint requirement `{configured}`"
            )));
        }
    }
    Ok(explicit.or(configured))
}

fn canonical_channel(channel: &str) -> &str {
    channel
        .get(..18)
        .filter(|prefix| prefix.starts_with("nightly-"))
        .unwrap_or(channel)
}

async fn run_check_shaped_cook(
    recipe_path: &Path,
    target_dir: &Path,
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
    cache_enabled: bool,
) -> Result<i32, SoldrError> {
    let prepare = vec![
        format!("+{}", plan.channel),
        "chef".into(),
        "prepare".into(),
        "--recipe-path".into(),
        recipe_path.display().to_string(),
    ];
    let code = cargo_front_door::run_cargo_front_door(&prepare, cache_enabled, false).await?;
    if code != 0 {
        return Ok(code);
    }
    let reconstruct = vec![
        format!("+{}", plan.channel),
        "chef".into(),
        "cook".into(),
        "--recipe-path".into(),
        recipe_path.display().to_string(),
        "--no-build".into(),
    ];
    let code = cargo_front_door::run_cargo_front_door(&reconstruct, cache_enabled, false).await?;
    if code != 0 {
        return Ok(code);
    }
    cargo_front_door::run_cargo_front_door(
        &build_check_args(args, plan, target_dir),
        cache_enabled,
        false,
    )
    .await
}

fn build_check_args(
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
    target_dir: &Path,
) -> Vec<String> {
    let mut result = vec![
        format!("+{}", plan.channel),
        DYLINT_DEPENDENCY_COOK_FLAG.into(),
    ];
    for config in &args.cargo_config {
        result.extend(["--config".into(), config.clone()]);
    }
    result.push("check".into());
    result.extend(["--target-dir".into(), target_dir.display().to_string()]);
    if args.release {
        result.push("--release".into());
    }
    if let Some(profile) = &args.profile {
        result.extend(["--profile".into(), profile.clone()]);
    }
    if let Some(target) = &args.target {
        result.extend(["--target".into(), target.clone()]);
    }
    if args.workspace {
        result.push("--workspace".into());
    }
    for package in &args.packages {
        result.extend(["--package".into(), package.clone()]);
    }
    if !args.features.is_empty() {
        result.extend(["--features".into(), args.features.join(",")]);
    }
    for (enabled, flag) in [
        (args.all_features, "--all-features"),
        (args.no_default_features, "--no-default-features"),
        (args.all_targets, "--all-targets"),
        (args.tests, "--tests"),
        (args.benches, "--benches"),
        (args.examples, "--examples"),
        (args.locked, "--locked"),
        (args.frozen, "--frozen"),
        (args.offline, "--offline"),
    ] {
        if enabled {
            result.push(flag.into());
        }
    }
    result
}

async fn remove_workspace_dummy_artifacts(
    recipe_path: &Path,
    target_dir: &Path,
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    let packages = workspace_package_names(recipe_path)?;
    if packages.is_empty() {
        return Ok(());
    }
    let mut clean = vec![
        format!("+{}", plan.channel),
        DYLINT_DEPENDENCY_COOK_FLAG.into(),
    ];
    for config in &args.cargo_config {
        clean.extend(["--config".into(), config.clone()]);
    }
    clean.extend([
        "clean".into(),
        "--target-dir".into(),
        target_dir.display().to_string(),
    ]);
    for package in packages {
        clean.extend(["--package".into(), package]);
    }
    let code = cargo_front_door::run_cargo_front_door(&clean, false, false).await?;
    if code != 0 {
        return Err(SoldrError::Other(format!(
            "soldr dylint cook: dummy artifact cleanup failed with exit {code}"
        )));
    }
    Ok(())
}

fn workspace_package_names(recipe_path: &Path) -> Result<Vec<String>, SoldrError> {
    let mut names = Vec::new();
    let recipe: serde_json::Value = serde_json::from_slice(&std::fs::read(recipe_path)?)
        .map_err(|error| SoldrError::Other(format!("recipe parsing failed: {error}")))?;
    let manifests = recipe
        .get("skeleton")
        .and_then(|value| value.get("manifests"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| SoldrError::Other("recipe contains no manifests".into()))?;
    for manifest in manifests {
        let Some(contents) = manifest.get("contents").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(value) = toml::from_str::<toml::Value>(contents) else {
            continue;
        };
        if let Some(name) = value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn build_output(
    root: &Path,
    args: &DylintCookArgs,
    plan: &DylintToolchainPlan,
    verified: bool,
) -> Result<DylintCookOutput, SoldrError> {
    let target_directory = cargo_target_root(root)?
        .join("dylint")
        .join("target")
        .join(canonical_channel(&plan.channel));
    let mut digest = Sha256::new();
    digest.update(b"soldr-dylint-cook-v1\0");
    digest.update(plan.channel.as_bytes());
    digest.update([0]);
    digest.update(plan.compiler_release.as_bytes());
    digest.update([0]);
    digest.update(plan.compiler_commit.as_bytes());
    digest.update([0]);
    digest.update(semantic_input_hash(root, args)?.as_bytes());
    let cache_key = hex::encode(digest.finalize());
    let marker = DylintCookMarker {
        schema_version: SCHEMA_VERSION,
        cache_key: cache_key.clone(),
        compiler_commit: plan.compiler_commit.clone(),
        target_directory: target_directory.display().to_string(),
    };
    let outcome = if verified
        && read_marker(&target_directory.join(MARKER_NAME)).as_ref() == Some(&marker)
        && target_has_dependency_payload(&target_directory)
    {
        "hit"
    } else {
        "miss"
    };
    Ok(DylintCookOutput {
        schema_version: SCHEMA_VERSION,
        command: "dylint-cook",
        compiler: CompilerPlan {
            channel: plan.channel.clone(),
            release: plan.compiler_release.clone(),
            commit_hash: plan.compiler_commit.clone(),
            verified,
        },
        target_directory: target_directory.display().to_string(),
        build_shape: BuildShape {
            operation: "check",
            profile: args
                .profile
                .clone()
                .unwrap_or_else(|| if args.release { "release" } else { "dev" }.into()),
            target: args.target.clone(),
            workspace: args.workspace,
            packages: args.packages.clone(),
            features: args.features.clone(),
            all_features: args.all_features,
            no_default_features: args.no_default_features,
            all_targets: args.all_targets,
            tests: args.tests,
            benches: args.benches,
            examples: args.examples,
            locked: args.locked,
            frozen: args.frozen,
            offline: args.offline,
            cargo_config: args.cargo_config.clone(),
            wrapper_identity: WRAPPER_IDENTITY,
        },
        cache_key,
        outcome,
    })
}

fn marker_for_output(output: &DylintCookOutput) -> DylintCookMarker {
    DylintCookMarker {
        schema_version: SCHEMA_VERSION,
        cache_key: output.cache_key.clone(),
        compiler_commit: output.compiler.commit_hash.clone(),
        target_directory: output.target_directory.clone(),
    }
}

fn emit_output(output: &DylintCookOutput, json: bool) -> Result<(), SoldrError> {
    if json {
        println!(
            "{}",
            serde_json::to_string(output).map_err(|error| {
                SoldrError::Other(format!("failed to encode Dylint cook output: {error}"))
            })?
        );
    } else {
        eprintln!(
            "soldr dylint cook: {} {} -> {} [{}]",
            output.compiler.channel,
            output.compiler.commit_hash,
            output.target_directory,
            output.outcome
        );
    }
    Ok(())
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
            SoldrError::Other(format!("failed to parse {}: {error}", path.display()))
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

fn semantic_input_hash(root: &Path, args: &DylintCookArgs) -> Result<String, SoldrError> {
    let mut entries = BTreeMap::<String, Vec<u8>>::new();
    visit_semantic_files(root, &mut |path, bytes| {
        if let Ok(relative) = path.strip_prefix(root) {
            entries.insert(normalize_path(relative), bytes.to_vec());
        }
    })?;
    let mut environment = BTreeMap::new();
    for (key, value) in std::env::vars() {
        if matches!(
            key.as_str(),
            "RUSTFLAGS" | "CARGO_ENCODED_RUSTFLAGS" | "CARGO_BUILD_TARGET" | "SOLDR_RUSTC_WRAPPER"
        ) || key.starts_with("CARGO_PROFILE_")
            || key.starts_with("CARGO_TARGET_")
        {
            environment.insert(key, value);
        }
    }
    let mut digest = Sha256::new();
    for (path, bytes) in entries {
        hash_field(&mut digest, path.as_bytes());
        hash_field(&mut digest, &bytes);
    }
    hash_field(
        &mut digest,
        &serde_json::to_vec(args)
            .map_err(|error| SoldrError::Other(format!("shape encoding failed: {error}")))?,
    );
    hash_field(
        &mut digest,
        &serde_json::to_vec(&environment)
            .map_err(|error| SoldrError::Other(format!("environment encoding failed: {error}")))?,
    );
    hash_field(&mut digest, WRAPPER_IDENTITY.as_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn visit_semantic_files(
    root: &Path,
    callback: &mut dyn FnMut(&Path, &[u8]),
) -> Result<(), SoldrError> {
    fn visit(directory: &Path, callback: &mut dyn FnMut(&Path, &[u8])) -> Result<(), SoldrError> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".claude" | "target" | "node_modules")
                ) {
                    continue;
                }
                visit(&path, callback)?;
            } else if kind.is_file() {
                let name = path.file_name().and_then(|value| value.to_str());
                if matches!(name, Some("config.toml" | "config"))
                    || name == Some(concat!("Car", "go.toml"))
                    || name == Some(concat!("Car", "go.lock"))
                {
                    let bytes = std::fs::read(&path)?;
                    callback(&path, &bytes);
                }
            }
        }
        Ok(())
    }
    visit(root, callback)
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn configured_library_toolchain(root: &Path) -> Result<Option<String>, SoldrError> {
    let manifest_path = root.join(concat!("Car", "go.toml"));
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(&manifest_path)?).map_err(|error| {
            SoldrError::Other(format!(
                "failed to parse {}: {error}",
                manifest_path.display()
            ))
        })?;
    let libraries = manifest
        .get("workspace")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("dylint"))
        .and_then(|value| value.get("libraries"))
        .and_then(toml::Value::as_array);
    let mut requirements = BTreeMap::<String, Vec<String>>::new();
    if let Some(libraries) = libraries {
        for library in libraries {
            let Some(relative) = library.get("path").and_then(toml::Value::as_str) else {
                continue;
            };
            if let Some(channel) = read_rust_toolchain_manifest(&root.join(relative))?.channel {
                requirements
                    .entry(canonical_channel(&channel).to_string())
                    .or_default()
                    .push(format!("{relative} ({channel})"));
            }
        }
    }
    if requirements.len() > 1 {
        let details = requirements
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(SoldrError::Other(format!(
            "soldr dylint cook: conflicting custom-lint nightly requirements: {details}"
        )));
    }
    if let Some(channel) = requirements.keys().next() {
        return Ok(Some(channel.clone()));
    }
    Ok(read_rust_toolchain_manifest(root)?.channel)
}

fn lock_target(target: &Path) -> Result<File, SoldrError> {
    let path = target.join(LOCK_NAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    file.lock_exclusive().map_err(|error| {
        SoldrError::Other(format!("failed to lock {}: {error}", path.display()))
    })?;
    Ok(file)
}

fn lock_workspace_source(root: &Path) -> Result<File, SoldrError> {
    let path = workspace_source_lock_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    file.lock_exclusive().map_err(|error| {
        SoldrError::Other(format!(
            "failed to lock workspace sources via {}: {error}",
            path.display()
        ))
    })?;
    Ok(file)
}

fn workspace_source_lock_path(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return dot_git.join("soldr-dylint-cook.lock");
    }
    if dot_git.is_file() {
        if let Ok(contents) = std::fs::read_to_string(&dot_git) {
            if let Some(git_dir) = contents.trim().strip_prefix("gitdir:") {
                let git_dir = PathBuf::from(git_dir.trim());
                let git_dir = if git_dir.is_absolute() {
                    git_dir
                } else {
                    root.join(git_dir)
                };
                return git_dir.join("soldr-dylint-cook.lock");
            }
        }
    }
    // Archive/non-git workspaces still need cross-nightly serialization.
    // Keep a stable advisory lock at the workspace boundary; it contains no
    // state and may be safely ignored by source archives.
    root.join(".soldr-dylint-cook.lock")
}

fn read_marker(path: &Path) -> Option<DylintCookMarker> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_marker(path: &Path, marker: &DylintCookMarker) -> Result<(), SoldrError> {
    let bytes = serde_json::to_vec(marker)
        .map_err(|error| SoldrError::Other(format!("marker encoding failed: {error}")))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    replace_marker_file(&temporary, path, |from, to| std::fs::rename(from, to))?;
    Ok(())
}

fn replace_marker_file(
    temporary: &Path,
    path: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Err(error) = rename(temporary, path) {
        // Windows rename does not replace an existing destination. The
        // workspace + target locks make this remove-then-rename fallback
        // safe; interruption can only omit the marker and force a recook.
        if path.exists() {
            std::fs::remove_file(path)?;
            rename(temporary, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

fn target_has_dependency_payload(target: &Path) -> bool {
    fn contains_file(directory: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && contains_file(&path) {
                return true;
            }
            if path.is_file()
                && !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(MARKER_NAME | LOCK_NAME)
                )
            {
                return true;
            }
        }
        false
    }
    contains_file(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    crate::timed_test!(parses_check_shaped_dylint_cook_scope, {
        let parsed = parse_args(&argv(&[
            "--json",
            "--plan-only",
            "--release",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--workspace",
            "-p",
            "demo",
            "--features",
            "serde,trace",
            "--all-targets",
            "--config",
            "build.jobs=2",
            "--toolchain",
            "nightly-2026-04-16",
        ]))
        .expect("parse");
        assert!(parsed.json && parsed.plan_only && parsed.release);
        assert_eq!(parsed.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
        assert!(parsed.workspace && parsed.all_targets);
        assert_eq!(parsed.packages, ["demo"]);
        assert_eq!(parsed.features, ["serde", "trace"]);
        assert_eq!(parsed.cargo_config, ["build.jobs=2"]);
    });

    crate::timed_test!(semantic_key_ignores_source_but_tracks_manifests, {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join(concat!("Car", "go.toml")),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(root.join(concat!("Car", "go.lock")), "version = 3\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        let args = DylintCookArgs::default();
        let first = semantic_input_hash(root, &args).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn two() {}\n").unwrap();
        assert_eq!(first, semantic_input_hash(root, &args).unwrap());
        std::fs::write(root.join(concat!("Car", "go.lock")), "version = 4\n").unwrap();
        assert_ne!(first, semantic_input_hash(root, &args).unwrap());
    });

    crate::timed_test!(conflicting_custom_lint_toolchains_are_actionable, {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["lint-a", "lint-b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(
            root.join(concat!("Car", "go.toml")),
            "[workspace]\nmembers=[]\n[workspace.metadata.dylint]\n\
             libraries=[{path='lint-a'},{path='lint-b'}]\n",
        )
        .unwrap();
        for (name, date) in [("lint-a", "16"), ("lint-b", "17")] {
            std::fs::write(
                root.join(name).join("rust-toolchain.toml"),
                format!("[toolchain]\nchannel='nightly-2026-04-{date}'\n"),
            )
            .unwrap();
        }
        let error = configured_library_toolchain(root).unwrap_err().to_string();
        assert!(error.contains("conflicting"));
        assert!(error.contains("lint-a") && error.contains("lint-b"));
    });

    crate::timed_test!(warm_marker_requires_real_dependency_payload, {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(MARKER_NAME), "{}").unwrap();
        assert!(!target_has_dependency_payload(temp.path()));
        std::fs::create_dir_all(temp.path().join("debug/deps")).unwrap();
        std::fs::write(temp.path().join("debug/deps/libserde-deadbeef.rmeta"), "x").unwrap();
        assert!(target_has_dependency_payload(temp.path()));
    });

    crate::timed_test!(marker_replacement_handles_windows_existing_destination, {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(MARKER_NAME);
        let temporary = temp.path().join("replacement.tmp");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(&temporary, b"new").unwrap();
        let mut attempts = 0;

        replace_marker_file(&temporary, &path, |from, to| {
            attempts += 1;
            if attempts == 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated Windows destination-exists failure",
                ))
            } else {
                std::fs::rename(from, to)
            }
        })
        .unwrap();

        assert_eq!(attempts, 2);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!temporary.exists());
    });

    crate::timed_test!(
        workspace_source_lock_is_shared_across_target_and_nightly_shapes,
        {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::write(root.join(concat!("Car", "go.lock")), "version = 3\n").unwrap();
            let _nightly_a_target = root.join("target/dylint/target/nightly-2026-01-17");
            let _nightly_b_target = root.join("other-target/dylint/target/nightly-2026-01-18");
            let first = lock_workspace_source(root).unwrap();
            let contender = OpenOptions::new()
                .read(true)
                .write(true)
                .open(workspace_source_lock_path(root))
                .unwrap();

            assert!(
                contender.try_lock_exclusive().is_err(),
                "different nightly/target shapes must contend on one workspace source lock"
            );
            FileExt::unlock(&first).unwrap();
            contender
                .try_lock_exclusive()
                .expect("workspace lock must become available after release");
            FileExt::unlock(&contender).unwrap();
        }
    );

    crate::timed_test!(check_shape_preserves_scope_and_private_marker, {
        let parsed = parse_args(&argv(&[
            "--profile=ci",
            "--target=aarch64-unknown-linux-gnu",
            "--workspace",
            "--package=demo",
            "--features=serde,trace",
            "--tests",
            "--locked",
            "--config=build.jobs=2",
        ]))
        .unwrap();
        let plan = DylintToolchainPlan {
            channel: "nightly-2099-01-02".into(),
            compiler_release: "1.99.0-nightly".into(),
            compiler_commit: "0123456789abcdef".into(),
        };
        let built = build_check_args(&parsed, &plan, Path::new("isolated-target"));
        assert_eq!(built[0], "+nightly-2099-01-02");
        assert_eq!(built[1], DYLINT_DEPENDENCY_COOK_FLAG);
        assert!(built.windows(2).any(|pair| pair == ["--profile", "ci"]));
        assert!(built
            .windows(2)
            .any(|pair| pair == ["--target-dir", "isolated-target"]));
        assert!(built.contains(&"--workspace".to_string()));
        assert!(built.contains(&"--tests".to_string()));
        assert!(built.contains(&"--locked".to_string()));
    });

    crate::timed_test!(rejects_ambiguous_feature_scope, {
        let error = parse_args(&argv(&["--all-features", "--no-default-features"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicts"));
    });
}
