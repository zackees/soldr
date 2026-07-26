//! Catalogue-driven nightly selection for Dylint and its nested commands.

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::core::{
    command_output_with_timeout, suppress_windows_console_window, SoldrError, SoldrPaths,
    TargetTriple,
};
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

/// Overrides the freshness window (seconds) for the warm-run prepared-plan
/// marker consulted by [`prepare`]. `0` means "never trust the marker" —
/// every run pays the full catalogue-fetch + rustup-probe path. Unset
/// falls back to [`DEFAULT_PREPARE_TTL`].
pub(crate) const PREPARE_TTL_ENV_VAR: &str = "SOLDR_DYLINT_PREPARE_TTL_SECS";
/// When truthy (`1` / `true`, case-insensitive), skips the prepared-plan
/// marker entirely and always re-runs the full catalogue-fetch + rustup
/// verification path, even if a fresh marker exists on disk.
pub(crate) const REVERIFY_ENV_VAR: &str = "SOLDR_DYLINT_REVERIFY";

const MAP_ASSET: &str = "rust-nightly-versions.v1.json";
const REQUIRED_COMPONENTS: &[&str] = &["rustc-dev", "rust-src", "llvm-tools-preview"];

/// Default freshness window for the warm-run prepared-plan marker: 24
/// hours. Long enough that a normal dev inner loop (many `soldr cargo
/// dylint` invocations across a day) skips the network + subprocess
/// probes entirely, short enough that a stale marker is naturally
/// reclaimed without operator intervention.
const DEFAULT_PREPARE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Schema tag for the on-disk marker layout
/// (`<soldr_root>/dylint/prepared/<PREPARE_MARKER_SCHEMA>/<version>.identity`).
/// Bump this if the marker file format ever changes shape so stale
/// markers from an older soldr build are never misread.
const PREPARE_MARKER_SCHEMA: &str = "v1";

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
        apply_dylint_driver_path(command);
    }
}

/// Give the dylint driver cargo-dylint builds a stable soldr-owned
/// home instead of the tool's own unmanaged default (normally
/// `~/.dylint_drivers` or wherever `$DYLINT_DRIVER_PATH` happens to
/// point). A fixed path means warm runs reuse the already-built
/// driver and CI caches have something deterministic to restore.
/// Respects an explicit caller-set `DYLINT_DRIVER_PATH` — soldr never
/// clobbers a user override.
fn apply_dylint_driver_path(command: &mut std::process::Command) {
    if std::env::var_os("DYLINT_DRIVER_PATH").is_some() {
        return;
    }
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return;
    };
    let driver_dir = paths.root.join("dylint").join("drivers");
    if std::fs::create_dir_all(&driver_dir).is_err() {
        // Best-effort: if the directory cannot be created, fall back
        // to the tool's own default rather than pointing at a
        // nonexistent path.
        return;
    }
    command.env("DYLINT_DRIVER_PATH", driver_dir);
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
    if non_empty_env(TOOLCHAIN_ENV_VAR).is_none()
        && plan_from_configured_environment()?.is_none()
        && !truthy_env(REVERIFY_ENV_VAR)
    {
        let requested = requested_toolchain_channel(requested_channel, workspace_root)?;
        if requested
            .as_deref()
            .is_none_or(|channel| !is_dated_nightly(channel))
        {
            let version = requested_rust_version(requested.as_deref())?;
            if let Some(mut plan) = load_prepared_marker(&version) {
                plan.channel = qualify_toolchain_name(&plan.channel)?;
                eprintln!(
                    "soldr: dylint toolchain {} ready (cached identity)",
                    plan.channel
                );
                return Ok(plan);
            }
        }
    }
    let plan = resolve_plan_inner(requested_channel, workspace_root, true).await?;
    prepare_resolved(plan)
}

pub(crate) async fn resolve_plan(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<DylintToolchainPlan, SoldrError> {
    resolve_plan_inner(requested_channel, workspace_root, false).await
}

async fn resolve_plan_inner(
    requested_channel: Option<&str>,
    workspace_root: &Path,
    install_explicit_if_unmapped: bool,
) -> Result<DylintToolchainPlan, SoldrError> {
    let plan = if let Some(channel) = non_empty_env(TOOLCHAIN_ENV_VAR) {
        plan_from_retained_environment(&channel)?
    } else if let Some(plan) = plan_from_configured_environment()? {
        plan
    } else {
        let requested = requested_toolchain_channel(requested_channel, workspace_root)?;
        let bytes = crate::fetch::fetch_verified_catalogue_asset(
            "zackees",
            "soldr-toolchain",
            "assets",
            MAP_ASSET,
        )
        .await?;
        if let Some(channel) = requested.as_deref().filter(|value| is_dated_nightly(value)) {
            match select_explicit_from_map(&bytes, channel) {
                Ok(plan) => plan,
                Err(map_error) => {
                    if let Some(plan) = plan_from_installed_explicit_nightly(channel)? {
                        plan
                    } else if install_explicit_if_unmapped {
                        install_and_observe_explicit_nightly(channel)?
                    } else {
                        return Err(map_error);
                    }
                }
            }
        } else {
            let version = requested_rust_version(requested.as_deref())?;
            select_from_map(&bytes, &version)?
        }
    };
    Ok(plan)
}

fn plan_from_installed_explicit_nightly(
    channel: &str,
) -> Result<Option<DylintToolchainPlan>, SoldrError> {
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel)).is_err() {
        return Ok(None);
    }
    let (compiler_release, compiler_commit) = observe_compiler(channel)?;
    let identity = NightlyIdentity {
        rust_version: major_minor(&compiler_release).unwrap_or_default(),
        rustc_release: compiler_release.clone(),
        rustc_commit_hash: compiler_commit.clone(),
    };
    validate_identity(channel, &identity)?;
    Ok(Some(DylintToolchainPlan {
        channel: channel.to_string(),
        compiler_release,
        compiler_commit,
    }))
}

fn install_and_observe_explicit_nightly(channel: &str) -> Result<DylintToolchainPlan, SoldrError> {
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(channel)).is_err() {
        let code =
            crate::toolchain::rustup_toolchain_install_with_profile(channel, Some("minimal"))?;
        if code != 0 {
            return Err(SoldrError::Other(format!(
                "rustup failed to install {channel} (exit {code})"
            )));
        }
    }
    plan_from_installed_explicit_nightly(channel)?.ok_or_else(|| {
        SoldrError::Other(format!(
            "installed explicit Dylint toolchain `{channel}` could not be resolved"
        ))
    })
}

fn prepare_resolved(mut plan: DylintToolchainPlan) -> Result<DylintToolchainPlan, SoldrError> {
    let prepared_identity = plan.cache_identity();
    if non_empty_env(PREPARED_IDENTITY_ENV_VAR).as_deref() != Some(prepared_identity.as_str()) {
        ensure_installed(&plan)?;
        verify_installed_identity(&plan)?;
    }
    plan.channel = qualify_toolchain_name(&plan.channel)?;
    if let Some(version) = major_minor(&plan.compiler_release) {
        if let Err(error) = write_prepared_marker(&version, &plan) {
            eprintln!("soldr: failed to write dylint prepared marker: {error}");
        }
    }
    Ok(plan)
}

pub(crate) fn verify_if_installed(plan: &DylintToolchainPlan) -> Result<bool, SoldrError> {
    if resolve_toolchain_binary_for_channel(concat!("rust", "c"), Some(&plan.channel)).is_err() {
        return Ok(false);
    }
    verify_installed_identity(plan)?;
    Ok(true)
}

pub(crate) fn verify_observed_identity(plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    verify_installed_identity(plan)
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

/// `<soldr_root>/dylint/prepared/<PREPARE_MARKER_SCHEMA>/<sanitized version>.identity`
fn prepared_marker_path(base_dir: &Path, version: &str) -> PathBuf {
    base_dir
        .join("dylint")
        .join("prepared")
        .join(PREPARE_MARKER_SCHEMA)
        .join(format!("{}.identity", sanitize_marker_key(version)))
}

/// Replace any character that is not filesystem-safe across every
/// soldr-supported host with `_`. `version` is normally a plain
/// `major.minor` string, but this also tolerates an explicit channel
/// string being threaded through in the future without risking path
/// traversal or reserved-character issues on Windows.
fn sanitize_marker_key(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn prepare_ttl() -> Duration {
    non_empty_env(PREPARE_TTL_ENV_VAR)
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PREPARE_TTL)
}

fn truthy_env(key: &str) -> bool {
    non_empty_env(key)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Production entry point for the warm-path marker lookup: resolves the
/// real soldr root + `RUSTUP_HOME` and delegates to
/// [`load_prepared_marker_from`] with the current TTL policy and wall
/// clock. Returns `None` on any resolution failure so callers always
/// fall through to the full cold path rather than erroring.
fn load_prepared_marker(version: &str) -> Option<DylintToolchainPlan> {
    let base_dir = SoldrPaths::new().ok()?.root;
    let rustup_home = crate::core::resolve_rustup_home()?;
    load_prepared_marker_from(
        &base_dir,
        &rustup_home,
        version,
        prepare_ttl(),
        SystemTime::now(),
    )
}

/// Testable core of the warm-path marker lookup. A marker "hits" only
/// when all of the following hold:
///
/// * `ttl` is non-zero (zero means "never trust the marker"),
/// * the marker file exists and its mtime is within `ttl` of `now`,
/// * its single line parses into a shape-valid identity
///   (`validate_identity`'s channel / release / commit checks), and
/// * the toolchain directory the identity names is still present under
///   `rustup_home/toolchains/` (a cheap directory-existence check —
///   deliberately no subprocess spawn).
fn load_prepared_marker_from(
    base_dir: &Path,
    rustup_home: &Path,
    version: &str,
    ttl: Duration,
    now: SystemTime,
) -> Option<DylintToolchainPlan> {
    if ttl.is_zero() {
        return None;
    }
    let path = prepared_marker_path(base_dir, version);
    let metadata = std::fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age > ttl {
        return None;
    }
    let contents = std::fs::read_to_string(&path).ok()?;
    let plan = parse_marker_identity(contents.lines().next()?)?;
    if !is_toolchain_installed_at(rustup_home, &plan.channel) {
        return None;
    }
    Some(plan)
}

/// Parse a single `<channel>|<rustc_release>|<commit_hash>` marker line
/// (the same shape [`DylintToolchainPlan::cache_identity`] produces)
/// back into a plan, reusing [`validate_identity`]'s shape checks so a
/// corrupted or foreign-format marker is rejected rather than trusted.
fn parse_marker_identity(line: &str) -> Option<DylintToolchainPlan> {
    let mut parts = line.splitn(3, '|');
    let channel = parts.next()?.to_string();
    let compiler_release = parts.next()?.to_string();
    let compiler_commit = parts.next()?.to_string();
    if parts.next().is_some() || channel.is_empty() {
        return None;
    }
    let identity = NightlyIdentity {
        rust_version: major_minor(&compiler_release).unwrap_or_default(),
        rustc_release: compiler_release.clone(),
        rustc_commit_hash: compiler_commit.clone(),
    };
    validate_identity(&channel, &identity).ok()?;
    Some(DylintToolchainPlan {
        channel,
        compiler_release,
        compiler_commit,
    })
}

/// Cheap on-disk sanity check standing in for a `rustup component list`
/// subprocess: does `rustup_home/toolchains/` contain a directory for
/// `channel`? Tries the exact `<channel>-<host triple>` name first (the
/// standard rustup toolchain directory naming); falls back to accepting
/// any directory whose name starts with `<channel>-` if the host triple
/// cannot be determined, so an unusual host doesn't spuriously miss a
/// perfectly good warm cache.
fn is_toolchain_installed_at(rustup_home: &Path, channel: &str) -> bool {
    let toolchains_dir = rustup_home.join("toolchains");
    if toolchains_dir.join(channel).is_dir() {
        return true;
    }
    if let Ok(triple) = TargetTriple::host() {
        if toolchains_dir
            .join(format!("{channel}-{}", triple.triple()))
            .is_dir()
        {
            return true;
        }
    }
    let Ok(entries) = std::fs::read_dir(&toolchains_dir) else {
        return false;
    };
    let prefix = format!("{channel}-");
    entries.filter_map(Result::ok).any(|entry| {
        entry.file_name().to_string_lossy().starts_with(&prefix) && entry.path().is_dir()
    })
}

/// Production entry point for persisting the warm-path marker after a
/// successful cold-path prepare. Best-effort by contract — callers must
/// not fail the run on a write error, only log it.
fn write_prepared_marker(version: &str, plan: &DylintToolchainPlan) -> Result<(), SoldrError> {
    let base_dir = SoldrPaths::new()?.root;
    write_prepared_marker_at(&base_dir, version, plan)
}

fn write_prepared_marker_at(
    base_dir: &Path,
    version: &str,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    let path = prepared_marker_path(base_dir, version);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", plan.cache_identity()))?;
    Ok(())
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

fn requested_rust_version(requested_channel: Option<&str>) -> Result<String, SoldrError> {
    if let Some(channel) = requested_channel {
        if let Some(version) = major_minor(channel) {
            return Ok(version);
        }
        let (release, _) = observe_compiler(channel)?;
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

fn requested_toolchain_channel(
    requested_channel: Option<&str>,
    workspace_root: &Path,
) -> Result<Option<String>, SoldrError> {
    let manifest = crate::core::read_rust_toolchain_manifest(workspace_root)?;
    Ok(requested_channel
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| non_empty_env("RUSTUP_TOOLCHAIN"))
        .or(manifest.channel))
}

fn is_dated_nightly(channel: &str) -> bool {
    let Some(value) = channel.strip_prefix("nightly-") else {
        return false;
    };
    let Some(date) = value.get(..10) else {
        return false;
    };
    let valid_date_shape = date.as_bytes().iter().enumerate().all(|(index, byte)| {
        matches!(index, 4 | 7) && *byte == b'-' || !matches!(index, 4 | 7) && byte.is_ascii_digit()
    });
    let suffix = &value[10..];
    valid_date_shape && (suffix.is_empty() || suffix.starts_with('-') && suffix.len() > 1)
}

fn select_explicit_from_map(
    bytes: &[u8],
    requested_channel: &str,
) -> Result<DylintToolchainPlan, SoldrError> {
    let map: NightlyVersionMap = serde_json::from_slice(bytes)
        .map_err(|error| SoldrError::Other(format!("failed to parse {MAP_ASSET}: {error}")))?;
    if map.schema_version != 1 {
        return Err(SoldrError::Other(format!(
            "{MAP_ASSET} has unsupported schema_version {}",
            map.schema_version
        )));
    }
    let map_channel = requested_channel
        .get(..18)
        .filter(|channel| is_dated_nightly(channel))
        .unwrap_or(requested_channel);
    let identity = map.nightlies.get(map_channel).ok_or_else(|| {
        SoldrError::Other(format!(
            "{MAP_ASSET} has no identity for explicitly configured {requested_channel}"
        ))
    })?;
    validate_identity(map_channel, identity)?;
    Ok(DylintToolchainPlan {
        channel: requested_channel.to_string(),
        compiler_release: identity.rustc_release.clone(),
        compiler_commit: identity.rustc_commit_hash.clone(),
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

    crate::timed_test!(explicit_nightly_uses_mapped_identity_without_installing, {
        let plan =
            select_explicit_from_map(&sample_map("nightly-2026-01-18"), "nightly-2026-01-18")
                .expect("explicit map entry");
        assert_eq!(plan.channel, "nightly-2026-01-18");
        assert_eq!(plan.compiler_commit, COMMIT);
    });

    crate::timed_test!(extracts_major_minor_versions, {
        assert_eq!(major_minor("1.94.1").as_deref(), Some("1.94"));
        assert_eq!(major_minor("1.94.0-nightly").as_deref(), Some("1.94"));
        assert_eq!(major_minor("stable"), None);
    });

    crate::timed_test!(recognizes_only_explicit_dated_nightlies, {
        assert!(is_dated_nightly("nightly-2026-04-16"));
        assert!(is_dated_nightly(
            "nightly-2026-04-16-x86_64-unknown-linux-gnu"
        ));
        assert!(!is_dated_nightly("nightly"));
        assert!(!is_dated_nightly("nightly-latest"));
        assert!(!is_dated_nightly("nightly-2026-04-16junk"));
        assert!(!is_dated_nightly("nightly-2026-04-16-"));
        assert!(!is_dated_nightly("nightly-2026-04"));
        assert!(!is_dated_nightly("1.97.0"));
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

    // -----------------------------------------------------------------
    // Warm-run prepared-plan marker (issue: dylint warm-run fast path).
    // -----------------------------------------------------------------

    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    /// Guards mutation of process-global env vars so these tests never
    /// race other tests in this binary that read the same keys. Mirrors
    /// the pattern in `toolchain.rs`'s test module.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn sample_plan() -> DylintToolchainPlan {
        DylintToolchainPlan {
            channel: "nightly-2026-01-18".to_string(),
            compiler_release: "1.94.0-nightly".to_string(),
            compiler_commit: COMMIT.to_string(),
        }
    }

    /// Creates `rustup_home/toolchains/<channel>-<anything>` so
    /// `is_toolchain_installed_at`'s prefix-scan fallback matches
    /// regardless of the actual host triple compiled into the test
    /// binary.
    fn stub_installed_toolchain(rustup_home: &Path, channel: &str) {
        let dir = rustup_home
            .join("toolchains")
            .join(format!("{channel}-stub-triple"));
        std::fs::create_dir_all(dir).expect("create stub toolchain dir");
    }

    crate::timed_test!(prepared_marker_roundtrip_hits_when_fresh_and_installed, {
        let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
        let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
        let plan = sample_plan();
        stub_installed_toolchain(rustup_home.path(), &plan.channel);

        write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

        let loaded = load_prepared_marker_from(
            soldr_root.path(),
            rustup_home.path(),
            "1.94",
            Duration::from_secs(60 * 60),
            SystemTime::now(),
        );
        assert_eq!(loaded, Some(plan));
    });

    crate::timed_test!(
        prepared_marker_accepts_fully_qualified_toolchain_directory,
        {
            let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
            let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
            let mut plan = sample_plan();
            plan.channel.push_str("-x86_64-unknown-linux-gnu");
            std::fs::create_dir_all(rustup_home.path().join("toolchains").join(&plan.channel))
                .expect("create fully-qualified toolchain dir");

            write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

            let loaded = load_prepared_marker_from(
                soldr_root.path(),
                rustup_home.path(),
                "1.94",
                Duration::from_secs(60 * 60),
                SystemTime::now(),
            );
            assert_eq!(loaded, Some(plan));
        }
    );

    crate::timed_test!(prepared_marker_rejected_when_ttl_expired, {
        let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
        let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
        let plan = sample_plan();
        stub_installed_toolchain(rustup_home.path(), &plan.channel);

        write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

        // "now" far in the future relative to the just-written marker's
        // mtime, well past a 1-second TTL.
        let far_future = SystemTime::now() + Duration::from_secs(3600);
        let loaded = load_prepared_marker_from(
            soldr_root.path(),
            rustup_home.path(),
            "1.94",
            Duration::from_secs(1),
            far_future,
        );
        assert_eq!(loaded, None);
    });

    crate::timed_test!(prepared_marker_ttl_zero_never_trusts_marker, {
        let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
        let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
        let plan = sample_plan();
        stub_installed_toolchain(rustup_home.path(), &plan.channel);

        write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

        let loaded = load_prepared_marker_from(
            soldr_root.path(),
            rustup_home.path(),
            "1.94",
            Duration::ZERO,
            SystemTime::now(),
        );
        assert_eq!(loaded, None);
    });

    crate::timed_test!(prepared_marker_rejected_when_malformed, {
        let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
        let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
        stub_installed_toolchain(rustup_home.path(), "nightly-2026-01-18");

        let path = prepared_marker_path(soldr_root.path(), "1.94");
        std::fs::create_dir_all(path.parent().unwrap()).expect("create marker dir");
        std::fs::write(&path, "not-a-valid-identity-line\n").expect("write malformed marker");

        let loaded = load_prepared_marker_from(
            soldr_root.path(),
            rustup_home.path(),
            "1.94",
            Duration::from_secs(60 * 60),
            SystemTime::now(),
        );
        assert_eq!(loaded, None);
    });

    crate::timed_test!(prepared_marker_rejected_when_toolchain_dir_missing, {
        let soldr_root = tempfile::tempdir().expect("soldr root tempdir");
        // No stubbed toolchain directory under this rustup_home.
        let rustup_home = tempfile::tempdir().expect("rustup home tempdir");
        let plan = sample_plan();

        write_prepared_marker_at(soldr_root.path(), "1.94", &plan).expect("write marker");

        let loaded = load_prepared_marker_from(
            soldr_root.path(),
            rustup_home.path(),
            "1.94",
            Duration::from_secs(60 * 60),
            SystemTime::now(),
        );
        assert_eq!(loaded, None);
    });

    crate::timed_test!(parse_marker_identity_roundtrips_cache_identity_format, {
        let plan = sample_plan();
        let parsed = parse_marker_identity(&plan.cache_identity()).expect("parse identity line");
        assert_eq!(parsed, plan);
    });

    crate::timed_test!(parse_marker_identity_rejects_malformed_lines, {
        assert!(parse_marker_identity("garbage").is_none());
        assert!(parse_marker_identity("nightly-2026-01-18|1.94.0-nightly|short").is_none());
        assert!(parse_marker_identity("not-nightly|1.94.0-nightly|").is_none());
    });

    crate::timed_test!(sanitize_marker_key_strips_path_hostile_characters, {
        assert_eq!(sanitize_marker_key("1.94"), "1.94");
        assert_eq!(
            sanitize_marker_key("nightly-2026-01-18"),
            "nightly-2026-01-18"
        );
        assert_eq!(sanitize_marker_key("a/b\\c:d"), "a_b_c_d");
    });

    crate::timed_test!(truthy_env_bypasses_marker_lookup, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        {
            let _env = EnvVarGuard::set(REVERIFY_ENV_VAR, "1");
            assert!(truthy_env(REVERIFY_ENV_VAR));
        }
        {
            let _env = EnvVarGuard::set(REVERIFY_ENV_VAR, "true");
            assert!(truthy_env(REVERIFY_ENV_VAR));
        }
        {
            let _env = EnvVarGuard::set(REVERIFY_ENV_VAR, "0");
            assert!(!truthy_env(REVERIFY_ENV_VAR));
        }
        assert!(!truthy_env(REVERIFY_ENV_VAR));
    });

    crate::timed_test!(prepare_ttl_parses_env_override_and_falls_back_to_default, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        {
            let _env = EnvVarGuard::set(PREPARE_TTL_ENV_VAR, "60");
            assert_eq!(prepare_ttl(), Duration::from_secs(60));
        }
        {
            let _env = EnvVarGuard::set(PREPARE_TTL_ENV_VAR, "0");
            assert_eq!(prepare_ttl(), Duration::ZERO);
        }
        {
            let _env = EnvVarGuard::set(PREPARE_TTL_ENV_VAR, "not-a-number");
            assert_eq!(prepare_ttl(), DEFAULT_PREPARE_TTL);
        }
        assert_eq!(prepare_ttl(), DEFAULT_PREPARE_TTL);
    });

    // -----------------------------------------------------------------
    // Regression guard: DylintToolchainPlan::apply_to_command must only
    // stamp the dylint-scoped identity env vars (plus the best-effort
    // DYLINT_DRIVER_PATH) and must NEVER switch the analyzed
    // workspace's cargo build profile. A sibling repo shipped exactly
    // this bug once — soldr injected a profile override inside a
    // dylint run and silently changed what got built/analyzed.
    // -----------------------------------------------------------------
    crate::timed_test!(
        apply_to_command_never_touches_build_profile_or_injects_args,
        {
            let plan = sample_plan();
            let mut command = std::process::Command::new("does-not-matter");
            plan.apply_to_command(&mut command);

            let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> =
                command.get_envs().collect();

            let expected_keys = [
                "RUSTUP_TOOLCHAIN",
                TOOLCHAIN_ENV_VAR,
                COMPILER_RELEASE_ENV_VAR,
                COMPILER_COMMIT_ENV_VAR,
                CACHE_IDENTITY_ENV_VAR,
                PREPARED_IDENTITY_ENV_VAR,
            ];
            for key in expected_keys {
                assert!(
                    envs.contains_key(OsStr::new(key)),
                    "apply_to_command must set {key}"
                );
            }

            for key in envs.keys() {
                let key_str = key.to_string_lossy();
                assert!(
                    !key_str.starts_with("CARGO_PROFILE_RELEASE_")
                        && !key_str.starts_with("CARGO_BUILD_")
                        && key_str != "PROFILE",
                    "dylint scope stamping must never switch the analyzed workspace's \
                 build profile, but set: {key_str}"
                );
                // DYLINT_DRIVER_PATH is the one soldr-owned addition beyond
                // the identity env vars (best-effort; may be absent if
                // SoldrPaths::new() can't resolve in this environment).
                assert!(
                    expected_keys.contains(&key_str.as_ref()) || key_str == "DYLINT_DRIVER_PATH",
                    "unexpected env var set by DylintToolchainPlan::apply_to_command: {key_str}"
                );
            }

            // A profile switch could also arrive as an injected CLI arg
            // (`--release` / `--profile <name>`); apply_to_command must
            // never add args to the command at all.
            assert_eq!(
                command.get_args().count(),
                0,
                "apply_to_command must not inject any CLI args (e.g. --release/--profile)"
            );
        }
    );
}
