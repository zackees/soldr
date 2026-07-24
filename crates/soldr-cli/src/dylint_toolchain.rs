//! Catalogue-driven nightly selection for Dylint and its nested commands.

use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

use crate::core::{command_output_with_timeout, suppress_windows_console_window, SoldrError};
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary_for_channel, rustup_binary};

pub(crate) const TOOLCHAIN_ENV_VAR: &str = "SOLDR_DYLINT_TOOLCHAIN";
pub(crate) const COMPILER_RELEASE_ENV_VAR: &str = "SOLDR_DYLINT_RUSTC_RELEASE";
pub(crate) const COMPILER_COMMIT_ENV_VAR: &str = "SOLDR_DYLINT_RUSTC_COMMIT_HASH";
pub(crate) const CACHE_IDENTITY_ENV_VAR: &str = "SOLDR_DYLINT_CACHE_IDENTITY";
pub(crate) const PREPARED_IDENTITY_ENV_VAR: &str = "SOLDR_DYLINT_PREPARED_IDENTITY";
pub(crate) const SUCCESS_MARKER_ENV_VAR: &str = "SOLDR_DYLINT_SUCCESS_MARKER";
pub(crate) const CONFIGURED_TOOLCHAIN_ENV_VAR: &str = "SOLDR_DYLINT_CONFIGURED_TOOLCHAIN";
pub(crate) const CONFIGURED_COMPILER_RELEASE_ENV_VAR: &str =
    "SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE";
pub(crate) const CONFIGURED_COMPILER_COMMIT_ENV_VAR: &str =
    "SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH";

const MAP_ASSET: &str = "rust-nightly-versions.v1.json";
const REQUIRED_COMPONENTS: &[&str] = &["rustc-dev", "rust-src", "llvm-tools-preview"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DylintToolchainPlan {
    pub channel: String,
    pub compiler_release: String,
    pub compiler_commit: String,
}

impl DylintToolchainPlan {
    pub(crate) fn cache_identity(&self) -> String {
        format!(
            "{}|{}|{}",
            self.channel, self.compiler_release, self.compiler_commit
        )
    }

    pub(crate) fn apply_to_command(&self, command: &mut std::process::Command) {
        command.env("RUSTUP_TOOLCHAIN", &self.channel);
        command.env(TOOLCHAIN_ENV_VAR, &self.channel);
        command.env(COMPILER_RELEASE_ENV_VAR, &self.compiler_release);
        command.env(COMPILER_COMMIT_ENV_VAR, &self.compiler_commit);
        command.env(CACHE_IDENTITY_ENV_VAR, self.cache_identity());
        command.env(PREPARED_IDENTITY_ENV_VAR, self.cache_identity());
    }
}

pub(crate) fn write_success_marker(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let Some(path) = std::env::var_os(SUCCESS_MARKER_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", plan.cache_identity()))?;
    Ok(())
}

pub(crate) fn clear_success_marker() -> Result<(), SoldrError> {
    let Some(path) = std::env::var_os(SUCCESS_MARKER_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
    else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Deserialize)]
struct NightlyVersionMap {
    schema_version: u32,
    nightlies: BTreeMap<String, NightlyIdentity>,
    versions: BTreeMap<String, VersionBucket>,
}

#[derive(Debug, Deserialize)]
struct NightlyIdentity {
    rust_version: String,
    rustc_release: String,
    rustc_commit_hash: String,
}

#[derive(Debug, Deserialize)]
struct VersionBucket {
    nightlies: Vec<String>,
    selected: String,
}

pub(crate) async fn prepare(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<DylintToolchainPlan, SoldrError> {
    let mut plan = if let Some(channel) = non_empty_env(TOOLCHAIN_ENV_VAR) {
        plan_from_retained_environment(&channel)?
    } else if let Some(plan) = plan_from_configured_environment()? {
        plan
    } else {
        let version = requested_rust_version(requested_channel, workspace_root)?;
        let bytes = crate::fetch::fetch_verified_catalogue_asset(
            "zackees",
            "soldr-toolchain",
            "assets",
            MAP_ASSET,
        )
        .await?;
        select_from_map(&bytes, &version)?
    };
    let prepared_identity = plan.cache_identity();
    if non_empty_env(PREPARED_IDENTITY_ENV_VAR).as_deref() != Some(prepared_identity.as_str()) {
        ensure_installed(&plan)?;
        verify_installed_identity(&plan)?;
    }
    plan.channel = qualify_toolchain_name(&plan.channel)?;
    Ok(plan)
}

fn qualify_toolchain_name(channel: &str) -> Result<String, SoldrError> {
    if is_fully_qualified_nightly(channel) {
        return Ok(channel.to_string());
    }
    let binary = resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel))?;
    let host = observe_compiler_host(&binary, channel)?;
    Ok(format!("{channel}-{host}"))
}

fn is_fully_qualified_nightly(channel: &str) -> bool {
    channel
        .strip_prefix("nightly-")
        .and_then(|value| value.get(10..))
        .is_some_and(|suffix| suffix.starts_with('-') && suffix.len() > 1)
}

fn plan_from_configured_environment() -> Result<Option<DylintToolchainPlan>, SoldrError> {
    let values = (
        non_empty_env(CONFIGURED_TOOLCHAIN_ENV_VAR),
        non_empty_env(CONFIGURED_COMPILER_RELEASE_ENV_VAR),
        non_empty_env(CONFIGURED_COMPILER_COMMIT_ENV_VAR),
    );
    match values {
        (None, None, None) => Ok(None),
        (Some(channel), Some(compiler_release), Some(compiler_commit)) => {
            let identity = NightlyIdentity {
                rust_version: major_minor(&compiler_release).unwrap_or_default(),
                rustc_release: compiler_release.clone(),
                rustc_commit_hash: compiler_commit.clone(),
            };
            validate_identity(&channel, &identity)?;
            Ok(Some(DylintToolchainPlan {
                channel,
                compiler_release,
                compiler_commit,
            }))
        }
        _ => Err(SoldrError::Other(
            "configured Dylint compiler identity is incomplete".into(),
        )),
    }
}

fn plan_from_retained_environment(channel: &str) -> Result<DylintToolchainPlan, SoldrError> {
    match (
        non_empty_env(COMPILER_RELEASE_ENV_VAR),
        non_empty_env(COMPILER_COMMIT_ENV_VAR),
    ) {
        (Some(compiler_release), Some(compiler_commit)) => Ok(DylintToolchainPlan {
            channel: channel.to_string(),
            compiler_release,
            compiler_commit,
        }),
        _ => {
            let (compiler_release, compiler_commit) = observe_compiler(channel)?;
            Ok(DylintToolchainPlan {
                channel: channel.to_string(),
                compiler_release,
                compiler_commit,
            })
        }
    }
}

fn requested_rust_version(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<String, SoldrError> {
    let manifest = crate::core::read_rust_toolchain_manifest(workspace_root)?;
    let channel = requested_channel
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env("RUSTUP_TOOLCHAIN"))
        .or(manifest.channel);
    if let Some(channel) = channel {
        if let Some(version) = major_minor(&channel) {
            return Ok(version);
        }
        let (release, _) = observe_compiler(&channel)?;
        return major_minor(&release).ok_or_else(|| {
            SoldrError::Other(format!(
                "could not derive a major.minor Rust version from `{release}`"
            ))
        });
    }
    let binary = crate::resolve_toolchain_binary(concat!("rust", "c"))?;
    let (release, _) = observe_compiler_binary(&binary, "the default Rust toolchain")?;
    major_minor(&release).ok_or_else(|| {
        SoldrError::Other(format!(
            "could not derive a major.minor Rust version from `{release}`"
        ))
    })
}

fn select_from_map(bytes: &[u8], version: &str) -> Result<DylintToolchainPlan, SoldrError> {
    let map: NightlyVersionMap = serde_json::from_slice(bytes)
        .map_err(|error| SoldrError::Other(format!("failed to parse {MAP_ASSET}: {error}")))?;
    if map.schema_version != 1 {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} has unsupported schema_version {}",
            map.schema_version
        )));
    }
    let bucket = map.versions.get(version).ok_or_else(|| {
        SoldrError::Other(format!(
            "{MAP_ASSET} has no nightly mapping for Rust {version}"
        ))
    })?;
    if bucket.nightlies.first() != Some(&bucket.selected) {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} violates its newest-first contract for Rust {version}"
        )));
    }
    if !bucket.nightlies.windows(2).all(|pair| pair[0] > pair[1]) {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} nightlies for Rust {version} are not descending"
        )));
    }
    let identity = map.nightlies.get(&bucket.selected).ok_or_else(|| {
        SoldrError::Other(format!(
            "{MAP_ASSET} selects missing nightly {}",
            bucket.selected
        ))
    })?;
    if identity.rust_version != version {
        return Err(SoldrError::Other(format!(
            "{} is indexed under Rust {version} but reports Rust {}",
            bucket.selected, identity.rust_version
        )));
    }
    validate_identity(&bucket.selected, identity)?;
    Ok(DylintToolchainPlan {
        channel: bucket.selected.clone(),
        compiler_release: identity.rustc_release.clone(),
        compiler_commit: identity.rustc_commit_hash.clone(),
    })
}

fn validate_identity(channel: &str, identity: &NightlyIdentity) -> Result<(), SoldrError> {
    if !channel.starts_with("nightly-")
        || !identity.rustc_release.ends_with("-nightly")
        || identity.rustc_commit_hash.len() != 40
        || !identity
            .rustc_commit_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} contains a malformed identity for {channel}"
        )));
    }
    Ok(())
}

fn ensure_installed(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let installed = installed_components(&plan.channel).unwrap_or_default();
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(&plan.channel)).is_err() {
        let code = crate::toolchain::rustup_toolchain_install_with_profile(
            &plan.channel,
            Some("minimal"),
        )?;
        if code != 0 {
            return Err(SoldrError::Other(format!(
                "rustup failed to install {} (exit {code})",
                plan.channel
            )));
        }
    }
    for component in REQUIRED_COMPONENTS {
        if !installed.iter().any(|line| line.starts_with(component)) {
            let code = crate::toolchain::rustup_component_add(&plan.channel, component)?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "rustup failed to add {component} to {} (exit {code})",
                    plan.channel
                )));
            }
        }
    }
    Ok(())
}

fn installed_components(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "list", "--toolchain", channel, "--installed"]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "rustup component list for Dylint")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn verify_installed_identity(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let observed = observe_compiler(&plan.channel)?;
    if observed.0 != plan.compiler_release || observed.1 != plan.compiler_commit {
        return Err(SoldrError::Other(format!(
            "installed {} differs from catalogue: expected {} {}, got {} {}",
            plan.channel, plan.compiler_release, plan.compiler_commit, observed.0, observed.1
        )));
    }
    Ok(())
}

fn observe_compiler(channel: &str) -> Result<(String, String), SoldrError> {
    let binary = resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel))?;
    observe_compiler_binary(&binary, channel)
}

fn observe_compiler_binary(
    binary: &Path,
    description: &str,
) -> Result<(String, String), SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.arg("-vV");
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "Dylint compiler identity")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "{description} identity probe failed with {}",
            output.status
        )));
    }
    parse_compiler_verbose(&String::from_utf8_lossy(&output.stdout))
}

fn observe_compiler_host(binary: &Path, description: &str) -> Result<String, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.arg("-vV");
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, "Dylint compiler host identity")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "{description} host identity probe failed with {}",
            output.status
        )));
    }
    parse_compiler_host(&String::from_utf8_lossy(&output.stdout))
}

fn parse_compiler_verbose(output: &str) -> Result<(String, String), SoldrError> {
    let mut release = None;
    let mut commit = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("release:") {
            release = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("commit-hash:") {
            commit = Some(value.trim().to_string());
        }
    }
    match (release, commit) {
        (Some(release), Some(commit)) if commit.len() == 40 => Ok((release, commit)),
        _ => Err(SoldrError::Other(
            "compiler identity lacks a release or full commit hash".into(),
        )),
    }
}

fn parse_compiler_host(output: &str) -> Result<String, SoldrError> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("host:").map(str::trim))
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .ok_or_else(|| SoldrError::Other("compiler identity lacks a host triple".into()))
}

fn major_minor(value: &str) -> Option<String> {
    let value = value
        .strip_prefix(concat!("rust", "c", " "))
        .unwrap_or(value);
    let version = value
        .split_whitespace()
        .next()?
        .trim_end_matches("-nightly");
    let mut parts = version.split('.');
    let major = parts.next()?;
    let minor = parts.next()?;
    (major.bytes().all(|byte| byte.is_ascii_digit())
        && minor.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| format!("{major}.{minor}"))
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "31a9463c6e2794a59ce57a8f37abc6966afc2a58";

    fn sample_map(selected: &str) -> Vec<u8> {
        format!(
            r#"{{
              "schema_version": 1,
              "nightlies": {{
                "nightly-2026-01-18": {{
                  "rust_version": "1.94",
                  "rustc_release": "1.94.0-nightly",
                  "rustc_commit_hash": "{COMMIT}"
                }},
                "nightly-2026-01-17": {{
                  "rust_version": "1.94",
                  "rustc_release": "1.94.0-nightly",
                  "rustc_commit_hash": "1111111111111111111111111111111111111111"
                }}
              }},
              "versions": {{
                "1.94": {{
                  "nightlies": ["nightly-2026-01-18", "nightly-2026-01-17"],
                  "selected": "{selected}"
                }}
              }}
            }}"#
        )
        .into_bytes()
    }

    crate::timed_test!(selects_first_newest_nightly_and_full_identity, {
        let plan =
            select_from_map(&sample_map("nightly-2026-01-18"), "1.94").expect("select map entry");
        assert_eq!(plan.channel, "nightly-2026-01-18");
        assert_eq!(
            plan.cache_identity(),
            format!("nightly-2026-01-18|1.94.0-nightly|{COMMIT}")
        );
    });

    crate::timed_test!(rejects_selected_nightly_that_is_not_first, {
        let error = select_from_map(&sample_map("nightly-2026-01-17"), "1.94")
            .expect_err("must reject a non-first selection");
        assert!(error.to_string().contains("newest-first contract"));
    });

    crate::timed_test!(extracts_major_minor_versions, {
        assert_eq!(major_minor("1.94.1").as_deref(), Some("1.94"));
        assert_eq!(major_minor("1.94.0-nightly").as_deref(), Some("1.94"));
        assert_eq!(major_minor("stable"), None);
    });

    crate::timed_test!(qualifies_nightly_names_with_the_compiler_host, {
        assert!(!is_fully_qualified_nightly("nightly-2026-01-18"));
        assert!(is_fully_qualified_nightly(
            "nightly-2026-01-18-x86_64-unknown-linux-gnu"
        ));
        let host = parse_compiler_host(
            "rustc 1.94.0-nightly\nrelease: 1.94.0-nightly\nhost: x86_64-unknown-linux-gnu\n",
        )
        .expect("parse host");
        assert_eq!(host, "x86_64-unknown-linux-gnu");
    });
}
