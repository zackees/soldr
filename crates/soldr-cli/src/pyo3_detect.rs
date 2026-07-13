//! Target-aware PyO3 build planning (soldr#1610 / #1614).
//!
//! Cargo metadata is the source of truth for the active PyO3 version,
//! features, and workspace targets. Policy resolution is pure and separate
//! from process inspection so every soldr entry point can consume the same
//! plan and the complete decision table stays unit-testable.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const COMPATIBILITY_ENV_VAR: &str = "SOLDR_PYO3_COMPATIBILITY";
pub const BUILD_KIND_ENV_VAR: &str = "SOLDR_PYO3_BUILD_KIND";

const CALLER_CONFIG_VARS: &[&str] = &[
    "PYO3_CONFIG_FILE",
    "PYO3_CROSS",
    "PYO3_CROSS_LIB_DIR",
    "PYO3_CROSS_PYTHON_IMPLEMENTATION",
    "PYO3_CROSS_PYTHON_VERSION",
    "PYO3_NO_PYTHON",
    "PYO3_PYTHON",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildShape {
    Absent,
    Extension,
    Embedding,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanMode {
    Native,
    NoPyo3,
    Abi3NoPython,
    ModernWindowsRawDylib,
    ExtensionDefault,
    RequiresExplicitCompatibility,
    CompatibilitySysroot,
    CallerConfigured,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedPyo3 {
    pub shape: BuildShape,
    pub versions: BTreeSet<String>,
    pub features: BTreeSet<String>,
}

impl DetectedPyo3 {
    pub fn abi3(&self) -> bool {
        self.features.iter().any(|feature| {
            feature == "abi3"
                || feature == "abi3t"
                || feature.starts_with("abi3-py")
                || feature.starts_with("abi3t-py")
        })
    }

    fn single_version(&self) -> Option<&str> {
        (self.versions.len() == 1)
            .then(|| self.versions.iter().next().map(String::as_str))
            .flatten()
    }
}

#[derive(Debug, Clone)]
pub struct PolicyInput {
    pub host: String,
    pub target: String,
    pub detected: Option<DetectedPyo3>,
    pub caller_pyo3: BTreeMap<String, String>,
    pub compatibility_sysroot: bool,
    pub raw_dylib_disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pyo3BuildPlan {
    pub host: String,
    pub target: String,
    pub shape: BuildShape,
    pub pyo3_versions: Vec<String>,
    pub pyo3_features: Vec<String>,
    pub mode: PlanMode,
    pub env: BTreeMap<String, String>,
    pub needs_python_sysroot: bool,
    pub diagnostic: Option<String>,
}

impl Pyo3BuildPlan {
    pub fn apply_to_command(&self, command: &mut Command) {
        for (key, value) in &self.env {
            if std::env::var_os(key).is_none() {
                command.env(key, value);
            }
        }
    }

    pub fn emit_diagnostic(&self) {
        if let Some(message) = &self.diagnostic {
            eprintln!("soldr: PyO3 build plan: {message}");
        }
    }

    pub async fn materialize_compatibility(
        &mut self,
        paths: &crate::core::SoldrPaths,
    ) -> Result<(), crate::core::SoldrError> {
        if !self.needs_python_sysroot {
            return Ok(());
        }
        let sysroot =
            crate::fetch::python_sysroot::ensure_python_sysroot(paths, &self.target).await?;
        self.env
            .extend(compatibility_sysroot_env(&sysroot.root, &sysroot.version));
        Ok(())
    }
}

fn compatibility_sysroot_env(root: &Path, version: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let abi_version = version.split('.').take(2).collect::<Vec<_>>().join(".");
    env.insert("PYO3_CROSS".into(), "1".into());
    env.insert(
        "PYO3_CROSS_LIB_DIR".into(),
        root.join("lib").display().to_string(),
    );
    env.insert("PYO3_CROSS_PYTHON_VERSION".into(), abi_version);
    env.insert("PYO3_CROSS_PYTHON_IMPLEMENTATION".into(), "CPython".into());
    env
}

pub fn resolve_policy(input: PolicyInput) -> Pyo3BuildPlan {
    let detected = input.detected.as_ref();
    let shape = detected.map_or(BuildShape::Absent, |value| value.shape);
    let versions = detected
        .map(|value| value.versions.iter().cloned().collect())
        .unwrap_or_default();
    let features = detected
        .map(|value| value.features.iter().cloned().collect())
        .unwrap_or_default();
    let mut plan = Pyo3BuildPlan {
        host: input.host.clone(),
        target: input.target.clone(),
        shape,
        pyo3_versions: versions,
        pyo3_features: features,
        mode: PlanMode::Unresolved,
        env: BTreeMap::new(),
        needs_python_sysroot: false,
        diagnostic: None,
    };

    if input.host == input.target {
        plan.mode = PlanMode::Native;
        return plan;
    }
    if detected.is_none() || shape == BuildShape::Absent {
        plan.mode = PlanMode::NoPyo3;
        return plan;
    }
    if input
        .caller_pyo3
        .keys()
        .any(|key| CALLER_CONFIG_VARS.contains(&key.as_str()))
    {
        plan.mode = PlanMode::CallerConfigured;
        return plan;
    }

    let detected = detected.expect("checked above");
    if shape == BuildShape::Extension && detected.abi3() {
        plan.mode = PlanMode::Abi3NoPython;
        plan.env
            .insert("PYO3_NO_PYTHON".to_string(), "1".to_string());
        return plan;
    }

    if shape == BuildShape::Extension
        && input.target.contains("windows")
        && detected
            .single_version()
            .is_some_and(|version| version_at_least(version, 0, 29))
        && !input.raw_dylib_disabled
    {
        plan.mode = PlanMode::ModernWindowsRawDylib;
        return plan;
    }

    if input.compatibility_sysroot {
        plan.mode = PlanMode::CompatibilitySysroot;
        plan.needs_python_sysroot = true;
        return plan;
    }

    if shape == BuildShape::Extension
        && (input.target.contains("linux") || input.target.contains("apple-darwin"))
        && detected
            .single_version()
            .is_some_and(|version| version_at_least(version, 0, 29))
    {
        plan.mode = PlanMode::ExtensionDefault;
        plan.diagnostic = Some(
            "cross extension is not proven ABI3; leaving Python configuration to PyO3/maturin"
                .to_string(),
        );
        return plan;
    }

    plan.mode = PlanMode::RequiresExplicitCompatibility;
    plan.diagnostic = Some(format!(
        "cross {:?} build cannot safely use PYO3_NO_PYTHON; provide PYO3_* configuration or set {COMPATIBILITY_ENV_VAR}=sysroot",
        shape
    ));
    plan
}

fn version_at_least(version: &str, required_major: u64, required_minor: u64) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|value| value.parse().ok());
    let minor = parts.next().and_then(|value| value.parse().ok());
    matches!((major, minor), (Some(major), Some(minor)) if (major, minor) >= (required_major, required_minor))
}

pub fn resolve_for_invocation(
    workspace_root: &Path,
    args: &[String],
    target_override: Option<&str>,
) -> Pyo3BuildPlan {
    let target = target_override
        .map(normalize_target)
        .unwrap_or_else(|| resolve_build_target(args, workspace_root));
    let host = host_triple().to_string();
    resolve_for_target(workspace_root, args, host, target)
}

/// Cargo-front-door variant that can skip metadata only when soldr has
/// positively resolved the target Cargo will use. A missing target is not
/// proof of a native build because `.cargo/config.toml` may select a cross
/// target behind soldr's back.
pub(crate) fn resolve_for_cargo_invocation(
    workspace_root: &Path,
    args: &[String],
    known_target: Option<&str>,
) -> Pyo3BuildPlan {
    let host = host_triple().to_string();
    // An explicit --target is Cargo's strongest target selector and must win
    // even if the caller also exported CARGO_BUILD_TARGET. Conversely, an
    // arbitrary --config may contain `build.target`; unless --target is
    // present, retain the metadata path rather than treating a weaker target
    // source as authoritative.
    let explicit_target = explicit_target(args)
        .map(|target| normalize_target(&target))
        .filter(|target| !target.is_empty());
    let config_may_select_target = explicit_target.is_none()
        && args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| arg == "--config" || arg.starts_with("--config="));
    let known_target = explicit_target.or_else(|| {
        (!config_may_select_target)
            .then(|| known_target.map(normalize_target))
            .flatten()
            .filter(|target| !target.is_empty())
    });
    if !host.is_empty() && known_target.as_deref() == Some(host.as_str()) {
        return resolve_policy(PolicyInput {
            host,
            target: known_target.expect("checked above"),
            detected: None,
            caller_pyo3: BTreeMap::new(),
            compatibility_sysroot: false,
            raw_dylib_disabled: false,
        });
    }

    match known_target {
        Some(target) => resolve_for_target(workspace_root, args, host, target),
        None => resolve_for_invocation(workspace_root, args, None),
    }
}

fn resolve_for_target(
    workspace_root: &Path,
    args: &[String],
    host: String,
    target: String,
) -> Pyo3BuildPlan {
    let detected = match detect_workspace_pyo3(workspace_root, args, &target) {
        Ok(value) => value,
        Err(error) => {
            return Pyo3BuildPlan {
                host,
                target,
                shape: BuildShape::Absent,
                pyo3_versions: Vec::new(),
                pyo3_features: Vec::new(),
                mode: PlanMode::Unresolved,
                env: BTreeMap::new(),
                needs_python_sysroot: false,
                diagnostic: Some(format!(
                    "Cargo metadata could not prove PyO3 eligibility ({error}); no PyO3 variables were injected"
                )),
            };
        }
    };
    let compatibility_sysroot = std::env::var(COMPATIBILITY_ENV_VAR)
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "sysroot"
            )
        });
    let raw_dylib_disabled = std::env::var("PYO3_USE_RAW_DYLIB")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
    let mut detected = detected;
    if let Some(kind) = build_kind_override() {
        if let Some(value) = &mut detected {
            value.shape = kind;
        }
    }
    resolve_policy(PolicyInput {
        host,
        target,
        detected,
        caller_pyo3: caller_pyo3_env(),
        compatibility_sysroot,
        raw_dylib_disabled,
    })
}

fn build_kind_override() -> Option<BuildShape> {
    match std::env::var(BUILD_KIND_ENV_VAR)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "extension" => Some(BuildShape::Extension),
        "embedding" => Some(BuildShape::Embedding),
        _ => None,
    }
}

fn caller_pyo3_env() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(key, _)| key.starts_with("PYO3_"))
        .collect()
}

pub fn resolve_build_target(args: &[String], workspace_root: &Path) -> String {
    choose_build_target(
        args,
        std::env::var("CARGO_BUILD_TARGET").ok().as_deref(),
        project_maturin_target(workspace_root).as_deref(),
        host_triple(),
    )
}

fn choose_build_target(
    args: &[String],
    env_target: Option<&str>,
    project_target: Option<&str>,
    host: &str,
) -> String {
    explicit_target(args)
        .or_else(|| {
            env_target
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            project_target
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .map(|target| normalize_target(&target))
        .unwrap_or_else(|| host.to_string())
}

fn explicit_target(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--target=") {
            return Some(value.to_string());
        }
    }
    None
}

pub fn normalize_explicit_target_args(args: &[String]) -> Vec<String> {
    let mut normalized = args.to_vec();
    let mut index = 0;
    while index < normalized.len() {
        if normalized[index] == "--target" {
            if let Some(value) = normalized.get_mut(index + 1) {
                *value = normalize_target(value);
            }
            index += 1;
        } else if let Some(value) = normalized[index].strip_prefix("--target=") {
            normalized[index] = format!("--target={}", normalize_target(value));
        }
        index += 1;
    }
    normalized
}

pub fn maturin_args_are_build(args: &[String]) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "-V" | "--version"))
    {
        return false;
    }

    match args.first().map(String::as_str) {
        Some("build" | "develop") => true,
        Some("pep517") => matches!(
            args.get(1).map(String::as_str),
            Some("build-wheel" | "write-dist-info")
        ),
        _ => false,
    }
}

fn project_maturin_target(start: &Path) -> Option<String> {
    let mut current = start.canonicalize().ok()?;
    loop {
        let pyproject = current.join("pyproject.toml");
        if let Ok(text) = std::fs::read_to_string(&pyproject) {
            if let Ok(document) = text.parse::<toml::Value>() {
                if let Some(target) = document
                    .get("tool")
                    .and_then(|value| value.get("maturin"))
                    .and_then(|value| value.get("target"))
                    .and_then(toml::Value::as_str)
                {
                    return Some(target.to_string());
                }
            }
        }
        if !current.pop() {
            break;
        }
    }
    None
}

fn normalize_target(target: &str) -> String {
    crate::target_alias::resolve_soldr_target(target)
        .map(|resolved| resolved.rust_triple)
        .unwrap_or_else(|_| target.trim().to_string())
}

pub fn host_triple() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        ""
    }
}

pub fn workspace_uses_pyo3(cwd: &Path) -> bool {
    detect_workspace_pyo3(cwd, &[], host_triple())
        .ok()
        .flatten()
        .is_some()
}

pub fn cross_env_for_target(workspace_root: &Path, target: &str) -> BTreeMap<String, String> {
    resolve_for_invocation(workspace_root, &[], Some(target)).env
}

fn detect_workspace_pyo3(
    workspace_root: &Path,
    args: &[String],
    target: &str,
) -> Result<Option<DetectedPyo3>, String> {
    let cargo =
        crate::binaries::resolve_toolchain_binary("cargo").map_err(|error| error.to_string())?;
    let mut command = Command::new(cargo);
    command.args(["metadata", "--format-version", "1"]);
    command.current_dir(workspace_root);
    // Cargo metadata may invoke rustc through a rustup proxy. In managed CI
    // the pinned toolchain lives in a private rustup home and is not the
    // user's default, so carry the explicit channel into this probe just as
    // the eventual cargo child does. Without it rustup reports that no
    // default toolchain is configured and PyO3 detection becomes lossy.
    if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        if !toolchain.is_empty() {
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
    } else if let Ok(manifest) = crate::core::read_rust_toolchain_manifest(workspace_root) {
        if let Some(channel) = manifest.channel {
            let channel = channel.trim();
            if !channel.is_empty() {
                command.env("RUSTUP_TOOLCHAIN", channel);
            }
        }
    }
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove("RUSTC_WORKSPACE_WRAPPER");
    if !target.is_empty() {
        command.args(["--filter-platform", target]);
    }
    append_metadata_feature_args(&mut command, args);
    if let Some(path) = manifest_path_arg(args) {
        command.args(["--manifest-path", &path]);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    detect_from_metadata_json(&output.stdout, args)
}

fn append_metadata_feature_args(command: &mut Command, args: &[String]) {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "--all-features" | "--no-default-features") {
            command.arg(arg);
        } else if arg == "--features" {
            if let Some(value) = args.get(index + 1) {
                command.args([arg, value]);
                index += 1;
            }
        } else if arg.starts_with("--features=") {
            command.arg(arg);
        }
        index += 1;
    }
}

fn manifest_path_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--manifest-path" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(value.to_string());
        }
    }
    None
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    workspace_members: Vec<String>,
    #[serde(default)]
    workspace_default_members: Vec<String>,
    resolve: Option<CargoResolve>,
}

#[derive(Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    version: String,
    targets: Vec<CargoTarget>,
}

#[derive(Deserialize)]
struct CargoTarget {
    kind: Vec<String>,
    crate_types: Vec<String>,
}

#[derive(Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoNode>,
}

#[derive(Deserialize)]
struct CargoNode {
    id: String,
    #[serde(default)]
    deps: Vec<CargoDep>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Deserialize)]
struct CargoDep {
    pkg: String,
}

fn detect_from_metadata_json(
    bytes: &[u8],
    args: &[String],
) -> Result<Option<DetectedPyo3>, String> {
    let metadata: CargoMetadata =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let Some(resolve) = &metadata.resolve else {
        return Ok(None);
    };
    let packages: HashMap<_, _> = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect();
    let pyo3_ids: HashSet<_> = metadata
        .packages
        .iter()
        .filter(|package| package.name == "pyo3")
        .map(|package| package.id.as_str())
        .collect();
    if pyo3_ids.is_empty() {
        return Ok(None);
    }
    let nodes: HashMap<_, _> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let selected_members = selected_workspace_members(&metadata, args, &packages);
    let reachable_pyo3 = reachable_pyo3_ids(&selected_members, &nodes, &pyo3_ids);
    if reachable_pyo3.is_empty() {
        return Ok(None);
    }
    let mut extension = false;
    let mut embedding = false;
    for member in selected_members.iter().copied() {
        if !reaches_any(member, &nodes, &reachable_pyo3) {
            continue;
        }
        if let Some(package) = packages.get(member) {
            for target in &package.targets {
                extension |= target.crate_types.iter().any(|kind| kind == "cdylib");
                embedding |= target.kind.iter().any(|kind| kind == "bin");
            }
        }
    }
    let mut versions = BTreeSet::new();
    let mut features = BTreeSet::new();
    for id in &reachable_pyo3 {
        if let Some(package) = packages.get(id) {
            versions.insert(package.version.clone());
        }
        if let Some(node) = nodes.get(id) {
            features.extend(node.features.iter().cloned());
        }
    }
    extension |= features.contains("extension-module");
    embedding |= features.contains("auto-initialize");
    let shape = match (extension, embedding) {
        (true, false) => BuildShape::Extension,
        (false, true) => BuildShape::Embedding,
        (true, true) => BuildShape::Ambiguous,
        (false, false) => BuildShape::Embedding,
    };
    Ok(Some(DetectedPyo3 {
        shape,
        versions,
        features,
    }))
}

fn selected_workspace_members<'a>(
    metadata: &'a CargoMetadata,
    args: &[String],
    packages: &HashMap<&str, &'a CargoPackage>,
) -> Vec<&'a str> {
    let requested = package_args(args);
    let use_all = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--workspace" | "--all"));
    let candidates = if use_all || metadata.workspace_default_members.is_empty() {
        &metadata.workspace_members
    } else {
        &metadata.workspace_default_members
    };
    candidates
        .iter()
        .filter(|id| {
            requested.is_empty()
                || packages.get(id.as_str()).is_some_and(|package| {
                    requested
                        .iter()
                        .any(|spec| package_spec_matches(package, spec))
                })
        })
        .map(String::as_str)
        .collect()
}

fn package_args(args: &[String]) -> Vec<String> {
    let mut packages = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if matches!(arg.as_str(), "-p" | "--package") {
            if let Some(value) = iter.next() {
                packages.push(value.clone());
            }
        } else if let Some(value) = arg.strip_prefix("--package=") {
            packages.push(value.to_string());
        }
    }
    packages
}

fn package_spec_matches(package: &CargoPackage, spec: &str) -> bool {
    package.name == spec
        || spec
            .split_once('@')
            .is_some_and(|(name, version)| name == package.name && version == package.version)
}

fn reachable_pyo3_ids<'a>(
    starts: &[&'a str],
    nodes: &HashMap<&'a str, &'a CargoNode>,
    pyo3_ids: &HashSet<&'a str>,
) -> HashSet<&'a str> {
    let mut pending = starts.to_vec();
    let mut seen = HashSet::new();
    let mut reachable = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if pyo3_ids.contains(id) {
            reachable.insert(id);
        }
        if let Some(node) = nodes.get(id) {
            pending.extend(node.deps.iter().map(|dep| dep.pkg.as_str()));
        }
    }
    reachable
}

fn reaches_any(start: &str, nodes: &HashMap<&str, &CargoNode>, targets: &HashSet<&str>) -> bool {
    let mut pending = vec![start];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if targets.contains(id) {
            return true;
        }
        if let Some(node) = nodes.get(id) {
            pending.extend(node.deps.iter().map(|dep| dep.pkg.as_str()));
        }
    }
    false
}

#[cfg(test)]
impl PolicyInput {
    fn test(shape: BuildShape, abi3: bool, host: &str, target: &str) -> Self {
        let detected = (shape != BuildShape::Absent).then(|| DetectedPyo3 {
            shape,
            versions: BTreeSet::from(["0.29.0".to_string()]),
            features: if abi3 {
                BTreeSet::from(["abi3-py310".to_string()])
            } else {
                BTreeSet::new()
            },
        });
        Self {
            host: host.to_string(),
            target: target.to_string(),
            detected,
            caller_pyo3: BTreeMap::new(),
            compatibility_sysroot: false,
            raw_dylib_disabled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(target_aware_policy_matrix, {
        let cases = [
            (
                "native",
                BuildShape::Extension,
                true,
                "x86_64-unknown-linux-gnu",
                "x86_64-unknown-linux-gnu",
                PlanMode::Native,
                false,
            ),
            (
                "no-pyo3",
                BuildShape::Absent,
                false,
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
                PlanMode::NoPyo3,
                false,
            ),
            (
                "abi3-cross",
                BuildShape::Extension,
                true,
                "x86_64-unknown-linux-gnu",
                "x86_64-apple-darwin",
                PlanMode::Abi3NoPython,
                true,
            ),
            (
                "modern-windows",
                BuildShape::Extension,
                false,
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
                PlanMode::ModernWindowsRawDylib,
                false,
            ),
            (
                "modern-unix-extension",
                BuildShape::Extension,
                false,
                "x86_64-unknown-linux-gnu",
                "aarch64-apple-darwin",
                PlanMode::ExtensionDefault,
                false,
            ),
            (
                "embedding",
                BuildShape::Embedding,
                false,
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-msvc",
                PlanMode::RequiresExplicitCompatibility,
                false,
            ),
        ];
        for (name, shape, abi3, host, target, expected_mode, no_python) in cases {
            let plan = resolve_policy(PolicyInput::test(shape, abi3, host, target));
            assert_eq!(plan.mode, expected_mode, "{name}");
            assert_eq!(plan.env.contains_key("PYO3_NO_PYTHON"), no_python, "{name}");
        }
    });

    crate::timed_test!(non_abi3_and_legacy_are_never_silently_abi3, {
        for (shape, version, target) in [
            (BuildShape::Embedding, "0.29.0", "x86_64-pc-windows-msvc"),
            (BuildShape::Extension, "0.22.6", "x86_64-pc-windows-msvc"),
            (BuildShape::Extension, "0.22.6", "aarch64-apple-darwin"),
        ] {
            let mut input = PolicyInput::test(shape, false, "x86_64-unknown-linux-gnu", target);
            input.detected.as_mut().unwrap().versions = BTreeSet::from([version.to_string()]);
            let plan = resolve_policy(input);
            assert_eq!(plan.mode, PlanMode::RequiresExplicitCompatibility);
            assert!(!plan.env.contains_key("PYO3_NO_PYTHON"));
        }

        let mut raw_dylib_disabled = PolicyInput::test(
            BuildShape::Extension,
            false,
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        );
        raw_dylib_disabled.raw_dylib_disabled = true;
        assert_eq!(
            resolve_policy(raw_dylib_disabled).mode,
            PlanMode::RequiresExplicitCompatibility
        );
    });

    crate::timed_test!(explicit_compatibility_and_caller_overrides_win, {
        let mut compatibility = PolicyInput::test(
            BuildShape::Embedding,
            false,
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
        );
        compatibility.compatibility_sysroot = true;
        let plan = resolve_policy(compatibility);
        assert_eq!(plan.mode, PlanMode::CompatibilitySysroot);
        assert!(plan.needs_python_sysroot);

        let mut caller = PolicyInput::test(
            BuildShape::Extension,
            true,
            "x86_64-unknown-linux-gnu",
            "x86_64-apple-darwin",
        );
        caller
            .caller_pyo3
            .insert("PYO3_CROSS_LIB_DIR".into(), "/caller".into());
        let plan = resolve_policy(caller);
        assert_eq!(plan.mode, PlanMode::CallerConfigured);
        assert!(plan.env.is_empty());
    });

    crate::timed_test!(
        compatibility_sysroot_exports_explicit_target_python_config,
        {
            let env = compatibility_sysroot_env(Path::new("/sdk/python/package"), "3.13.14");
            assert_eq!(env.get("PYO3_CROSS").map(String::as_str), Some("1"));
            let expected_lib_dir = Path::new("/sdk/python/package")
                .join("lib")
                .display()
                .to_string();
            assert_eq!(
                env.get("PYO3_CROSS_LIB_DIR").map(String::as_str),
                Some(expected_lib_dir.as_str())
            );
            assert_eq!(
                env.get("PYO3_CROSS_PYTHON_VERSION").map(String::as_str),
                Some("3.13")
            );
            assert_eq!(
                env.get("PYO3_CROSS_PYTHON_IMPLEMENTATION")
                    .map(String::as_str),
                Some("CPython")
            );
        }
    );

    crate::timed_test!(target_precedence_is_args_env_project_host, {
        let args = ["--target".into(), "win-x64".into()];
        assert_eq!(
            choose_build_target(
                &args,
                Some("x86_64-apple-darwin"),
                Some("aarch64-apple-darwin"),
                "x86_64-unknown-linux-gnu",
            ),
            "x86_64-pc-windows-msvc"
        );
        assert_eq!(
            choose_build_target(
                &[],
                Some("x86_64-apple-darwin"),
                Some("aarch64-apple-darwin"),
                "x86_64-unknown-linux-gnu",
            ),
            "x86_64-apple-darwin"
        );
        assert_eq!(
            choose_build_target(
                &[],
                None,
                Some("aarch64-apple-darwin"),
                "x86_64-unknown-linux-gnu",
            ),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            choose_build_target(&[], None, None, "x86_64-unknown-linux-gnu"),
            "x86_64-unknown-linux-gnu"
        );
    });

    crate::timed_test!(cargo_metadata_resolves_active_version_features_and_shape, {
        let metadata = serde_json::json!({
            "workspace_members": ["app 0.1.0 (path+file:///app)"],
            "packages": [
                {
                    "id": "app 0.1.0 (path+file:///app)",
                    "name": "app",
                    "version": "0.1.0",
                    "targets": [{"kind": ["cdylib"], "crate_types": ["cdylib"]}]
                },
                {
                    "id": "registry+pyo3#0.29.0",
                    "name": "pyo3",
                    "version": "0.29.0",
                    "targets": [{"kind": ["lib"], "crate_types": ["lib"]}]
                }
            ],
            "resolve": {"nodes": [
                {"id": "app 0.1.0 (path+file:///app)", "deps": [{"pkg": "registry+pyo3#0.29.0"}], "features": []},
                {"id": "registry+pyo3#0.29.0", "deps": [], "features": ["abi3-py310"]}
            ]}
        });
        let detected = detect_from_metadata_json(&serde_json::to_vec(&metadata).unwrap(), &[])
            .unwrap()
            .unwrap();
        assert_eq!(detected.shape, BuildShape::Extension);
        assert_eq!(detected.versions, BTreeSet::from(["0.29.0".to_string()]));
        assert!(detected.abi3());
    });

    crate::timed_test!(metadata_ignores_unreachable_pyo3_versions_and_features, {
        let metadata = serde_json::json!({
            "workspace_members": ["app"],
            "workspace_default_members": ["app"],
            "packages": [
                {"id": "app", "name": "app", "version": "0.1.0", "targets": [{"kind": ["cdylib"], "crate_types": ["cdylib"]}]},
                {"id": "pyo3-new", "name": "pyo3", "version": "0.29.0", "targets": []},
                {"id": "pyo3-old", "name": "pyo3", "version": "0.22.6", "targets": []}
            ],
            "resolve": {"nodes": [
                {"id": "app", "deps": [{"pkg": "pyo3-new"}], "features": []},
                {"id": "pyo3-new", "deps": [], "features": ["abi3-py310"]},
                {"id": "pyo3-old", "deps": [], "features": ["auto-initialize"]}
            ]}
        });
        let detected = detect_from_metadata_json(&serde_json::to_vec(&metadata).unwrap(), &[])
            .unwrap()
            .unwrap();
        assert_eq!(detected.versions, BTreeSet::from(["0.29.0".to_string()]));
        assert!(!detected.features.contains("auto-initialize"));
        assert_eq!(detected.shape, BuildShape::Extension);
    });

    crate::timed_test!(maturin_target_aliases_are_normalized_before_exec, {
        assert_eq!(
            normalize_explicit_target_args(&[
                "pep517".into(),
                "build-wheel".into(),
                "--target".into(),
                "win-x64".into(),
            ]),
            [
                "pep517",
                "build-wheel",
                "--target",
                "x86_64-pc-windows-msvc",
            ]
        );
        assert_eq!(
            normalize_explicit_target_args(&["build".into(), "--target=mac-arm64".into()]),
            ["build", "--target=aarch64-apple-darwin"]
        );
    });

    crate::timed_test!(only_build_producing_maturin_commands_receive_policy, {
        for args in [
            vec!["build".into()],
            vec!["develop".into()],
            vec!["pep517".into(), "build-wheel".into()],
            vec!["pep517".into(), "write-dist-info".into()],
        ] {
            assert!(maturin_args_are_build(&args), "{args:?}");
        }
        for args in [
            vec!["--version".into()],
            vec!["build".into(), "--help".into()],
            vec!["pep517".into(), "write-sdist".into()],
            vec!["list-python".into()],
        ] {
            assert!(!maturin_args_are_build(&args), "{args:?}");
        }
    });

    crate::timed_test!(host_triple_resolves_to_known_triple, {
        let host = host_triple();
        assert!(host.is_empty() || crate::core::is_canonical(host) || host.contains('-'));
    });

    crate::timed_test!(
        known_native_cargo_resolution_does_not_require_workspace_metadata,
        {
            let temp = tempfile::tempdir().expect("tempdir");
            let missing_workspace = temp.path().join("does-not-exist");
            let host = host_triple();

            let plan = resolve_for_cargo_invocation(&missing_workspace, &[], Some(host));

            assert_eq!(plan.mode, PlanMode::Native);
            assert!(plan.env.is_empty());
            assert!(plan.diagnostic.is_none());
        }
    );

    crate::timed_test!(unknown_cargo_target_does_not_assume_native, {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_workspace = temp.path().join("does-not-exist");

        let plan = resolve_for_cargo_invocation(&missing_workspace, &[], None);

        assert_eq!(plan.mode, PlanMode::Unresolved);
        assert!(plan
            .diagnostic
            .as_deref()
            .is_some_and(|message| message.contains("metadata")));
    });

    crate::timed_test!(cargo_config_target_keeps_conservative_metadata_path, {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_workspace = temp.path().join("does-not-exist");
        let args = vec![
            "build".to_string(),
            "--config".to_string(),
            "build.target=\"aarch64-unknown-linux-gnu\"".to_string(),
        ];

        let plan = resolve_for_cargo_invocation(&missing_workspace, &args, Some(host_triple()));

        assert_eq!(plan.mode, PlanMode::Unresolved);
    });

    crate::timed_test!(explicit_cross_target_beats_weaker_known_native_target, {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_workspace = temp.path().join("does-not-exist");
        let cross_target = if host_triple().contains("windows") {
            "aarch64-apple-darwin"
        } else {
            "x86_64-pc-windows-msvc"
        };
        let args = vec![
            "build".to_string(),
            "--target".to_string(),
            cross_target.to_string(),
        ];

        let plan = resolve_for_cargo_invocation(&missing_workspace, &args, Some(host_triple()));

        assert_eq!(plan.mode, PlanMode::Unresolved);
        assert_eq!(plan.target, cross_target);
    });

    crate::timed_test!(target_after_separator_cannot_override_known_cargo_target, {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_workspace = temp.path().join("does-not-exist");
        let cross_target = if host_triple().contains("windows") {
            "aarch64-apple-darwin"
        } else {
            "x86_64-pc-windows-msvc"
        };
        let args = vec![
            "run".to_string(),
            "--".to_string(),
            "--target".to_string(),
            host_triple().to_string(),
        ];

        let plan = resolve_for_cargo_invocation(&missing_workspace, &args, Some(cross_target));

        assert_eq!(plan.mode, PlanMode::Unresolved);
        assert_eq!(plan.target, cross_target);
    });

    crate::timed_test!(public_native_plan_keeps_metadata_reporting_semantics, {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_workspace = temp.path().join("does-not-exist");

        let plan = resolve_for_invocation(&missing_workspace, &[], Some(host_triple()));

        assert_eq!(plan.mode, PlanMode::Unresolved);
    });
}
