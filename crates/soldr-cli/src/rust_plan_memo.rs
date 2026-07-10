//! Prep memo for the rust-plan target-cache preparation phase (soldr#1540).
//!
//! `maybe_prepare_rust_artifact_plan` historically shelled out to three
//! subprocesses on every `soldr cargo ...` invocation with the target cache
//! enabled: `cargo metadata --format-version 1`, `rustc -Vv`, and
//! `cargo --version` (measured at ~0.6-0.9 s combined on the `medium`
//! fixture inside the Linux perf container). Their outputs only change when
//! a well-defined set of inputs changes, so this module persists them in a
//! versioned protobuf memo (schema: `rust_plan_memo.proto`) keyed by a
//! content-identity hash over every semantic input:
//!
//! - the discovered manifest anchor plus any `Cargo.toml` in a strict
//!   ancestor directory (parent-workspace injection/removal),
//! - every workspace manifest hash (membership globs, path-dep edits),
//! - manifests of path dependencies living *outside* the workspace root,
//! - `Cargo.lock` and every discoverable `.cargo/config{,.toml}` from the
//!   cwd upwards plus `$CARGO_HOME`,
//! - the `cargo metadata` passthrough args (`--manifest-path`, `--config`,
//!   `--features`, `--locked`, ...),
//! - toolchain binary identity (resolved path + file size + mtime) for the
//!   exact cargo/rustc binaries the prep phase would spawn, the nearest
//!   `rust-toolchain{,.toml}` pin, and rustup `settings.toml`,
//! - the environment variables that steer cargo metadata / rustup shim
//!   dispatch (`CARGO_HOME`, `CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR`,
//!   `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`) and the working directory.
//!
//! Correctness posture: a memo is only used when the freshly recomputed key
//! matches the stored `key_hash` exactly. Any discovery error, decode error,
//! schema mismatch, or missing input makes the caller fall back to the
//! authoritative subprocesses (issue #1540 hard gate). The memo never feeds
//! Cargo freshness decisions — it only skips re-running probe subprocesses
//! whose inputs are provably unchanged. `SOLDR_RUST_PLAN_MEMO=off` disables
//! the memo entirely.

use prost::Message as _;
use serde::Serialize;

use crate::cargo_front_door::{
    cargo_config_hash, file_hash_or_missing, sha256_bytes, stable_hash_json,
    workspace_manifest_hashes,
};
use crate::core::SoldrError;
use crate::zccache::normalize_path_for_compare;

use super::{cargo_metadata_passthrough_args, CargoMetadata, CargoMetadataPackage};

pub(super) const PREP_MEMO_SCHEMA_VERSION: u32 = 1;

/// Env-var gate: any of `off` / `0` / `false` / `no` disables the memo.
pub(super) const RUST_PLAN_MEMO_ENV_VAR: &str = "SOLDR_RUST_PLAN_MEMO";

pub mod wire {
    use prost::Message;

    /// Mirror of `rust_plan_memo.proto` `RustPlanPrepMemoV1`.
    #[derive(Clone, PartialEq, Message)]
    pub struct RustPlanPrepMemoV1 {
        #[prost(uint32, tag = "1")]
        pub schema_version: u32,
        #[prost(string, tag = "2")]
        pub key_hash: String,
        #[prost(string, tag = "3")]
        pub rustc_verbose_version: String,
        #[prost(string, tag = "4")]
        pub cargo_version_line: String,
        #[prost(string, tag = "5")]
        pub workspace_root: String,
        #[prost(string, tag = "6")]
        pub target_directory: String,
        #[prost(string, repeated, tag = "7")]
        pub workspace_members: Vec<String>,
        #[prost(message, repeated, tag = "8")]
        pub packages: Vec<PrepMemoPackage>,
    }

    /// Mirror of `rust_plan_memo.proto` `PrepMemoPackage`.
    #[derive(Clone, PartialEq, Message)]
    pub struct PrepMemoPackage {
        #[prost(string, tag = "1")]
        pub id: String,
        #[prost(string, tag = "2")]
        pub source: String,
        #[prost(bool, tag = "3")]
        pub has_source: bool,
        #[prost(string, tag = "4")]
        pub manifest_path: String,
    }
}

/// Raw stdout of the two toolchain probe subprocesses. Stored verbatim in
/// the memo; the channel/host parsing happens live at plan-build time so
/// `RUSTUP_TOOLCHAIN` env influence stays out of the persisted payload.
pub(crate) struct ToolchainProbe {
    pub(crate) rustc_verbose: String,
    pub(crate) cargo_version: String,
}

/// The three file-hash families `build_rust_artifact_plan` feeds into the
/// plan inputs. Computed once per invocation and shared with memo-key
/// validation so a hit performs zero extra manifest walks.
pub(crate) struct WorkspaceFileHashes {
    pub(crate) lockfile_hash: String,
    pub(crate) cargo_config_hash: String,
    pub(crate) manifest_hashes: Vec<String>,
}

impl WorkspaceFileHashes {
    pub(crate) fn collect(workspace_root: &std::path::Path) -> Result<Self, SoldrError> {
        Ok(Self {
            lockfile_hash: file_hash_or_missing(&workspace_root.join("Cargo.lock"))?,
            cargo_config_hash: cargo_config_hash(workspace_root)?,
            manifest_hashes: workspace_manifest_hashes(workspace_root)?,
        })
    }
}

pub(super) fn prep_memo_enabled() -> bool {
    let raw = std::env::var(RUST_PLAN_MEMO_ENV_VAR).unwrap_or_default();
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "off" | "0" | "false" | "no"
    )
}

/// Identity of one toolchain binary: resolved path + size + mtime. The
/// mtime is only ever *read* as a cache key (the sccache pattern); soldr
/// never writes or fabricates timestamps.
#[derive(Serialize)]
struct BinaryIdentity {
    path: String,
    len: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
}

fn binary_identity(path: &std::path::Path) -> Result<BinaryIdentity, SoldrError> {
    let resolved = which_like_resolve(path);
    let metadata = std::fs::metadata(&resolved)?;
    let mtime = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok(BinaryIdentity {
        path: resolved.display().to_string(),
        len: metadata.len(),
        mtime_secs: mtime.as_secs(),
        mtime_nanos: mtime.subsec_nanos(),
    })
}

/// Bare tool names (no separator) would be resolved via `PATH` by the OS
/// at spawn time; mirror that so the identity reflects the binary that
/// actually runs. Failure to resolve leaves the name as-is and the
/// subsequent `fs::metadata` error makes the memo ineligible.
fn which_like_resolve(path: &std::path::Path) -> std::path::PathBuf {
    if path.components().count() > 1 {
        return path.to_path_buf();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return path.to_path_buf();
    };
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(path);
        if candidate.is_file() {
            return candidate;
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{}.exe", path.display()));
            if with_exe.is_file() {
                return with_exe;
            }
        }
    }
    path.to_path_buf()
}

/// Snapshot of the environment inputs that steer `cargo metadata` output or
/// rustup shim dispatch. Captured once; injectable in tests.
#[derive(Serialize, Clone)]
pub(super) struct MemoEnvSnapshot {
    pub(super) cwd: String,
    pub(super) cargo_home: Option<String>,
    pub(super) cargo_target_dir: Option<String>,
    pub(super) cargo_build_target_dir: Option<String>,
    pub(super) rustup_home: Option<String>,
    pub(super) rustup_toolchain: Option<String>,
}

impl MemoEnvSnapshot {
    fn capture() -> Result<Self, SoldrError> {
        let cwd = std::env::current_dir()?;
        Ok(Self {
            cwd: cwd.display().to_string(),
            cargo_home: std::env::var("CARGO_HOME").ok(),
            cargo_target_dir: std::env::var("CARGO_TARGET_DIR").ok(),
            cargo_build_target_dir: std::env::var("CARGO_BUILD_TARGET_DIR").ok(),
            rustup_home: std::env::var("RUSTUP_HOME").ok(),
            rustup_toolchain: std::env::var("RUSTUP_TOOLCHAIN").ok(),
        })
    }
}

/// Everything the memo key needs that does *not* depend on the (memoized)
/// workspace root. Gathered up-front; any failure disables the memo for
/// this invocation and the authoritative subprocesses run.
pub(super) struct MemoContext {
    anchor_manifest: String,
    ancestor_manifest_hashes: Vec<String>,
    passthrough_args: Vec<String>,
    env: MemoEnvSnapshot,
    cargo_binary: BinaryIdentity,
    rustc_binary: BinaryIdentity,
    rust_toolchain_file: String,
    rustup_settings_hashes: Vec<String>,
    config_hashes: Vec<String>,
}

/// Canonical JSON shape hashed into `key_hash`. Field order is fixed by
/// the struct definition; every field participates in the hash.
#[derive(Serialize)]
struct PrepMemoKeyInputs<'a> {
    schema_version: u32,
    anchor_manifest: &'a str,
    ancestor_manifest_hashes: &'a [String],
    passthrough_args: &'a [String],
    env: &'a MemoEnvSnapshot,
    cargo_binary: &'a BinaryIdentity,
    rustc_binary: &'a BinaryIdentity,
    rust_toolchain_file: &'a str,
    rustup_settings_hashes: &'a [String],
    config_hashes: &'a [String],
    workspace_root: &'a str,
    manifest_hashes: &'a [String],
    lockfile_hash: &'a str,
    external_manifest_hashes: &'a [String],
}

impl MemoContext {
    pub(super) fn gather(
        cargo: &std::path::Path,
        rustc: &std::path::Path,
        args: &[String],
    ) -> Result<Self, SoldrError> {
        Self::gather_with_env(MemoEnvSnapshot::capture()?, cargo, rustc, args)
    }

    /// Same as [`MemoContext::gather`] with an injected environment
    /// snapshot; the seam unit tests use to exercise env-keyed
    /// invalidation without racing on process-global env vars.
    pub(super) fn gather_with_env(
        env: MemoEnvSnapshot,
        cargo: &std::path::Path,
        rustc: &std::path::Path,
        args: &[String],
    ) -> Result<Self, SoldrError> {
        let passthrough_args = cargo_metadata_passthrough_args(args)
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        let cwd = std::path::PathBuf::from(&env.cwd);
        let anchor = discovered_manifest(&passthrough_args, &cwd).ok_or_else(|| {
            SoldrError::Other("rust-plan memo: no Cargo.toml discovered from cwd".to_string())
        })?;
        let anchor = normalize_path_for_compare(&anchor)?;
        let anchor_dir = anchor
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| anchor.clone());

        Ok(Self {
            anchor_manifest: anchor.display().to_string(),
            ancestor_manifest_hashes: ancestor_manifest_hashes(&anchor_dir)?,
            passthrough_args,
            cargo_binary: binary_identity(cargo)?,
            rustc_binary: binary_identity(rustc)?,
            rust_toolchain_file: nearest_rust_toolchain_hash(&cwd)?,
            rustup_settings_hashes: rustup_settings_hashes(&env, &cwd)?,
            config_hashes: hierarchical_config_hashes(&env, &cwd)?,
            env,
        })
    }

    /// Memo file path. The anchor + passthrough args participate in the
    /// filename so alternating configurations of the same workspace (for
    /// example `--all-features` on/off) keep separate memos instead of
    /// evicting each other.
    pub(super) fn memo_path(&self, plan_dir: &std::path::Path) -> std::path::PathBuf {
        let discriminator = sha256_bytes(
            format!(
                "{}\n{}",
                self.anchor_manifest,
                self.passthrough_args.join("\n")
            )
            .as_bytes(),
        );
        plan_dir.join(format!("prep-memo-{}.pb", &discriminator[..16]))
    }

    fn key_hash(
        &self,
        workspace_root: &str,
        hashes: &WorkspaceFileHashes,
        external_manifest_hashes: &[String],
    ) -> String {
        stable_hash_json(&PrepMemoKeyInputs {
            schema_version: PREP_MEMO_SCHEMA_VERSION,
            anchor_manifest: &self.anchor_manifest,
            ancestor_manifest_hashes: &self.ancestor_manifest_hashes,
            passthrough_args: &self.passthrough_args,
            env: &self.env,
            cargo_binary: &self.cargo_binary,
            rustc_binary: &self.rustc_binary,
            rust_toolchain_file: &self.rust_toolchain_file,
            rustup_settings_hashes: &self.rustup_settings_hashes,
            config_hashes: &self.config_hashes,
            workspace_root,
            manifest_hashes: &hashes.manifest_hashes,
            lockfile_hash: &hashes.lockfile_hash,
            external_manifest_hashes,
        })
    }

    /// Attempt to serve the prep outputs from the on-disk memo. Returns
    /// `None` (fall back to the authoritative subprocesses) on any decode
    /// error, schema mismatch, discovery error, or key mismatch.
    pub(super) fn try_load(
        &self,
        plan_dir: &std::path::Path,
    ) -> Option<(CargoMetadata, ToolchainProbe, WorkspaceFileHashes)> {
        let bytes = std::fs::read(self.memo_path(plan_dir)).ok()?;
        let memo = wire::RustPlanPrepMemoV1::decode(bytes.as_slice()).ok()?;
        if memo.schema_version != PREP_MEMO_SCHEMA_VERSION {
            return None;
        }
        let workspace_root = std::path::PathBuf::from(&memo.workspace_root);
        let hashes = WorkspaceFileHashes::collect(&workspace_root).ok()?;
        let external =
            external_manifest_hashes_from_wire(&memo.packages, &memo.workspace_root).ok()?;
        if self.key_hash(&memo.workspace_root, &hashes, &external) != memo.key_hash {
            return None;
        }
        let metadata = CargoMetadata {
            packages: memo
                .packages
                .into_iter()
                .map(|package| CargoMetadataPackage {
                    id: package.id,
                    source: package.has_source.then_some(package.source),
                    manifest_path: Some(package.manifest_path),
                })
                .collect(),
            workspace_members: memo.workspace_members,
            workspace_root,
            target_directory: std::path::PathBuf::from(memo.target_directory),
        };
        let probe = ToolchainProbe {
            rustc_verbose: memo.rustc_verbose_version,
            cargo_version: memo.cargo_version_line,
        };
        Some((metadata, probe, hashes))
    }

    /// Persist the memo after an authoritative run. Best-effort: the caller
    /// ignores the result so a read-only cache dir can never fail a build.
    pub(super) fn store(
        &self,
        plan_dir: &std::path::Path,
        metadata: &CargoMetadata,
        probe: &ToolchainProbe,
        workspace_root: &str,
        hashes: &WorkspaceFileHashes,
    ) -> Result<(), SoldrError> {
        let packages = metadata
            .packages
            .iter()
            .map(|package| wire::PrepMemoPackage {
                id: package.id.clone(),
                source: package.source.clone().unwrap_or_default(),
                has_source: package.source.is_some(),
                manifest_path: package.manifest_path.clone().unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        let external = external_manifest_hashes_from_wire(&packages, workspace_root)?;
        let memo = wire::RustPlanPrepMemoV1 {
            schema_version: PREP_MEMO_SCHEMA_VERSION,
            key_hash: self.key_hash(workspace_root, hashes, &external),
            rustc_verbose_version: probe.rustc_verbose.clone(),
            cargo_version_line: probe.cargo_version.clone(),
            workspace_root: workspace_root.to_string(),
            target_directory: metadata.target_directory.display().to_string(),
            workspace_members: metadata.workspace_members.clone(),
            packages,
        };
        let mut bytes = Vec::with_capacity(memo.encoded_len());
        memo.encode(&mut bytes)
            .map_err(|err| SoldrError::Other(format!("failed to encode rust-plan memo: {err}")))?;
        std::fs::create_dir_all(plan_dir)?;
        let path = self.memo_path(plan_dir);
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path).map_err(|err| {
            let _ = std::fs::remove_file(&tmp);
            SoldrError::from(err)
        })?;
        Ok(())
    }
}

/// The manifest `cargo metadata` will anchor workspace discovery on:
/// an explicit `--manifest-path` passthrough wins, otherwise the nearest
/// `Cargo.toml` walking up from the cwd (cargo's own discovery rule).
fn discovered_manifest(
    passthrough_args: &[String],
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut iter = passthrough_args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--manifest-path" {
            return iter.next().map(std::path::PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(std::path::PathBuf::from(value));
        }
    }
    crate::cargo_metadata_soldr::find_cargo_toml(cwd)
}

/// Hash any `Cargo.toml` in a strict ancestor directory of the discovered
/// manifest. Creating or deleting a parent-workspace manifest changes the
/// true workspace root without touching anything under the old root, so it
/// must flip the key.
fn ancestor_manifest_hashes(anchor_dir: &std::path::Path) -> Result<Vec<String>, SoldrError> {
    let mut hashes = Vec::new();
    let mut current = anchor_dir.parent();
    while let Some(dir) = current {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            hashes.push(format!(
                "{}:{}",
                candidate.display(),
                file_hash_or_missing(&candidate)?
            ));
        }
        current = dir.parent();
    }
    Ok(hashes)
}

/// Nearest `rust-toolchain.toml` / `rust-toolchain` walking up from the
/// cwd — the file rustup shims consult for channel dispatch.
fn nearest_rust_toolchain_hash(cwd: &std::path::Path) -> Result<String, SoldrError> {
    let mut current = Some(cwd);
    while let Some(dir) = current {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(format!(
                    "{}:{}",
                    candidate.display(),
                    file_hash_or_missing(&candidate)?
                ));
            }
        }
        current = dir.parent();
    }
    Ok("missing".to_string())
}

/// rustup `settings.toml` (default-toolchain flips) from every plausible
/// rustup home: `$RUSTUP_HOME`, the nearest ancestor `.rustup`, and the
/// user-default `~/.rustup`. Cheap stats; covers the case where the
/// resolved cargo/rustc path is a rustup shim whose bytes do not change
/// when `rustup default` flips.
fn rustup_settings_hashes(
    env: &MemoEnvSnapshot,
    cwd: &std::path::Path,
) -> Result<Vec<String>, SoldrError> {
    let mut candidates = Vec::new();
    if let Some(home) = &env.rustup_home {
        candidates.push(std::path::PathBuf::from(home));
    }
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let candidate = dir.join(".rustup");
        if candidate.is_dir() {
            candidates.push(candidate);
            break;
        }
        current = dir.parent();
    }
    if let Ok(user_home) = crate::core::home_dir() {
        candidates.push(user_home.join(".rustup"));
    }
    let mut hashes = Vec::new();
    for home in candidates {
        let settings = home.join("settings.toml");
        if settings.is_file() {
            hashes.push(format!(
                "{}:{}",
                settings.display(),
                file_hash_or_missing(&settings)?
            ));
        }
    }
    hashes.sort();
    hashes.dedup();
    Ok(hashes)
}

/// Every `.cargo/config.toml` / `.cargo/config` cargo would merge for a
/// process running in `cwd` (cwd upwards), plus `$CARGO_HOME/config*`.
fn hierarchical_config_hashes(
    env: &MemoEnvSnapshot,
    cwd: &std::path::Path,
) -> Result<Vec<String>, SoldrError> {
    let mut hashes = Vec::new();
    let mut push_config_dir = |dir: &std::path::Path| -> Result<(), SoldrError> {
        for name in ["config.toml", "config"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                hashes.push(format!(
                    "{}:{}",
                    candidate.display(),
                    file_hash_or_missing(&candidate)?
                ));
            }
        }
        Ok(())
    };
    let mut current = Some(cwd);
    while let Some(dir) = current {
        push_config_dir(&dir.join(".cargo"))?;
        current = dir.parent();
    }
    if let Some(home) = &env.cargo_home {
        push_config_dir(std::path::Path::new(home))?;
    }
    hashes.sort();
    hashes.dedup();
    Ok(hashes)
}

/// Hash the manifests of path dependencies that live outside the workspace
/// root (workspace-internal manifests are already covered by the manifest
/// walk). Registry/git packages are content-addressed and skipped.
fn external_manifest_hashes_from_wire(
    packages: &[wire::PrepMemoPackage],
    workspace_root: &str,
) -> Result<Vec<String>, SoldrError> {
    let root = std::path::Path::new(workspace_root);
    let mut hashes = Vec::new();
    for package in packages {
        if package.has_source || package.manifest_path.is_empty() {
            continue;
        }
        let manifest = std::path::Path::new(&package.manifest_path);
        if manifest.starts_with(root) {
            continue;
        }
        hashes.push(format!(
            "{}:{}",
            package.manifest_path,
            file_hash_or_missing(manifest)?
        ));
    }
    hashes.sort();
    hashes.dedup();
    Ok(hashes)
}
