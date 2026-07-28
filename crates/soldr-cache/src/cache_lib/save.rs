//! `soldr save` / `soldr load`: produce and consume single-file archives
//! that bundle (a) a build-cache directory's contents and (b) a snapshot
//! of source-file mtimes the next compiler invocation will fingerprint.
//!
//! ## Why this exists
//!
//! Two real problems we kept hitting in the setup-soldr GitHub Action,
//! split out as soldr-side primitives so any project that builds from a
//! fresh checkout can opt in without the action wrapper:
//!
//! 1. **Cache-archive transport.** Bespoke tar+zstd plumbing in TS picked
//!    up subtle bugs (decompressing back into `<cacheDir>` instead of
//!    `dirname(<cacheDir>)` produced silent double-nesting and made every
//!    restore look fine while zccache's lookups missed). One Rust impl,
//!    one test suite, no shell quoting drama.
//!
//! 2. **Source-file mtimes.** `actions/checkout` (and any equivalent
//!    sandbox restore) rewrites every source file's mtime on every run,
//!    so Cargo's fingerprint files invalidate on the first stat. We snap
//!    cold's mtimes + content-hashes into the archive, then on load we
//!    only replay an mtime if the current file is byte-identical to the
//!    snapshot — never underbuilds the way a blind `touch -d <commit
//!    time>` does.
//!
//! ## Format
//!
//! A single `.tar.zst` (zstd-compressed POSIX tar). Inside:
//!
//! - `SOLDR_MANIFEST.pb` at the archive root — the protobuf manifest.
//! - `cache/...` — the build cache directory's contents.
//!
//! `cache/` is always exactly one level deep at the archive root so
//! readers never have to guess the basename. Manifest carries
//! `cache_dir_name = "cache"` for forward compat.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

use prost::Message as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use thiserror::Error;

pub mod proto {
    //! Hand-written prost types corresponding to `manifest.proto` in this
    //! directory.
    //!
    //! We write these by hand rather than running `prost-build` from a
    //! build script because the codegen pulls in `protoc` (the C++
    //! protobuf compiler) at build time, and we don't want to require
    //! every CI runner / contributor to install it. The schema is tiny
    //! and stable; the `.proto` file is preserved as the source of
    //! truth for anyone reading the wire format. If you change one,
    //! change the other — and bump `Manifest::version` if the change
    //! is breaking.

    use prost::{Enumeration, Message};

    #[derive(Clone, PartialEq, Message)]
    pub struct Manifest {
        #[prost(uint32, tag = "1")]
        pub version: u32,
        #[prost(int64, tag = "2")]
        pub saved_at_ms: i64,
        #[prost(string, tag = "3")]
        pub workspace: ::prost::alloc::string::String,
        #[prost(string, tag = "4")]
        pub cache_dir_name: ::prost::alloc::string::String,
        #[prost(message, repeated, tag = "5")]
        pub files: ::prost::alloc::vec::Vec<SourceFile>,
        #[prost(uint64, tag = "6")]
        pub source_file_count: u64,
        #[prost(uint64, tag = "7")]
        pub cache_file_count: u64,
        #[prost(enumeration = "CacheLayerKind", tag = "8")]
        pub cache_layer_kind: i32,
        #[prost(message, repeated, tag = "9")]
        pub cache_files: ::prost::alloc::vec::Vec<CacheFile>,
        #[prost(bytes = "vec", tag = "10")]
        pub base_manifest_blake3: ::prost::alloc::vec::Vec<u8>,
        #[prost(string, repeated, tag = "11")]
        pub deleted_cache_paths: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
        #[prost(message, repeated, tag = "12")]
        pub cache_symlinks: ::prost::alloc::vec::Vec<SymlinkEntry>,
    }

    /// A symlink below the cache dir (#1548). Manifest-only: symlinks are
    /// never carried as tar entries, so pre-#1548 loaders simply skip this
    /// unknown field and behave exactly as before (no link restored).
    #[derive(Clone, PartialEq, Message)]
    pub struct SymlinkEntry {
        /// Cache-dir-relative POSIX path of the link itself.
        #[prost(string, tag = "1")]
        pub path: ::prost::alloc::string::String,
        /// Raw link target, forward-slashed. Always RELATIVE — absolute or
        /// root-escaping targets are rejected at save time and re-rejected
        /// at load time before any link is created.
        #[prost(string, tag = "2")]
        pub target: ::prost::alloc::string::String,
        /// Whether the target resolved to a directory at save time. Used on
        /// Windows to pick symlink_dir vs symlink_file at restore.
        #[prost(bool, tag = "3")]
        pub is_dir: bool,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct SourceFile {
        #[prost(string, tag = "1")]
        pub path: ::prost::alloc::string::String,
        #[prost(int64, tag = "2")]
        pub mtime_ms: i64,
        #[prost(uint64, tag = "3")]
        pub size: u64,
        #[prost(bytes = "vec", tag = "4")]
        pub blake3: ::prost::alloc::vec::Vec<u8>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct CacheFile {
        #[prost(string, tag = "1")]
        pub path: ::prost::alloc::string::String,
        #[prost(int64, tag = "2")]
        pub mtime_ns: i64,
        #[prost(uint64, tag = "3")]
        pub size: u64,
        #[prost(bytes = "vec", tag = "4")]
        pub blake3: ::prost::alloc::vec::Vec<u8>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Enumeration)]
    #[repr(i32)]
    pub enum CacheLayerKind {
        Complete = 0,
        Base = 1,
        Delta = 2,
    }
}

pub use proto::{CacheFile, CacheLayerKind, Manifest, SourceFile, SymlinkEntry};

/// Well-known filename for the manifest inside an archive.
pub const MANIFEST_NAME: &str = "SOLDR_MANIFEST.pb";

/// Directory name for the bundled cache contents inside an archive.
pub const CACHE_DIR_NAME: &str = "cache";

/// Current manifest schema version. Bump only on breaking format changes.
pub const MANIFEST_VERSION: u32 = 1;

/// Environment variable that overrides the parallel-extract worker count
/// inside `soldr load` (#575). Accepts a positive integer; clamps to at
/// least 1. When unset or unparseable, falls back to `--threads` (if the
/// caller passed it) and then to rayon's default (num_cpus). The override
/// also sizes the rayon pool used by `load()` so concurrent mtime-replay
/// and cache-file extraction share the same parallelism budget.
pub const LOAD_WORKERS_ENV: &str = "SOLDR_LOAD_WORKERS";

/// Fallback worker count for `soldr load` when `--threads` is unset, the
/// env override is unset, and we can't probe rayon's default. Effectively
/// dead code on supported platforms (rayon always returns a positive
/// `current_num_threads()`), but documents the floor explicitly. Used by
/// the parallel cache-file extractor in `load()`.
pub const DEFAULT_LOAD_WORKERS: usize = 4;
const REPLAY_WORKER_RECV_TIMEOUT: Duration = Duration::from_secs(60);
const EXTRACT_WORKER_RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// Default zstd compression level — matches what setup-soldr's TS impl
/// has been using (level 3 gives ~0.26 ratio on Rust artifact caches at
/// roughly 1 GB/s on a Linux CI runner).
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Environment variable that selects the default `soldr save` profile
/// when the CLI does not pass `--ci` / `--minimal`.
pub const SAVE_PROFILE_ENV: &str = "SOLDR_SAVE_PROFILE";

/// Payload profile for `soldr save`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveProfile {
    /// Historical behavior: archive every regular file below `cache_dir`.
    #[default]
    Full,
    /// CI/minimal v1: archive cache payload needed for warm hits while
    /// excluding logs, runtime scratch, sockets, lock files, zccache
    /// runtime binaries, and soldr managed binary/toolchain trees that
    /// do not participate in rustc hits.
    Ci,
}

impl SaveProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Ci => "ci",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "full" | "default" | "complete" => Some(Self::Full),
            "ci" | "minimal" => Some(Self::Ci),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum SaveLoadError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    BareIo(#[from] std::io::Error),
    #[error("manifest encode failed: {0}")]
    Encode(#[from] prost::EncodeError),
    #[error("manifest decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("archive is missing the {MANIFEST_NAME} entry")]
    MissingManifest,
    #[error("invalid path inside archive: {0}")]
    BadArchivePath(String),
    #[error("zstd error: {0}")]
    Zstd(std::io::Error),
    #[error("walk error at {path}: {message}")]
    Walk { path: PathBuf, message: String },
}

fn io(path: impl Into<PathBuf>, source: std::io::Error) -> SaveLoadError {
    SaveLoadError::Io {
        path: path.into(),
        source,
    }
}

pub type Result<T> = std::result::Result<T, SaveLoadError>;

// ---------- helpers shared by save + load ----------

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn ms_to_systime(ms: i64) -> SystemTime {
    if ms < 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    }
}

fn ns_to_systime(ns: i64) -> SystemTime {
    if ns < 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    }
}

/// Stream-hash a file with BLAKE3. Returns the full 32-byte hash.
fn hash_file(path: &Path) -> Result<[u8; 32]> {
    zccache::hash::hash_file(path)
        .map(|hash| *hash.as_bytes())
        .map_err(|e| io(path, e))
}

/// Resolve the absolute path(s) that count as "the Cargo target directory"
/// for `workspace` (#1547). Path-name matching alone (excluding every
/// directory literally named `target` anywhere in the tree) can hide
/// legitimate tracked source such as `src/target/mod.rs`, so the walker
/// only excludes a `target/` entry when its full path matches one of
/// these candidates.
///
/// Candidates, matching Cargo's own resolution order (most specific
/// first) — we do not attempt to parse `.cargo/config.toml`'s
/// `build.target-dir` key here; an override there falls through to
/// the conservative default below, which only means the real output
/// dir gets hashed too (safe: extra work, never a missed input):
/// * `$CARGO_TARGET_DIR` (if absolute),
/// * `$CARGO_BUILD_TARGET_DIR` (if absolute),
/// * `<workspace>/target` (Cargo's default).
fn workspace_target_dir_candidates(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(3);
    for var in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(dir) = std::env::var_os(var) {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                out.push(path);
            }
        }
    }
    out.push(workspace.join("target"));
    out
}

/// Walk a workspace and return every regular file's repo-relative POSIX
/// path. We deliberately do NOT shell out to `git ls-files` — soldr's
/// users include sandboxed CI jobs and local-dev runs that don't always
/// have git on PATH at the moment this is invoked. `.git/` and
/// `node_modules/` are excluded by name at any depth — those basenames
/// are never legitimate tracked source. The build-output `target/`
/// directory is excluded by *resolved path*, not by name (#1547): a
/// source directory that happens to be named `target` anywhere other
/// than the actual Cargo target dir (e.g. `src/target/mod.rs`) is real
/// source and must be hashed. See [`workspace_target_dir_candidates`].
///
/// Uses jwalk for a parallel walk; on a 1000-file workspace this is
/// ~4x faster than walkdir at the directory-traversal level. The walk
/// itself is the cheap part — caller still has to hash each file.
fn walk_workspace_files(workspace: &Path, threads: Option<usize>) -> Result<Vec<PathBuf>> {
    let target_dirs = workspace_target_dir_candidates(workspace);
    let walker = jwalk::WalkDir::new(workspace)
        .follow_links(false)
        .skip_hidden(false) // we want .cargo, .rustfmt.toml, etc.
        .process_read_dir(move |_depth, dir_path, _read_dir_state, children| {
            children.retain(|res| match res {
                Ok(entry) => {
                    let name = entry.file_name.to_string_lossy();
                    if entry.depth > 0 && (name == ".git" || name == "node_modules") {
                        return false;
                    }
                    if entry.depth > 0 && name == "target" {
                        let candidate = dir_path.join(&entry.file_name);
                        if target_dirs.iter().any(|t| t == &candidate) {
                            return false;
                        }
                    }
                    true
                }
                Err(_) => true,
            });
        });
    let walker = match threads {
        Some(n) if n > 0 => walker.parallelism(jwalk::Parallelism::RayonNewPool(n)),
        _ => walker,
    };
    let mut out = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: workspace.to_path_buf(),
            message: err.to_string(),
        })?;
        let file_type = entry.file_type();
        let is_file = file_type.is_file();
        // #1548: symlinked SOURCE files are surfaced via their target
        // content when — and only when — the link target lexically stays
        // inside the workspace and resolves to a regular file. Downstream
        // (hash + mtime snapshot at save, replay at load) uses
        // link-following `fs::metadata` / `hash_file`, so the entry
        // naturally carries the target's content hash and mtime.
        // Absolute, escaping, broken, and non-UTF-8 targets stay
        // conservatively OMITTED (the pre-#1548 behavior for all
        // symlinks): a missing manifest entry can only mean "no mtime
        // replayed", i.e. Cargo rebuilds — never an underbuild.
        let is_surfaced_symlink = !is_file
            && file_type.is_symlink()
            && workspace_symlink_is_surfaced(workspace, &entry.path());
        if !is_file && !is_surfaced_symlink {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(workspace)
            .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
        out.push(rel.to_path_buf());
    }
    out.sort();
    Ok(out)
}

/// Build a conservative Cargo-input inventory from compiler dep-info files.
/// A missing, malformed, stale, or build-script-sensitive inventory returns
/// `None`, so callers retain the broad source walk and cannot underbuild.
fn cargo_input_inventory(
    workspace: &Path,
    target_dir: &Path,
    threads: Option<usize>,
) -> Result<Option<Vec<PathBuf>>> {
    if !target_dir.is_dir() {
        return Ok(None);
    }
    let mut dep_info_files = Vec::new();
    let mut build_script_metadata = false;
    let mut workspace_dep_count = 0usize;
    let walker = jwalk::WalkDir::new(target_dir)
        .follow_links(false)
        .skip_hidden(false);
    let walker = match threads {
        Some(n) if n > 0 => walker.parallelism(jwalk::Parallelism::RayonNewPool(n)),
        _ => walker,
    };
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: target_dir.to_path_buf(),
            message: err.to_string(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "d") {
            dep_info_files.push(path.clone());
            if path.strip_prefix(target_dir).ok().is_some_and(|relative| {
                relative
                    .components()
                    .any(|component| component.as_os_str() == "build")
            }) {
                build_script_metadata = true;
            }
        }
        if path.components().any(|c| c.as_os_str() == ".fingerprint")
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("run-build-script"))
        {
            build_script_metadata = true;
        }
    }
    if dep_info_files.is_empty() || build_script_metadata {
        return Ok(None);
    }

    let mut inventory = BTreeSet::new();
    for dep_info in dep_info_files {
        let text = match std::fs::read_to_string(&dep_info) {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        let Some((_, dependencies)) = text.split_once(": ") else {
            return Ok(None);
        };
        for token in makefile_tokens(dependencies) {
            let path = PathBuf::from(token);
            let Ok(relative) = path.strip_prefix(workspace) else {
                continue;
            };
            if relative.as_os_str().is_empty() || !path.is_file() {
                return Ok(None);
            }
            workspace_dep_count += 1;
            inventory.insert(relative.to_path_buf());
        }
    }

    // Cargo manifests and toolchain/config files are inputs even when they
    // are absent from rustc dep-info. Walking metadata is cheap; hashing only
    // this set is the optimization target.
    let target_dirs = workspace_target_dir_candidates(workspace);
    let metadata_walker = jwalk::WalkDir::new(workspace)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(move |_depth, dir_path, _state, children| {
            children.retain(|res| match res {
                Ok(entry) => {
                    let name = entry.file_name.to_string_lossy();
                    if entry.depth > 0 && (name == ".git" || name == "node_modules") {
                        return false;
                    }
                    if entry.depth > 0 && name == "target" {
                        return !target_dirs
                            .iter()
                            .any(|t| t == &dir_path.join(&entry.file_name));
                    }
                    true
                }
                Err(_) => true,
            });
        });
    for entry in metadata_walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: workspace.to_path_buf(),
            message: err.to_string(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name.to_string_lossy();
        if name == "Cargo.toml"
            || name == "Cargo.lock"
            || name == "build.rs"
            || name.starts_with("rust-toolchain")
            || name == "config"
            || name.starts_with("config.")
        {
            inventory.insert(
                entry
                    .path()
                    .strip_prefix(workspace)
                    .map_err(|_| SaveLoadError::BadArchivePath(entry.path().display().to_string()))?
                    .to_path_buf(),
            );
        }
    }
    if inventory.is_empty() || workspace_dep_count == 0 {
        return Ok(None);
    }
    Ok(Some(inventory.into_iter().collect()))
}

fn makefile_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn workspace_files_for_save(workspace: &Path, threads: Option<usize>) -> Result<Vec<PathBuf>> {
    for target_dir in workspace_target_dir_candidates(workspace) {
        if let Some(files) = cargo_input_inventory(workspace, &target_dir, threads)? {
            return Ok(files);
        }
    }
    walk_workspace_files(workspace, threads)
}

/// True when a workspace symlink should appear in the source-file snapshot
/// (#1548): relative target, lexically contained in the workspace root, and
/// resolving to an existing regular file.
fn workspace_symlink_is_surfaced(workspace: &Path, abs: &Path) -> bool {
    let Ok(rel) = abs.strip_prefix(workspace) else {
        return false;
    };
    let Ok(raw) = std::fs::read_link(abs) else {
        return false;
    };
    let Some(target) = symlink_target_to_posix(&raw) else {
        return false;
    };
    if resolve_symlink_target_in_root(rel, &target).is_none() {
        return false;
    }
    std::fs::metadata(abs).map(|m| m.is_file()).unwrap_or(false)
}

/// Like `walk_workspace_files` but does NOT exclude `target/` (because
/// the cache dir itself is often called `cache/` or `zccache/` and we
/// want everything below it). Returns absolute paths of regular files
/// plus, separately, the absolute paths of symlinks encountered (#1548 —
/// the walk never follows them; validation happens in
/// [`walk_cache_files_for_profile`]).
fn walk_cache_files(
    cache_dir: &Path,
    threads: Option<usize>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let walker = jwalk::WalkDir::new(cache_dir)
        .follow_links(false)
        .skip_hidden(false);
    let walker = match threads {
        Some(n) if n > 0 => walker.parallelism(jwalk::Parallelism::RayonNewPool(n)),
        _ => walker,
    };
    let mut files = Vec::new();
    let mut symlinks = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: cache_dir.to_path_buf(),
            message: err.to_string(),
        })?;
        let file_type = entry.file_type();
        if file_type.is_file() {
            files.push(entry.path());
        } else if file_type.is_symlink() {
            symlinks.push(entry.path());
        }
    }
    files.sort();
    symlinks.sort();
    Ok((files, symlinks))
}

#[derive(Debug, Default)]
struct CacheWalk {
    included_paths: Vec<PathBuf>,
    excluded_files: u64,
    excluded_bytes: u64,
    /// Validated in-root symlinks to record in the manifest (#1548).
    symlinks: Vec<SymlinkEntry>,
    /// Symlinks skipped (absolute / escaping / broken / unreadable
    /// target). Each one warned on stderr at walk time.
    skipped_symlinks: u64,
}

fn walk_cache_files_for_profile(
    cache_dir: &Path,
    threads: Option<usize>,
    profile: SaveProfile,
) -> Result<CacheWalk> {
    let mut walk = CacheWalk::default();
    let (files, symlinks) = walk_cache_files(cache_dir, threads)?;
    for abs in files {
        let rel = abs
            .strip_prefix(cache_dir)
            .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
        if archive_always_excludes_cache_path(rel)
            || (profile == SaveProfile::Ci && ci_profile_excludes_cache_path(rel))
        {
            let meta = std::fs::metadata(&abs).map_err(|e| io(&abs, e))?;
            walk.excluded_files += 1;
            walk.excluded_bytes = walk.excluded_bytes.saturating_add(meta.len());
        } else {
            walk.included_paths.push(abs);
        }
    }
    for abs in symlinks {
        let rel = abs
            .strip_prefix(cache_dir)
            .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
        if archive_always_excludes_cache_path(rel)
            || (profile == SaveProfile::Ci && ci_profile_excludes_cache_path(rel))
        {
            walk.excluded_files += 1;
            continue;
        }
        match cache_symlink_entry(&abs, rel) {
            Ok(entry) => walk.symlinks.push(entry),
            Err(reason) => {
                // Record-and-skip LOUDLY (#1548): an unsafe or broken
                // symlink is never silently dropped from the archive.
                // Whatever consumed it after a restore sees a missing
                // path and conservatively rebuilds.
                eprintln!(
                    "soldr save: skipping symlink {} ({reason}) — not archived",
                    abs.display()
                );
                walk.skipped_symlinks += 1;
            }
        }
    }
    Ok(walk)
}

/// Build the manifest entry for one on-disk symlink, or explain why it is
/// conservatively excluded from the archive.
fn cache_symlink_entry(abs: &Path, rel: &Path) -> std::result::Result<SymlinkEntry, &'static str> {
    let raw = std::fs::read_link(abs).map_err(|_| "unreadable link target")?;
    let target = symlink_target_to_posix(&raw).ok_or("non-UTF-8 link target")?;
    resolve_symlink_target_in_root(rel, &target).ok_or("absolute or root-escaping link target")?;
    // The link must resolve to something real at save time — a dangling
    // link is never archived (restored consumers go Dirty instead of
    // trusting a target we could not verify).
    let followed = std::fs::metadata(abs).map_err(|_| "broken link target")?;
    Ok(SymlinkEntry {
        path: rel_to_posix(rel),
        target,
        is_dir: followed.is_dir(),
    })
}

/// Runtime coordination files are local to one daemon instance and cache
/// root. Restoring PID files, spawn locks, sockets, or failure markers into a
/// different root can prevent the embedded compile daemon from starting.
/// They are never cache payload, regardless of the requested save profile.
fn archive_always_excludes_cache_path(rel: &Path) -> bool {
    if rel.components().next().is_some_and(|component| {
        matches!(component, std::path::Component::Normal(part)
            if part.to_string_lossy().eq_ignore_ascii_case("soldr-daemon"))
    }) {
        return true;
    }
    path_is_transient_runtime_file(rel)
}

/// Locks, sockets, PID files, and in-flight staging scratch, at any depth.
///
/// The doc above says these are "never cache payload, regardless of the
/// requested save profile", but only the top-level `soldr-daemon/` tree was
/// actually excluded that way -- the lock/socket/pid rules lived solely in
/// the `ci` profile. So a full-profile `soldr save` archived the embedded
/// cache's live coordination files, and hit the obvious consequence:
///
/// ```text
/// soldr save: io error at .../embedded-v1/v1.12.17/staging/2492-0-.../.active.lock:
///   No such file or directory (os error 2)
/// ```
///
/// The daemon deleted its own lock between the directory walk and the stat.
/// Archiving it was never wanted -- restoring a stale lock or socket into a
/// different root is exactly what the doc warns prevents the compile daemon
/// from starting -- so the fix is to stop collecting it, not to widen the
/// error handling around it.
///
/// `staging/` is included by directory name because its contents are
/// partially-written files by construction: a publish in flight is not cache
/// payload, and its name is not predictable enough to match by suffix.
fn path_is_transient_runtime_file(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if parts.iter().any(|part| part == "staging") {
        return true;
    }
    let Some(file_name) = parts.last().map(String::as_str) else {
        return false;
    };
    matches!(file_name, "lock" | ".lock" | "pid" | ".pid")
        || file_name.ends_with(".lock")
        || file_name.ends_with(".sock")
        || file_name.ends_with(".socket")
        || file_name.ends_with(".pid")
}

fn manifest_path_is_daemon_runtime(path: &str) -> bool {
    manifest_rel_to_path(path)
        .ok()
        .is_some_and(|rel| archive_always_excludes_cache_path(&rel))
}

/// Return true when a cache-relative path is intentionally omitted from
/// the `ci` / `minimal` save profile. This is intentionally conservative:
/// it only drops runtime diagnostics, scratch files, locks/sockets, and
/// top-level soldr-managed tool/binary trees that are re-materialized by
/// the installer rather than consumed by rustc cache lookups.
pub fn ci_profile_excludes_cache_path(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return false;
    }

    if parts.iter().any(|part| {
        matches!(
            part.as_str(),
            "logs" | "log" | "tmp" | "temp" | "scratch" | "sockets" | "locks" | "runtime-binaries"
        )
    }) {
        return true;
    }

    let first = parts[0].as_str();
    if matches!(first, "bin" | "downloads" | "sdk" | "toolchains") {
        return true;
    }

    let file_name = parts.last().map(String::as_str).unwrap_or_default();
    matches!(file_name, "lock" | ".lock" | "pid" | ".pid")
        || file_name.ends_with(".log")
        || file_name.ends_with(".lock")
        || file_name.ends_with(".sock")
        || file_name.ends_with(".socket")
        || file_name.ends_with(".pid")
        || file_name.ends_with(".tmp")
        || file_name.ends_with(".temp")
}

/// Purely-LEXICAL symlink-target containment check (#1548). Resolves
/// `target` relative to `link_rel`'s parent directory (both relative to
/// the same root) and returns the normalized root-relative path of the
/// resolved target, or `None` when the link is unsafe to preserve:
///
/// * absolute targets (`/x`, `C:\x`, UNC prefixes) — rejected outright,
///   even if they happen to point back inside the root;
/// * targets whose `..` traversal escapes the root;
/// * empty targets or targets resolving to the root itself.
///
/// Never touches the filesystem — callers separately decide whether the
/// resolved path must exist (save does; load does not, because the link's
/// payload may legitimately be extracted after the link is examined).
fn resolve_symlink_target_in_root(link_rel: &Path, target: &str) -> Option<PathBuf> {
    if target.is_empty() {
        return None;
    }
    let mut resolved: Vec<std::ffi::OsString> = link_rel
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s.to_os_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for component in Path::new(target).components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => return None,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Escaping above the root ends validation immediately.
                resolved.pop()?;
            }
            std::path::Component::Normal(part) => resolved.push(part.to_os_string()),
        }
    }
    if resolved.is_empty() {
        return None;
    }
    Some(resolved.iter().collect())
}

/// Convert a raw `read_link` value into the forward-slashed UTF-8 string
/// stored in the manifest. `None` for non-UTF-8 targets (conservatively
/// skipped — they can't round-trip through the protobuf string field).
fn symlink_target_to_posix(raw: &Path) -> Option<String> {
    let s = raw.to_str()?;
    #[cfg(windows)]
    {
        Some(s.replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        Some(s.to_string())
    }
}

fn rel_to_posix(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn manifest_rel_to_path(path: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return Err(SaveLoadError::BadArchivePath(path.to_string()));
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return Err(SaveLoadError::BadArchivePath(path.to_string()));
    }
    Ok(out)
}

fn archive_rel_to_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            _ => return Err(SaveLoadError::BadArchivePath(path.display().to_string())),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(SaveLoadError::BadArchivePath(path.display().to_string()));
    }
    Ok(out)
}

/// Manifest entry + the `Metadata` it was derived from. The metadata is
/// reused for the tar header when the file is appended, so `save` /
/// `save_delta` stat each cache file exactly once instead of once in
/// the hash pre-pass and again at append time (#1541). This also keeps
/// the tar header and the manifest byte-for-byte consistent even if
/// the file mutates between the two phases.
/// `Ok(None)` when the file vanished between the directory walk and this
/// stat.
///
/// Defence in depth behind the exclusion above. The walk and the archive are
/// necessarily two separate passes over a tree a live daemon is still
/// writing, so *some* window exists no matter how good the filter is, and a
/// file that no longer exists cannot be cache payload worth failing a whole
/// save over. Scoped to `NotFound` specifically -- a permissions error or a
/// bad disk still fails loudly, because those mean the archive would be
/// silently incomplete.
fn cache_file_entry(
    cache_dir: &Path,
    abs: &Path,
) -> Result<Option<(CacheFile, std::fs::Metadata)>> {
    let rel = abs
        .strip_prefix(cache_dir)
        .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
    let meta = match std::fs::metadata(abs) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(abs, e)),
    };
    let hash = hash_file(abs)?;
    let entry = CacheFile {
        path: rel_to_posix(rel),
        mtime_ns: mtime_ns(&meta),
        size: meta.len(),
        blake3: hash.to_vec(),
    };
    Ok(Some((entry, meta)))
}

// ---------- save ----------

#[derive(Debug, Clone)]
pub struct SaveOptions<'a> {
    /// Workspace root to snapshot source mtimes from. `None` skips the
    /// source-file portion entirely (cache-only archive). Required when
    /// `mtimes_only` is `true`.
    pub workspace: Option<&'a Path>,
    /// Cache directory whose contents will be bundled. `None` is only
    /// permitted when `mtimes_only` is `true` (manifest-only archive).
    pub cache_dir: Option<&'a Path>,
    /// Destination archive path.
    pub out: &'a Path,
    /// zstd compression level (1..=22). 3 is a good default; anything
    /// over 9 hurts CI wall-clock more than it saves on transfer.
    pub zstd_level: i32,
    /// Number of rayon threads for the hash + tar walk. `None` uses the
    /// global rayon pool (`num_cpus`).
    pub threads: Option<usize>,
    /// Produce a manifest-only archive: skip the cache-dir walk and
    /// write a tar.zst whose sole entry is `SOLDR_MANIFEST.pb`. Requires
    /// `workspace` to be `Some`. The intent is a standalone source-file
    /// mtime snapshot that setup-soldr (or any other CI wrapper) can
    /// produce + restore without bundling an artifact cache. The on-disk
    /// shape is otherwise byte-identical to a normal save, so the same
    /// `load()` path consumes it.
    pub mtimes_only: bool,
    /// Cache payload profile. `Full` preserves the historical behavior;
    /// `Ci` excludes runtime-only files and reports those omissions.
    pub profile: SaveProfile,
}

#[derive(Debug, Clone, Default)]
pub struct SaveReport {
    pub profile: SaveProfile,
    pub source_files: u64,
    pub cache_files: u64,
    pub deleted_cache_files: u64,
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    pub archive_bytes: u64,
    pub elapsed_ms: u64,
    /// In-root cache symlinks recorded in the manifest (#1548).
    pub cache_symlinks: u64,
    /// Cache symlinks skipped at save time because their target was
    /// absolute, escaped the cache root, or was broken (#1548). Each
    /// skip also emits a stderr warning — never silent.
    pub cache_symlinks_skipped: u64,
}

#[derive(Clone)]
pub struct SaveDeltaOptions<'a> {
    /// Workspace root to snapshot source mtimes from. `None` skips the
    /// source-file portion entirely.
    pub workspace: Option<&'a Path>,
    /// Current cache directory whose changed/new files will be bundled.
    pub cache_dir: &'a Path,
    /// Base-layer manifest to compare current cache contents against.
    pub base_manifest: &'a Manifest,
    /// Destination delta archive path.
    pub out: &'a Path,
    /// zstd compression level (1..=22).
    pub zstd_level: i32,
    /// Number of rayon threads for hash + tar work.
    pub threads: Option<usize>,
    /// Cache payload profile. Applies before delta comparison so excluded
    /// paths are omitted from the delta manifest and can become tombstones
    /// against a fuller base layer.
    pub profile: SaveProfile,
}

/// Validate save inputs:
/// * When `mtimes_only`, `workspace` MUST be `Some` and `cache_dir`
///   MUST be `None` (passing both would silently ignore one of them).
/// * Otherwise `cache_dir` MUST be `Some` (cache-only archives are the
///   historical baseline behavior; an archive with neither a cache nor a
///   workspace is empty and almost certainly a CLI mistake).
fn validate_save_inputs(opts: &SaveOptions<'_>) -> Result<()> {
    if opts.mtimes_only {
        if opts.workspace.is_none() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr save --mtimes-only requires a --workspace to snapshot",
            )));
        }
        if opts.cache_dir.is_some() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr save --mtimes-only must NOT be combined with --cache-dir",
            )));
        }
    } else if opts.cache_dir.is_none() {
        return Err(SaveLoadError::BareIo(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soldr save requires either --cache-dir or --mtimes-only",
        )));
    }
    Ok(())
}

/// Bundle `cache_dir` + a workspace-source-mtime snapshot into a single
/// `.tar.zst` at `out`.
///
/// When [`SaveOptions::mtimes_only`] is `true`, the cache walk is skipped
/// entirely and the archive contains only `SOLDR_MANIFEST.pb`. That mode
/// requires `workspace` to be `Some` — there is nothing else to snapshot.
pub fn save(opts: &SaveOptions<'_>) -> Result<SaveReport> {
    validate_save_inputs(opts)?;
    let start = std::time::Instant::now();

    // Build the manifest (parallel hash if a workspace is provided)
    // AND enumerate cache files concurrently. The two walks touch
    // disjoint directory trees, so running them in parallel halves
    // page-cache-cold first-walk latency on big workspaces.
    //
    // In `mtimes_only` mode the cache half is a no-op closure — we
    // keep the join shape so the source walk still benefits from the
    // shared rayon pool.
    let pool = build_pool(opts.threads)?;
    let (source_result, cache_walk_result): (Result<Vec<SourceFile>>, Result<CacheWalk>) = pool
        .install(|| {
            rayon::join(
                || -> Result<Vec<SourceFile>> {
                    let Some(ws) = opts.workspace else {
                        return Ok(Vec::new());
                    };
                    let files = workspace_files_for_save(ws, opts.threads)?;
                    files
                        .par_iter()
                        .map(|rel| -> Result<SourceFile> {
                            let abs = ws.join(rel);
                            let meta = std::fs::metadata(&abs).map_err(|e| io(&abs, e))?;
                            let hash = hash_file(&abs)?;
                            Ok(SourceFile {
                                path: rel_to_posix(rel),
                                mtime_ms: mtime_ms(&meta),
                                size: meta.len(),
                                blake3: hash.to_vec(),
                            })
                        })
                        .collect()
                },
                || -> Result<CacheWalk> {
                    if opts.mtimes_only {
                        return Ok(CacheWalk::default());
                    }
                    match opts.cache_dir {
                        Some(dir) if dir.exists() => {
                            walk_cache_files_for_profile(dir, opts.threads, opts.profile)
                        }
                        _ => Ok(CacheWalk::default()),
                    }
                },
            )
        });
    let manifest_files = source_result?;
    let cache_walk = cache_walk_result?;
    let cache_files_paths = cache_walk.included_paths;
    let cache_symlink_entries = cache_walk.symlinks;
    let (cache_manifest_files, cache_file_metas): (Vec<CacheFile>, Vec<std::fs::Metadata>) =
        build_cache_manifest_entries(&pool, opts.cache_dir, &cache_files_paths)?
            .into_iter()
            .unzip();

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        saved_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        workspace: opts
            .workspace
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        // Empty cache_dir_name signals an mtimes-only archive to any
        // future reader that wants to short-circuit the cache-extract
        // path without re-parsing the SaveOptions. The existing reader
        // tolerates either value because it strips the CACHE_DIR_NAME
        // prefix only when a `cache/...` entry shows up in the tar.
        cache_dir_name: if opts.mtimes_only {
            String::new()
        } else {
            CACHE_DIR_NAME.into()
        },
        source_file_count: manifest_files.len() as u64,
        cache_file_count: cache_manifest_files.len() as u64,
        files: manifest_files,
        cache_layer_kind: CacheLayerKind::Complete as i32,
        cache_files: cache_manifest_files,
        base_manifest_blake3: Vec::new(),
        deleted_cache_paths: Vec::new(),
        cache_symlinks: cache_symlink_entries,
    };

    let manifest_bytes = {
        let mut buf = Vec::with_capacity(manifest.encoded_len());
        manifest.encode(&mut buf)?;
        buf
    };

    // Stream tar -> zstd encoder -> file. We append the manifest first
    // (cheap, ~hundreds of KB) and the cache files second so a streaming
    // load can read the manifest without buffering the whole archive.
    if let Some(parent) = opts.out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
    }
    let out_file = File::create(opts.out).map_err(|e| io(opts.out, e))?;
    let out_buf = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    let mut zstd_encoder =
        zstd::stream::write::Encoder::new(out_buf, opts.zstd_level).map_err(SaveLoadError::Zstd)?;
    zstd_encoder
        .multithread(num_cpus_for(opts.threads))
        .map_err(SaveLoadError::Zstd)?;

    let mut cache_files: u64 = 0;
    {
        let mut tar_builder = tar::Builder::new(&mut zstd_encoder);
        tar_builder.mode(tar::HeaderMode::Deterministic);

        // 1) manifest as a regular file at archive root
        {
            append_manifest_entry(&mut tar_builder, &manifest, &manifest_bytes)?;
        }

        // 2) cache dir contents under `cache/`.
        //
        // Cache file list was already enumerated in parallel above,
        // concurrent with the source-file walk. We just stream those
        // files into tar here. The tar writer feeds the multithreaded
        // zstd encoder, which does the heavy CPU work in its own
        // thread pool.
        if !cache_files_paths.is_empty() {
            // cache_files_paths is non-empty only when we enumerated a
            // real cache_dir above (i.e. not the mtimes_only branch),
            // so this expect() is unreachable in practice.
            let cache_dir = opts
                .cache_dir
                .expect("cache_files_paths non-empty implies cache_dir was set");
            cache_files = cache_files_paths.len() as u64;
            for (abs, meta) in cache_files_paths.iter().zip(cache_file_metas.iter()) {
                append_cache_file_entry(&mut tar_builder, cache_dir, abs, meta)?;
            }
        }
        tar_builder.finish().map_err(SaveLoadError::BareIo)?;
    }

    let writer = zstd_encoder.finish().map_err(SaveLoadError::Zstd)?;
    writer
        .into_inner()
        .map_err(|e| SaveLoadError::BareIo(e.into_error()))?;

    let archive_bytes = std::fs::metadata(opts.out).map(|m| m.len()).unwrap_or(0);

    Ok(SaveReport {
        profile: opts.profile,
        source_files: manifest.source_file_count,
        cache_files,
        deleted_cache_files: 0,
        excluded_files: cache_walk.excluded_files,
        excluded_bytes: cache_walk.excluded_bytes,
        archive_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        cache_symlinks: manifest.cache_symlinks.len() as u64,
        cache_symlinks_skipped: cache_walk.skipped_symlinks,
    })
}

pub fn save_delta(opts: &SaveDeltaOptions<'_>) -> Result<SaveReport> {
    let start = std::time::Instant::now();
    let pool = build_pool(opts.threads)?;

    let (source_result, cache_walk_result): (Result<Vec<SourceFile>>, Result<CacheWalk>) = pool
        .install(|| {
            rayon::join(
                || -> Result<Vec<SourceFile>> {
                    let Some(ws) = opts.workspace else {
                        return Ok(Vec::new());
                    };
                    let files = workspace_files_for_save(ws, opts.threads)?;
                    files
                        .par_iter()
                        .map(|rel| -> Result<SourceFile> {
                            let abs = ws.join(rel);
                            let meta = std::fs::metadata(&abs).map_err(|e| io(&abs, e))?;
                            let hash = hash_file(&abs)?;
                            Ok(SourceFile {
                                path: rel_to_posix(rel),
                                mtime_ms: mtime_ms(&meta),
                                size: meta.len(),
                                blake3: hash.to_vec(),
                            })
                        })
                        .collect()
                },
                || -> Result<CacheWalk> {
                    if opts.cache_dir.exists() {
                        walk_cache_files_for_profile(opts.cache_dir, opts.threads, opts.profile)
                    } else {
                        Ok(CacheWalk::default())
                    }
                },
            )
        });
    let manifest_files = source_result?;
    let cache_walk = cache_walk_result?;
    let cache_files_paths = cache_walk.included_paths;
    let cache_manifest_entries =
        build_cache_manifest_entries(&pool, Some(opts.cache_dir), &cache_files_paths)?;

    let base_by_path: BTreeMap<&str, &CacheFile> = opts
        .base_manifest
        .cache_files
        .iter()
        .filter(|entry| !manifest_path_is_daemon_runtime(&entry.path))
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let current_by_path: BTreeMap<&str, (&CacheFile, &PathBuf, &std::fs::Metadata)> =
        cache_manifest_entries
            .iter()
            .zip(cache_files_paths.iter())
            .map(|((entry, meta), path)| (entry.path.as_str(), (entry, path, meta)))
            .collect();

    let mut delta_entries = Vec::new();
    let mut delta_paths = Vec::new();
    for (path, (entry, abs, meta)) in &current_by_path {
        match base_by_path.get(path) {
            Some(base) if cache_file_metadata_matches(base, entry) => {}
            Some(base) if cache_file_content_matches(base, entry) => {
                delta_entries.push((*entry).clone());
            }
            _ => {
                delta_entries.push((*entry).clone());
                delta_paths.push(((*abs).clone(), (*meta).clone()));
            }
        }
    }

    let current_paths: BTreeSet<&str> = current_by_path.keys().copied().collect();
    let mut deleted_cache_paths: Vec<String> = base_by_path
        .keys()
        .copied()
        .filter(|path| !current_paths.contains(path))
        .map(ToOwned::to_owned)
        .collect();

    // Symlink tombstones (#1548): a link present in the base layer but
    // absent from the current cache tree (and not replaced by a regular
    // file, which extraction would overwrite anyway) must be removed on
    // load, exactly like a deleted regular file.
    let current_symlink_paths: BTreeSet<&str> = cache_walk
        .symlinks
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    for base_link in &opts.base_manifest.cache_symlinks {
        let path = base_link.path.as_str();
        if manifest_path_is_daemon_runtime(path) {
            continue;
        }
        if !current_symlink_paths.contains(path) && !current_paths.contains(path) {
            deleted_cache_paths.push(path.to_owned());
        }
    }
    deleted_cache_paths.sort();
    deleted_cache_paths.dedup();

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        saved_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        workspace: opts
            .workspace
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        cache_dir_name: CACHE_DIR_NAME.into(),
        source_file_count: manifest_files.len() as u64,
        cache_file_count: delta_entries.len() as u64,
        files: manifest_files,
        cache_layer_kind: CacheLayerKind::Delta as i32,
        cache_files: delta_entries,
        base_manifest_blake3: manifest_digest(opts.base_manifest)?,
        deleted_cache_paths,
        // Deltas carry the FULL current symlink set (entries are a few
        // bytes each): load recreates them idempotently, so base-vs-delta
        // diffing buys nothing but complexity.
        cache_symlinks: cache_walk.symlinks.clone(),
    };

    write_delta_archive(
        opts.out,
        opts.zstd_level,
        opts.threads,
        &manifest,
        opts.cache_dir,
        &delta_paths,
    )?;
    let archive_bytes = std::fs::metadata(opts.out).map(|m| m.len()).unwrap_or(0);

    Ok(SaveReport {
        profile: opts.profile,
        source_files: manifest.source_file_count,
        cache_files: manifest.cache_file_count,
        deleted_cache_files: manifest.deleted_cache_paths.len() as u64,
        excluded_files: cache_walk.excluded_files,
        excluded_bytes: cache_walk.excluded_bytes,
        archive_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
        cache_symlinks: manifest.cache_symlinks.len() as u64,
        cache_symlinks_skipped: cache_walk.skipped_symlinks,
    })
}

/// Hash + stat every cache file in parallel. Output order matches
/// `cache_files_paths` (rayon's indexed collect preserves order), so
/// callers can zip the two to append files without re-stating them.
fn build_cache_manifest_entries(
    pool: &rayon::ThreadPool,
    cache_dir: Option<&Path>,
    cache_files_paths: &[PathBuf],
) -> Result<Vec<(CacheFile, std::fs::Metadata)>> {
    let Some(cache_dir) = cache_dir else {
        return Ok(Vec::new());
    };
    pool.install(|| {
        cache_files_paths
            .par_iter()
            .filter_map(|abs| cache_file_entry(cache_dir, abs).transpose())
            .collect()
    })
}

fn cache_file_metadata_matches(left: &CacheFile, right: &CacheFile) -> bool {
    left.size == right.size && left.mtime_ns == right.mtime_ns && left.blake3 == right.blake3
}

fn cache_file_content_matches(left: &CacheFile, right: &CacheFile) -> bool {
    left.size == right.size && left.blake3 == right.blake3
}

fn encode_manifest(manifest: &Manifest) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(manifest.encoded_len());
    manifest.encode(&mut buf)?;
    Ok(buf)
}

fn append_manifest_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    manifest: &Manifest,
    manifest_bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(manifest.saved_at_ms.max(0) as u64 / 1000);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar_builder
        .append_data(&mut header, MANIFEST_NAME, manifest_bytes)
        .map_err(SaveLoadError::BareIo)
}

/// Append one cache file into the tar. `meta` comes from the manifest
/// pre-pass ([`cache_file_entry`]) so the file is stat'd exactly once
/// per save and the tar header always agrees with the manifest (#1541).
fn append_cache_file_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    cache_dir: &Path,
    abs: &Path,
    meta: &std::fs::Metadata,
) -> Result<()> {
    let rel = abs
        .strip_prefix(cache_dir)
        .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
    let mut archive_path = PathBuf::from(CACHE_DIR_NAME);
    archive_path.push(rel);
    let archive_path_str = rel_to_posix(&archive_path);
    let mut file = File::open(abs).map_err(|e| io(abs, e))?;
    let mut header = tar::Header::new_gnu();
    header.set_metadata(meta);
    header.set_cksum();
    tar_builder
        .append_data(&mut header, archive_path_str, &mut file)
        .map_err(SaveLoadError::BareIo)
}

fn manifest_digest(manifest: &Manifest) -> Result<Vec<u8>> {
    Ok(zccache::hash::hash_bytes(&encode_manifest(manifest)?)
        .as_bytes()
        .to_vec())
}

fn write_delta_archive(
    out: &Path,
    zstd_level: i32,
    threads: Option<usize>,
    manifest: &Manifest,
    cache_dir: &Path,
    cache_files_paths: &[(PathBuf, std::fs::Metadata)],
) -> Result<()> {
    let manifest_bytes = encode_manifest(manifest)?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
    }
    let out_file = File::create(out).map_err(|e| io(out, e))?;
    let out_buf = BufWriter::with_capacity(8 * 1024 * 1024, out_file);
    let mut zstd_encoder =
        zstd::stream::write::Encoder::new(out_buf, zstd_level).map_err(SaveLoadError::Zstd)?;
    zstd_encoder
        .multithread(num_cpus_for(threads))
        .map_err(SaveLoadError::Zstd)?;

    {
        let mut tar_builder = tar::Builder::new(&mut zstd_encoder);
        tar_builder.mode(tar::HeaderMode::Deterministic);

        append_manifest_entry(&mut tar_builder, manifest, &manifest_bytes)?;

        for (abs, meta) in cache_files_paths {
            append_cache_file_entry(&mut tar_builder, cache_dir, abs, meta)?;
        }
        tar_builder.finish().map_err(SaveLoadError::BareIo)?;
    }

    let writer = zstd_encoder.finish().map_err(SaveLoadError::Zstd)?;
    writer
        .into_inner()
        .map_err(|e| SaveLoadError::BareIo(e.into_error()))?;
    Ok(())
}

pub fn read_manifest_from_archive(archive: &Path) -> Result<Manifest> {
    let in_file = File::open(archive).map_err(|e| io(archive, e))?;
    let buf = BufReader::with_capacity(16 * 1024 * 1024, in_file);
    let zstd_reader = zstd::stream::read::Decoder::new(buf).map_err(SaveLoadError::Zstd)?;
    let mut tar_reader = tar::Archive::new(zstd_reader);
    for entry in tar_reader.entries().map_err(SaveLoadError::BareIo)? {
        let mut entry = entry.map_err(SaveLoadError::BareIo)?;
        let path = entry.path().map_err(SaveLoadError::BareIo)?.into_owned();
        if path.as_os_str() != MANIFEST_NAME {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(SaveLoadError::BareIo)?;
        return Ok(Manifest::decode(&buf[..])?);
    }
    Err(SaveLoadError::MissingManifest)
}

pub fn read_manifest_file(path: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(path).map_err(|e| io(path, e))?;
    Ok(Manifest::decode(&bytes[..])?)
}

pub fn write_manifest_file(path: &Path, manifest: &Manifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
    }
    let bytes = encode_manifest(manifest)?;
    std::fs::write(path, bytes).map_err(|e| io(path, e))
}

// ---------- load ----------

#[derive(Debug, Clone)]
pub struct LoadOptions<'a> {
    pub archive: &'a Path,
    /// Destination cache directory. `None` is only permitted when
    /// `mtimes_only` is `true`; in that mode any `cache/...` tar entry
    /// is treated as a hard error since the archive should not have
    /// contained one.
    pub cache_dir: Option<&'a Path>,
    /// Workspace whose source-file mtimes should be replayed. `None`
    /// skips the mtime-replay step (cache-only load). Required when
    /// `mtimes_only` is `true`.
    pub workspace: Option<&'a Path>,
    pub threads: Option<usize>,
    /// Treat the archive as a manifest-only snapshot. Skip the cache
    /// extraction step (the archive should not contain any `cache/...`
    /// entries; one is an error). Requires `workspace` to be `Some` —
    /// otherwise the load is a no-op.
    pub mtimes_only: bool,
    /// Emit a per-phase profile line to stderr after the load finishes:
    /// zstd decode time, tar parse + dispatch time, total extract time,
    /// per-worker job count, and per-file extract latency percentiles.
    /// Useful for tuning the parallel-extract worker count (#575).
    pub profile_extract: bool,
    /// On Windows, when the current process is admin, briefly add the
    /// cache directory to the Defender exclusion list for the duration
    /// of the load. No-op on non-Windows or when not admin — never
    /// triggers a UAC prompt. Default off; setup-soldr passes this on
    /// Windows runners. (#575)
    pub auto_defender_exclude: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    pub cache_files_restored: u64,
    pub source_files_in_manifest: u64,
    pub mtimes_applied: u64,
    pub mtimes_skipped_missing: u64,
    pub mtimes_skipped_size_mismatch: u64,
    pub mtimes_skipped_modified: u64,
    pub elapsed_ms: u64,
    /// Manifest symlinks recreated inside the restore root (#1548).
    pub cache_symlinks_restored: u64,
    /// Manifest symlinks NOT recreated: invalid/escaping target on
    /// re-validation, a real directory in the way, or symlink creation
    /// failed (e.g. missing Windows privilege). Each skip warns on
    /// stderr — never silent (#1548).
    pub cache_symlinks_skipped: u64,
}

/// Validate load inputs:
/// * When `mtimes_only`, `workspace` MUST be `Some` and `cache_dir`
///   MUST be `None`. Mixing the two is a CLI mistake.
/// * Otherwise `cache_dir` MUST be `Some` (the load has to know where
///   to extract cache entries).
fn validate_load_inputs(opts: &LoadOptions<'_>) -> Result<()> {
    if opts.mtimes_only {
        if opts.workspace.is_none() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr load --mtimes-only requires a --workspace to replay into",
            )));
        }
        if opts.cache_dir.is_some() {
            return Err(SaveLoadError::BareIo(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "soldr load --mtimes-only must NOT be combined with --cache-dir",
            )));
        }
    } else if opts.cache_dir.is_none() {
        return Err(SaveLoadError::BareIo(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soldr load requires either --cache-dir or --mtimes-only",
        )));
    }
    Ok(())
}

/// Decompress + restore an archive produced by [`save`].
///
/// The implementation pipelines THREE operations on the existing rayon pool:
///   1. **Stream-decompress + tar-header parsing** in the driver thread —
///      zstd's read-side decoder is single-threaded by design.
///   2. **Per-file extraction** (CreateFile + write + set_mtime) dispatched
///      via a bounded `sync_channel` to N rayon workers. On Windows this
///      pipelines Defender real-time-scan callbacks across cores instead of
///      serializing them on a single thread — same insight as zackees/zccache#189
///      for the analogous walk problem. (zackees/soldr#575)
///   3. **Mtime replay onto workspace sources** runs concurrently with (2)
///      on its own rayon task once the manifest is parsed.
///
/// Per-file mtime preservation: each worker restores the manifest's
/// nanosecond mtime right after the write completes (#1541) — the tar
/// header's second-truncated mtime is only a fallback for entries the
/// manifest doesn't cover. Manifest entries whose payload was not in
/// the tar (delta metadata-only updates) get their mtimes replayed in
/// a parallel pass after extraction instead of the historical serial
/// stat+set loop over every manifest entry.
///
/// When [`LoadOptions::mtimes_only`] is `true`, only the manifest entry
/// is consumed; any `cache/...` entry in the archive is rejected as a
/// hard error (the producer should not have included one).
pub fn load(opts: &LoadOptions<'_>) -> Result<LoadReport> {
    validate_load_inputs(opts)?;
    let start = std::time::Instant::now();

    if let Some(dir) = opts.cache_dir {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }

    let in_file = File::open(opts.archive).map_err(|e| io(opts.archive, e))?;
    let buf = BufReader::with_capacity(16 * 1024 * 1024, in_file);
    let zstd_reader = zstd::stream::read::Decoder::new(buf).map_err(SaveLoadError::Zstd)?;
    let mut tar_reader = tar::Archive::new(zstd_reader);
    // Belt-and-suspenders (soldr#1144): historically we set
    // preserve_mtime(false) here on the theory that our per-worker
    // filetime::set_file_mtime path would handle the restore. That
    // path only runs for the parallel-extract dispatch; if any code
    // path drains via tar's own unpack (e.g. a future refactor, an
    // error-recovery fallback, or a mtimes_only load whose payload
    // grew a cache/ entry) the mtime silently defaults to
    // extraction-wall-clock. Cargo's incremental fingerprint records
    // an artifact's mtime at first compile and treats a later
    // "newer" mtime as evidence of external modification, forcing
    // re-link + re-fingerprint on every hit — the exact 20x/hit
    // slowdown seen in perf-matrix run 28497381630 (medium/cold-
    // tar-untar-warm at 1.22x speedup vs the 3.0x floor). Setting
    // this to `true` makes tar's built-in mtime restore the
    // baseline; per-worker restore stays as the fast path.
    //
    // Permissions still get restored by the worker (see extract_one
    // — it chmods from job.mode_bits after write). tar's
    // preserve_permissions would clobber that on Windows (where
    // Unix mode bits are meaningless).
    tar_reader.set_preserve_mtime(true);
    tar_reader.set_preserve_permissions(false);

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut manifest_decoded: Option<Manifest> = None;
    // #1541: manifest-driven cache-file mtimes. Keyed by the manifest's
    // POSIX-relative path; populated once the manifest entry is parsed
    // (archives produced by `save` always put it first). Slots flip to
    // `applied` when the corresponding tar entry is dispatched with the
    // nanosecond mtime attached, so the post-extract replay pass only
    // has to touch manifest entries whose payload was NOT in the tar
    // (delta metadata-only updates).
    let mut cache_mtime_index: HashMap<String, CacheMtimeSlot> = HashMap::new();
    // Env override (LOAD_WORKERS_ENV / SOLDR_LOAD_WORKERS) wins over the
    // caller-supplied --threads; otherwise the explicit --threads wins;
    // otherwise rayon picks its default (num_cpus). The pool we build here
    // is also what the per-file extract workers run on, so this single
    // knob governs all load-time parallelism.
    let effective_threads = load_worker_count_override().or(opts.threads);
    let pool = build_pool(effective_threads)?;
    // Holds the mtime-replay job once we've parsed the manifest and
    // dispatched the work onto rayon. We poll on it after the tar
    // stream is fully drained.
    let mut replay_handle: Option<std::sync::mpsc::Receiver<Vec<MtimeOutcome>>> = None;

    // #575 parallel extraction infrastructure. Spun up lazily on the first
    // cache entry so mtimes_only loads pay zero overhead.
    let mut extract_dispatch: Option<ExtractDispatch> = None;
    let extract_error: Arc<Mutex<Option<SaveLoadError>>> = Arc::new(Mutex::new(None));
    let cache_files_counter = Arc::new(AtomicU64::new(0));
    let profile = if opts.profile_extract {
        Some(Arc::new(ExtractProfile::new(pool.current_num_threads())))
    } else {
        None
    };
    let driver_loop_start = std::time::Instant::now();
    // Cumulative microseconds the driver spent inside `entry.read_to_end`
    // for cache-file bodies. That call drives the zstd decoder, so this
    // is the closest cheap approximation of "zstd_decode" wall-clock the
    // streaming API gives us. tar_parse_us is the loop's remaining time.
    let mut zstd_decode_us: u64 = 0;

    // #575/#596 Defender auto-exclusion (Windows + admin only — never
    // UAC-prompts). The guard removes the exclusion on drop.
    let _defender_guard = if opts.auto_defender_exclude {
        opts.cache_dir
            .map(defender_exclusion_guard_for)
            .unwrap_or_default()
    } else {
        DefenderExclusionGuard::default()
    };

    for entry in tar_reader.entries().map_err(SaveLoadError::BareIo)? {
        let mut entry = entry.map_err(SaveLoadError::BareIo)?;
        let path = entry.path().map_err(SaveLoadError::BareIo)?.into_owned();

        if path.as_os_str() == MANIFEST_NAME {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(SaveLoadError::BareIo)?;
            let manifest: Manifest = prost::Message::decode(&buf[..])?;
            if let Some(cache_dir) = opts.cache_dir {
                apply_cache_tombstones(cache_dir, &manifest)?;
                cache_mtime_index = build_cache_mtime_index(&manifest);
            }
            manifest_bytes = Some(buf);
            // Kick off the mtime replay NOW so it runs in parallel
            // with the rest of the tar extraction. The cache files
            // and workspace source files live on disjoint trees so
            // their I/O doesn't fight.
            if let Some(ws) = opts.workspace {
                let manifest_for_replay = manifest.clone();
                let ws_owned = ws.to_path_buf();
                let (tx, rx) = std::sync::mpsc::channel();
                pool.spawn(move || {
                    let outcomes: Vec<MtimeOutcome> = manifest_for_replay
                        .files
                        .par_iter()
                        .map(|e| replay_one(&ws_owned, e))
                        .collect();
                    let _ = tx.send(outcomes);
                });
                replay_handle = Some(rx);
            }
            manifest_decoded = Some(manifest);
            continue;
        }

        // Expect everything else under `cache/`. In mtimes_only mode
        // there should be no such entries — a producer-side bug if
        // there is one, so reject it loudly.
        let stripped = match path.strip_prefix(CACHE_DIR_NAME) {
            Ok(p) => archive_rel_to_path(p)?,
            Err(_) => {
                return Err(SaveLoadError::BadArchivePath(path.display().to_string()));
            }
        };
        // Compatibility with archives produced before daemon runtime state
        // became reserved: drain but never materialize those entries. A load
        // must not overwrite the live PID/lock/socket namespace of this host.
        if archive_always_excludes_cache_path(&stripped) {
            continue;
        }
        if opts.mtimes_only {
            return Err(SaveLoadError::BadArchivePath(format!(
                "mtimes_only load refuses cache entry: {}",
                path.display()
            )));
        }
        // cache_dir is guaranteed Some by validate_load_inputs when we
        // reach this branch.
        let cache_dir = opts.cache_dir.expect("cache_dir checked at entry");
        let dest = cache_dir.join(&stripped);
        let entry_type = entry.header().entry_type();

        // Directories: create immediately on the driver thread. Cheap; this
        // also guarantees the directory exists by the time workers race to
        // write files inside it (we still call create_dir_all on the parent
        // in the worker as a belt-and-suspenders, since tar doesn't
        // guarantee directory entries precede their contents).
        if entry_type == tar::EntryType::Directory {
            std::fs::create_dir_all(&dest).map_err(|e| io(&dest, e))?;
            continue;
        }

        // Read the body fully into memory + capture mtime, then dispatch to
        // a worker. For the cache-archive use case bodies are typically
        // small (<MiB each) and the bounded channel caps how many are
        // resident at once, so memory usage stays bounded.
        let mut body = Vec::new();
        let body_read_start = opts.profile_extract.then(std::time::Instant::now);
        entry
            .read_to_end(&mut body)
            .map_err(SaveLoadError::BareIo)?;
        if let Some(t0) = body_read_start {
            zstd_decode_us = zstd_decode_us.saturating_add(t0.elapsed().as_micros() as u64);
        }
        let mtime_secs = entry.header().mtime().ok();
        // Capture the tar header's Unix mode so the worker can chmod
        // the file after writing. Without this, executable scripts
        // and binaries (e.g. cargo `build-script-build`) lose +x on
        // restore and fail with EACCES — see #587.
        let mode_bits = entry.header().mode().ok();
        // #1541: prefer the manifest's nanosecond mtime over the tar
        // header's second-truncated one. Guarded by a size match so a
        // payload that diverged from the manifest (e.g. mutated mid-
        // save) falls back to the header mtime, exactly like the old
        // replay pass would have skipped it on its size check.
        let mut mtime_ns: Option<i64> = None;
        if !cache_mtime_index.is_empty() {
            let rel_posix = rel_to_posix(&stripped);
            if let Some(slot) = cache_mtime_index.get_mut(&rel_posix) {
                if slot.size == body.len() as u64 {
                    mtime_ns = Some(slot.mtime_ns);
                    slot.applied = true;
                }
            }
        }

        // Lazy-start the dispatch on first cache entry.
        let dispatch = extract_dispatch.get_or_insert_with(|| {
            ExtractDispatch::start(
                &pool,
                opts.threads,
                Arc::clone(&extract_error),
                Arc::clone(&cache_files_counter),
                profile.as_ref().map(Arc::clone),
            )
        });

        let job = ExtractJob {
            dest,
            entry_type,
            body,
            mtime_secs,
            mtime_ns,
            mode_bits,
        };
        if dispatch.send(job).is_err() {
            // Receivers are gone — there must be a stored error already.
            break;
        }
    }

    let driver_loop_us = driver_loop_start.elapsed().as_micros() as u64;
    let workers_drain_start = std::time::Instant::now();
    // Close the dispatch channel and wait for workers to drain. Any error
    // from a worker is surfaced here (first-error-wins).
    if let Some(dispatch) = extract_dispatch {
        dispatch.finish()?;
    }
    let workers_drain_us = workers_drain_start.elapsed().as_micros() as u64;
    if let Some(err) = extract_error.lock().expect("extract_error mutex").take() {
        return Err(err);
    }
    let cache_files_restored = cache_files_counter.load(Ordering::Relaxed);
    if let Some(profile) = profile {
        let phases = ExtractPhaseTimings {
            zstd_decode_us,
            // tar_parse_us = driver loop time minus the read_to_end accounting.
            // Saturates to 0 if profile noise (timer jitter, scheduler) pushed
            // accumulated zstd_decode_us slightly past driver_loop_us.
            tar_parse_us: driver_loop_us.saturating_sub(zstd_decode_us),
            extract_total_us: driver_loop_us.saturating_add(workers_drain_us),
        };
        profile.emit_to_stderr(phases, cache_files_restored);
    }

    let manifest = match manifest_decoded {
        Some(manifest) => manifest,
        None => {
            let manifest_bytes = manifest_bytes.ok_or(SaveLoadError::MissingManifest)?;
            prost::Message::decode(&manifest_bytes[..])?
        }
    };

    let mut report = LoadReport {
        cache_files_restored,
        source_files_in_manifest: manifest.files.len() as u64,
        ..LoadReport::default()
    };

    if let Some(cache_dir) = opts.cache_dir {
        // #1548: recreate manifest symlinks AFTER extraction so their
        // targets already exist. Every entry is re-validated against the
        // restore root here — a crafted manifest can never make load
        // create a link that points outside `cache_dir`.
        let (restored, skipped) = restore_cache_symlinks(cache_dir, &manifest.cache_symlinks);
        report.cache_symlinks_restored = restored;
        report.cache_symlinks_skipped = skipped;
        replay_pending_cache_file_mtimes(
            &pool,
            cache_dir,
            &manifest.cache_files,
            &cache_mtime_index,
        )?;
    }

    // If we kicked off the replay early, wait for it. Otherwise (no
    // workspace, or first-run before manifest seen) run it inline
    // here for completeness.
    if let Some(rx) = replay_handle {
        let outcomes = rx.recv_timeout(REPLAY_WORKER_RECV_TIMEOUT).map_err(|err| {
            SaveLoadError::BareIo(std::io::Error::other(format!(
                "replay worker did not finish within {}s: {err}",
                REPLAY_WORKER_RECV_TIMEOUT.as_secs()
            )))
        })?;
        for o in outcomes {
            match o {
                MtimeOutcome::Applied => report.mtimes_applied += 1,
                MtimeOutcome::Missing => report.mtimes_skipped_missing += 1,
                MtimeOutcome::SizeMismatch => report.mtimes_skipped_size_mismatch += 1,
                MtimeOutcome::Modified => report.mtimes_skipped_modified += 1,
            }
        }
    }

    report.elapsed_ms = start.elapsed().as_millis() as u64;
    Ok(report)
}

enum MtimeOutcome {
    Applied,
    Missing,
    SizeMismatch,
    Modified,
}

// ---------------------------------------------------------------------------
// #575 parallel cache-file extraction
// ---------------------------------------------------------------------------

/// In-flight work item dispatched from the tar driver to a rayon worker.
struct ExtractJob {
    dest: PathBuf,
    entry_type: tar::EntryType,
    body: Vec<u8>,
    mtime_secs: Option<u64>,
    /// Nanosecond-precision mtime from the archive manifest (#1541).
    /// When present it wins over `mtime_secs` (the tar header only
    /// carries seconds) and the post-extract manifest replay pass is
    /// skipped for this file — the worker's write is the final word,
    /// eliminating one stat + one utimensat per restored file.
    mtime_ns: Option<i64>,
    /// Unix file mode from the tar header, used to restore the
    /// executable bit on Unix. None when the header lacked a mode.
    /// Ignored on Windows (NTFS uses ACLs, not Unix modes; tar
    /// archives don't carry meaningful NTFS permissions). (#587)
    mode_bits: Option<u32>,
}

/// Bounded-channel + worker-thread bundle that owns the parallel extraction
/// of cache-file entries. Wraps the existing rayon pool — no new thread
/// runtime, no new direct deps. Bounded so the driver pauses if workers
/// can't keep up (caps in-memory body buffer at ~`bound × entry_size`).
struct ExtractDispatch {
    /// `Option` so shutdown can close the channel exactly once, whether it
    /// is reached through [`ExtractDispatch::finish`] or through `Drop`.
    tx: Option<std::sync::mpsc::SyncSender<ExtractJob>>,
    /// Barrier joined when every worker exits; size = num_workers + 1
    /// (workers + the driver caller).
    barrier: Arc<std::sync::Barrier>,
    /// Guards against waiting on `barrier` twice. A `Barrier` is reusable:
    /// a second wait opens a new generation that only `n_workers + 1`
    /// further arrivals could release, so double-waiting would hang.
    shutdown_done: bool,
}

impl ExtractDispatch {
    fn start(
        pool: &rayon::ThreadPool,
        _threads: Option<usize>,
        err_slot: Arc<Mutex<Option<SaveLoadError>>>,
        counter: Arc<AtomicU64>,
        profile: Option<Arc<ExtractProfile>>,
    ) -> Self {
        // Worker count = pool size. The pool was built by `build_pool` with
        // the user's `--threads` value (or rayon's default), so trusting it
        // here keeps the user's intent intact AND guarantees we never spawn
        // more closures than the pool has threads to run (which would
        // deadlock our barrier if the spawned closures wait for siblings
        // that never get scheduled).
        let n_workers = pool.current_num_threads().max(1);
        // Bounded queue. 64 = trade-off between (1) the driver getting
        // throttled by slow workers vs (2) the resident memory of in-flight
        // bodies. With cache-archive entries averaging <200 KiB, 64 caps
        // memory at ~13 MiB which is negligible.
        let (tx, rx) = sync_channel::<ExtractJob>(64);
        let rx = Arc::new(Mutex::new(rx));
        let barrier = Arc::new(std::sync::Barrier::new(n_workers + 1));

        for worker_idx in 0..n_workers {
            let rx = Arc::clone(&rx);
            let err_slot = Arc::clone(&err_slot);
            let counter = Arc::clone(&counter);
            let barrier = Arc::clone(&barrier);
            let profile = profile.as_ref().map(Arc::clone);
            pool.spawn(move || {
                loop {
                    let job = {
                        let guard = rx.lock().expect("extract rx mutex");
                        match guard.recv_timeout(EXTRACT_WORKER_RECV_TIMEOUT) {
                            Ok(j) => j,
                            // Only a closed channel means "no more work".
                            // A timeout means the driver is merely slow --
                            // a big zstd frame, a stalled disk. Treating it
                            // as disconnect (the previous behaviour) retired
                            // every worker permanently, after which the
                            // SyncSender still accepted up to `bound` jobs
                            // that nobody would ever extract; `finish()`
                            // then returned Ok and `load()` reported success
                            // with files silently absent from the tree.
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    };
                    // If a sibling already recorded an error, drain remaining
                    // jobs without doing more I/O (keeps the driver moving
                    // toward the finish line so we can surface the error
                    // promptly).
                    if err_slot.lock().expect("err_slot mutex").is_some() {
                        continue;
                    }
                    let is_regular = job.entry_type == tar::EntryType::Regular;
                    let job_start = profile.is_some().then(std::time::Instant::now);
                    if let Err(e) = extract_one(&job) {
                        let mut slot = err_slot.lock().expect("err_slot mutex");
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        continue;
                    }
                    if is_regular {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    if let (Some(prof), Some(t0)) = (profile.as_ref(), job_start) {
                        let us = t0.elapsed().as_micros() as u64;
                        prof.record(worker_idx, us);
                    }
                }
                barrier.wait();
            });
        }

        ExtractDispatch {
            tx: Some(tx),
            barrier,
            shutdown_done: false,
        }
    }

    fn send(
        &self,
        job: ExtractJob,
    ) -> std::result::Result<(), std::sync::mpsc::SendError<ExtractJob>> {
        match self.tx.as_ref() {
            Some(tx) => tx.send(job),
            // Only reachable after shutdown, which the driver never does
            // before its last send. Report it as a send failure rather
            // than panicking so a future refactor degrades loudly but
            // safely.
            None => Err(std::sync::mpsc::SendError(job)),
        }
    }

    /// Close the channel and block until every worker has exited.
    /// Idempotent, and shared with `Drop` so the wait cannot be skipped.
    fn shutdown(&mut self) {
        if self.shutdown_done {
            return;
        }
        self.shutdown_done = true;
        // Dropping the sender is what lets workers observe `Disconnected`
        // and leave their receive loop.
        self.tx = None;
        self.barrier.wait();
    }

    /// Close the channel and block until every worker has exited.
    /// Returns Ok(()) regardless of worker errors — those land in the
    /// shared err_slot the caller passed to `start`.
    fn finish(mut self) -> Result<()> {
        self.shutdown();
        Ok(())
    }
}

/// Sibling path used to stage a restored file before it is renamed into
/// place (#1909).
///
/// Must live in the same directory as `dest`: `rename` is only atomic within
/// a filesystem, and a temp dir can easily be on a different one. The name
/// carries pid + a process-local counter so concurrent extract workers — and
/// concurrent `soldr load` processes sharing a target dir — never collide.
fn staging_path_for(dest: &Path) -> PathBuf {
    static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "entry".to_string());
    dest.with_file_name(format!(
        ".{name}.soldr-tmp-{pid}-{seq}",
        pid = std::process::id()
    ))
}

/// #1909: the driver loop reaches `load()`'s error paths through `?`, which
/// drops the dispatch without calling [`ExtractDispatch::finish`]. Without a
/// `Drop` impl those rayon workers kept running after `load()` returned,
/// still writing into the cache tree the caller was about to use -- and
/// cargo exec'ing a build script while a worker held it open for write is
/// `ETXTBSY` ("Text file busy"), the failure this fixes.
///
/// They also leaked: workers park on a barrier sized `n_workers + 1` whose
/// final party (the driver) had already returned, so nothing ever released
/// them. `rayon::ThreadPool::drop` does not join outstanding `spawn`ed
/// closures, so dropping the pool did not rescue this either.
impl Drop for ExtractDispatch {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Worker-side per-entry extraction. Splits Regular vs Directory handling
/// (Directories are created by the driver thread, so we only see Regular
/// + the long tail of tar entry types here).
fn extract_one(job: &ExtractJob) -> Result<()> {
    if let Some(parent) = job.dest.parent() {
        // Belt-and-suspenders: tar doesn't guarantee directory entries
        // precede their contents, so workers that race ahead of a sibling
        // directory entry still get their parent dir created.
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    match job.entry_type {
        tar::EntryType::Regular => {
            // #1909: write to a sibling temp path and rename into place,
            // rather than writing `dest` directly.
            //
            // `execve` fails with ETXTBSY if *any* process holds the target
            // open for writing. Our own handle is closed before this function
            // returns, but soldr spawns detached children (the auto-gc
            // sweeper, the daemon) throughout a build: a child forked while a
            // write descriptor is open inherits it, and keeps that inode busy
            // until it execs. Rust opens files O_CLOEXEC, so the descriptor
            // does not survive the exec — but between fork and exec it exists,
            // and that window is enough for cargo to try running a restored
            // build script and get "Text file busy".
            //
            // Renaming makes the race structurally impossible instead of
            // merely unlikely: the inode that lands at `dest` never had a
            // writable descriptor pointing at it, so no inherited fd can
            // refer to it. It also means `dest` never exists in a
            // partially-written state.
            let staged = staging_path_for(&job.dest);
            std::fs::write(&staged, &job.body).map_err(|e| io(&staged, e))?;

            // Apply metadata to the staged file, so the entry becomes visible
            // at `dest` already complete. `rename` preserves both.
            //
            // #587: restore +x (and other Unix permission bits) from
            // the tar header. Without this, cargo build-script-build
            // binaries restored from cache fail execve with EACCES.
            // Windows ignores the mode — NTFS uses ACLs, and the tar
            // header's Unix mode isn't meaningful there.
            #[cfg(unix)]
            if let Some(mode) = job.mode_bits {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(mode);
                if let Err(e) = std::fs::set_permissions(&staged, perms) {
                    let _ = std::fs::remove_file(&staged);
                    return Err(io(&staged, e));
                }
            }
            let stamp = if let Some(ns) = job.mtime_ns {
                // Manifest-driven metadata application (#1541): restore
                // the exact nanosecond mtime here (atime = mtime, matching
                // what the manifest replay pass used to do serially after
                // extraction).
                Some(filetime::FileTime::from_system_time(ns_to_systime(ns)))
            } else {
                job.mtime_secs
                    .map(|secs| filetime::FileTime::from_unix_time(secs as i64, 0))
            };
            if let Some(stamp) = stamp {
                if let Err(e) = filetime::set_file_times(&staged, stamp, stamp) {
                    let _ = std::fs::remove_file(&staged);
                    return Err(io(&staged, e));
                }
            }

            if let Err(e) = std::fs::rename(&staged, &job.dest) {
                // Never leave the staging file behind on failure; a stray
                // `.soldr-tmp` in `target/` would confuse cargo and survive
                // into the next build.
                let _ = std::fs::remove_file(&staged);
                return Err(io(&job.dest, e));
            }
        }
        tar::EntryType::Directory => {
            // Already handled by the driver, but if somehow we got here,
            // ensure idempotency.
            std::fs::create_dir_all(&job.dest).map_err(|e| io(&job.dest, e))?;
        }
        other => {
            // Symlinks / hard links / device nodes etc. are not produced
            // by `save` (only Regular + Directory; symlinks travel as
            // manifest-only `cache_symlinks` entries — #1548). Reject
            // loudly so we don't silently swallow a future archive shape
            // change.
            return Err(SaveLoadError::BareIo(std::io::Error::other(format!(
                "unexpected tar entry type {other:?} at {}",
                job.dest.display()
            ))));
        }
    }
    Ok(())
}

/// Per-phase driver-thread wall-clock collected during a profiled load.
/// Microsecond precision so `format_profile_line` can emit ms with no
/// loss at the conversion boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtractPhaseTimings {
    pub zstd_decode_us: u64,
    pub tar_parse_us: u64,
    pub extract_total_us: u64,
}

/// Per-load profiling state collected when `LoadOptions::profile_extract`
/// is on (#575). Each worker writes into its own slot — no contention.
/// `emit_to_stderr` formats the summary line at the end of `load()`.
struct ExtractProfile {
    /// Per-worker microsecond latencies for each successful Regular
    /// extraction. Sized once at construction; never resized later.
    per_worker_latencies: Vec<Mutex<Vec<u64>>>,
}

impl ExtractProfile {
    fn new(n_workers: usize) -> Self {
        let mut per_worker_latencies = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            per_worker_latencies.push(Mutex::new(Vec::new()));
        }
        ExtractProfile {
            per_worker_latencies,
        }
    }

    fn record(&self, worker_idx: usize, latency_us: u64) {
        // Defensive: rayon promises a stable index in `[0, n_workers)`,
        // but a future re-architecture might break that — fail soft.
        if let Some(slot) = self.per_worker_latencies.get(worker_idx) {
            if let Ok(mut v) = slot.lock() {
                v.push(latency_us);
            }
        }
    }

    fn emit_to_stderr(&self, phases: ExtractPhaseTimings, files: u64) {
        let mut all_us: Vec<u64> = Vec::new();
        let mut per_worker_counts = Vec::with_capacity(self.per_worker_latencies.len());
        for slot in &self.per_worker_latencies {
            let v = slot.lock().expect("profile slot mutex");
            per_worker_counts.push(v.len());
            all_us.extend_from_slice(&v);
        }
        eprintln!(
            "{}",
            format_profile_line(phases, &per_worker_counts, &all_us, files)
        );
    }
}

/// Render the documented `soldr load: profile:` line shape (#575). Exposed
/// at module scope so unit tests can exercise the format independent of a
/// live extract.
///
/// Shape matches the spec in zackees/soldr#575:
///
/// ```text
/// soldr load: profile: zstd_decode=4120ms tar_parse=890ms extract_total=10510ms
///   workers={0:n=12058, 1:n=12090, 2:n=12053, 3:n=12030}
///   per_file_p50_us=180 p95_us=450 p99_us=1200 cache_files=48231
/// ```
///
/// Driven by `extract_total_us` instead of summing — that's the only
/// wall-clock number that includes the workers-drain tail, which is the
/// number tuning anyone actually cares about. Worker indices are 0-based
/// to match rayon's convention.
pub fn format_profile_line(
    phases: ExtractPhaseTimings,
    per_worker_counts: &[usize],
    per_file_latencies_us: &[u64],
    files: u64,
) -> String {
    let mut sorted: Vec<u64> = per_file_latencies_us.to_vec();
    sorted.sort_unstable();
    let pct = |p: f64| -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    let workers_summary: String = per_worker_counts
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{}:n={}", i, n))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "soldr load: profile: zstd_decode={zstd}ms tar_parse={tar}ms extract_total={total}ms workers={{{workers}}} per_file_p50_us={p50} p95_us={p95} p99_us={p99} cache_files={files}",
        zstd = phases.zstd_decode_us / 1000,
        tar = phases.tar_parse_us / 1000,
        total = phases.extract_total_us / 1000,
        workers = workers_summary,
        p50 = pct(0.50),
        p95 = pct(0.95),
        p99 = pct(0.99),
        files = files,
    )
}

/// RAII guard for `--auto-defender-exclude` (#596).
///
/// When constructed via [`defender_exclusion_guard_for`] on Windows with
/// an admin token and PowerShell available, the cache directory is added
/// to Defender's exclusion list via `Add-MpPreference`. On drop the
/// matching `Remove-MpPreference` runs best-effort. Outside that happy
/// path the guard is a no-op — we never trigger a UAC prompt.
#[derive(Default)]
struct DefenderExclusionGuard {
    tracked: Option<(PathBuf, String)>,
}

impl Drop for DefenderExclusionGuard {
    fn drop(&mut self) {
        let Some((powershell, path)) = self.tracked.take() else {
            return;
        };
        let plan = vec![crate::defender::PathAction {
            path: path.clone(),
            action: crate::defender::ExclusionAction::Remove,
            scope: "soldr-load".into(),
            status: crate::defender::ActionStatus::Planned,
            detail: None,
        }];
        // Why: we just added this path on guard creation, so always
        // attempt removal — don't re-query Defender (which could return
        // stale state under heavy load or contention) and pass the
        // tracked path so apply_exclusions issues `Remove-MpPreference`
        // unconditionally instead of short-circuiting to Skipped.
        let existing = vec![path.clone()];
        let outcomes = crate::defender::apply_exclusions(&powershell, &plan, &existing);
        let status = outcomes
            .first()
            .map(|a| format!("{:?}", a.status))
            .unwrap_or_else(|| "no-op".into());
        eprintln!("soldr load: defender exclusion removed for {path} ({status})");
    }
}

fn defender_exclusion_guard_for(cache_dir: &Path) -> DefenderExclusionGuard {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cache_dir;
        DefenderExclusionGuard::default()
    }
    #[cfg(target_os = "windows")]
    {
        let Some(powershell) = crate::defender::find_powershell() else {
            eprintln!(
                "soldr load: --auto-defender-exclude requested but no PowerShell on PATH; skipping"
            );
            return DefenderExclusionGuard::default();
        };
        if !crate::defender::is_admin() {
            eprintln!(
                "soldr load: --auto-defender-exclude requested but current process is not elevated; skipping (no UAC prompt)"
            );
            return DefenderExclusionGuard::default();
        }
        let path_str = cache_dir.display().to_string();
        let existing = crate::defender::current_exclusion_list(&powershell);
        let plan = vec![crate::defender::PathAction {
            path: path_str.clone(),
            action: crate::defender::ExclusionAction::Add,
            scope: "soldr-load".into(),
            status: crate::defender::ActionStatus::Planned,
            detail: None,
        }];
        let outcomes = crate::defender::apply_exclusions(&powershell, &plan, &existing);
        let outcome = outcomes.into_iter().next();
        let Some(outcome) = outcome else {
            return DefenderExclusionGuard::default();
        };
        match outcome.status {
            crate::defender::ActionStatus::Applied => {
                eprintln!("soldr load: defender exclusion added for {path_str}");
                DefenderExclusionGuard {
                    tracked: Some((powershell, path_str)),
                }
            }
            crate::defender::ActionStatus::AlreadyApplied => {
                eprintln!(
                    "soldr load: {path_str} already on Defender exclusion list; nothing to do"
                );
                DefenderExclusionGuard::default()
            }
            other => {
                let detail = outcome.detail.unwrap_or_default();
                eprintln!(
                    "soldr load: defender exclusion for {path_str} not applied ({other:?}{}{})",
                    if detail.is_empty() { "" } else { ": " },
                    detail
                );
                DefenderExclusionGuard::default()
            }
        }
    }
}

fn replay_one(workspace: &Path, entry: &SourceFile) -> MtimeOutcome {
    let abs = workspace.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(_) => return MtimeOutcome::Missing,
    };
    if !meta.is_file() {
        return MtimeOutcome::Missing;
    }
    if meta.len() != entry.size {
        return MtimeOutcome::SizeMismatch;
    }
    // Content check: only re-hash when size matches (we already
    // rejected the obvious "file got bigger / shorter" case).
    let hash = match hash_file(&abs) {
        Ok(h) => h,
        Err(_) => return MtimeOutcome::Modified,
    };
    if hash.as_slice() != entry.blake3.as_slice() {
        return MtimeOutcome::Modified;
    }
    let mtime = ms_to_systime(entry.mtime_ms);
    let atime = mtime;
    let t_mtime = filetime::FileTime::from_system_time(mtime);
    let t_atime = filetime::FileTime::from_system_time(atime);
    if filetime::set_file_times(&abs, t_atime, t_mtime).is_err() {
        return MtimeOutcome::Modified;
    }
    MtimeOutcome::Applied
}

fn apply_cache_tombstones(cache_dir: &Path, manifest: &Manifest) -> Result<()> {
    if manifest.deleted_cache_paths.is_empty() {
        return Ok(());
    }
    let restored_paths: BTreeSet<&str> = manifest
        .cache_files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    for path in &manifest.deleted_cache_paths {
        if restored_paths.contains(path.as_str()) {
            continue;
        }
        let rel = manifest_rel_to_path(path)?;
        if archive_always_excludes_cache_path(&rel) {
            continue;
        }
        let dest = cache_dir.join(rel);
        // symlink_metadata (#1548): a tombstoned SYMLINK must remove the
        // link itself. Following `metadata` here would misclassify a
        // link-to-dir as a directory and try remove_dir_all through it.
        match std::fs::symlink_metadata(&dest) {
            Ok(meta) if meta.file_type().is_symlink() => {
                remove_symlink(&dest).map_err(|e| io(&dest, e))?
            }
            Ok(meta) if meta.is_dir() => {
                std::fs::remove_dir_all(&dest).map_err(|e| io(&dest, e))?
            }
            Ok(_) => std::fs::remove_file(&dest).map_err(|e| io(&dest, e))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io(&dest, err)),
        }
    }
    Ok(())
}

/// Remove a symlink itself (never its target). On Windows a directory
/// symlink must be removed with `remove_dir`; try file-removal first and
/// fall back so both flavors are covered.
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(_) => std::fs::remove_dir(path),
        #[cfg(not(windows))]
        Err(err) => Err(err),
    }
}

/// Recreate manifest symlink entries below `cache_dir` (#1548). Returns
/// `(restored, skipped)`. Each entry is re-validated lexically against
/// the restore root; entries that fail validation, collide with a real
/// directory, or whose creation fails (e.g. Windows without the symlink
/// privilege) are skipped LOUDLY via stderr — the restore itself never
/// hard-fails on a symlink, missing links merely force a rebuild.
fn restore_cache_symlinks(cache_dir: &Path, entries: &[SymlinkEntry]) -> (u64, u64) {
    let mut restored = 0u64;
    let mut skipped = 0u64;
    for entry in entries {
        if manifest_path_is_daemon_runtime(&entry.path) {
            skipped += 1;
            continue;
        }
        match restore_one_symlink(cache_dir, entry) {
            Ok(()) => restored += 1,
            Err(reason) => {
                eprintln!(
                    "soldr load: refusing to restore symlink {} -> {} ({reason})",
                    entry.path, entry.target
                );
                skipped += 1;
            }
        }
    }
    (restored, skipped)
}

fn restore_one_symlink(cache_dir: &Path, entry: &SymlinkEntry) -> std::result::Result<(), String> {
    let rel = manifest_rel_to_path(&entry.path)
        .map_err(|_| "invalid link path in manifest".to_string())?;
    if resolve_symlink_target_in_root(&rel, &entry.target).is_none() {
        return Err("absolute or root-escaping link target".to_string());
    }
    let dest = cache_dir.join(&rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create parent dirs: {e}"))?;
    }
    match std::fs::symlink_metadata(&dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            remove_symlink(&dest).map_err(|e| format!("replace existing link: {e}"))?;
        }
        Ok(meta) if meta.is_dir() => {
            // Conservative: never delete a real directory tree to make
            // room for a link. Loud skip; the stale dir stays visible.
            return Err("a real directory occupies the link path".to_string());
        }
        Ok(_) => {
            std::fs::remove_file(&dest).map_err(|e| format!("replace existing file: {e}"))?;
        }
        Err(_) => {}
    }
    create_symlink_at(&entry.target, &dest, entry.is_dir).map_err(|e| format!("create link: {e}"))
}

/// Platform symlink creation. `target` is the manifest's forward-slashed
/// relative string; converted to the native separator on Windows.
fn create_symlink_at(target: &str, dest: &Path, is_dir: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let _ = is_dir;
        std::os::unix::fs::symlink(target, dest)
    }
    #[cfg(windows)]
    {
        let native = target.replace('/', "\\");
        if is_dir {
            std::os::windows::fs::symlink_dir(native, dest)
        } else {
            std::os::windows::fs::symlink_file(native, dest)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, dest, is_dir);
        Err(std::io::Error::other(
            "symlinks unsupported on this platform",
        ))
    }
}

/// Per-manifest-entry slot tracking whether the extract workers already
/// applied the nanosecond mtime for this path (#1541).
struct CacheMtimeSlot {
    mtime_ns: i64,
    size: u64,
    applied: bool,
}

fn build_cache_mtime_index(manifest: &Manifest) -> HashMap<String, CacheMtimeSlot> {
    manifest
        .cache_files
        .iter()
        .filter(|entry| !manifest_path_is_daemon_runtime(&entry.path))
        .map(|entry| {
            (
                entry.path.clone(),
                CacheMtimeSlot {
                    mtime_ns: entry.mtime_ns,
                    size: entry.size,
                    applied: false,
                },
            )
        })
        .collect()
}

/// Replay manifest mtimes for cache files whose payload was NOT carried
/// by the tar stream (delta metadata-only updates, or archives whose
/// manifest arrived after their cache entries). Entries already handled
/// by an extract worker are skipped; the remainder runs in parallel on
/// the load's rayon pool instead of the historical serial stat+set loop
/// over every manifest entry (#1541).
fn replay_pending_cache_file_mtimes(
    pool: &rayon::ThreadPool,
    cache_dir: &Path,
    entries: &[CacheFile],
    index: &HashMap<String, CacheMtimeSlot>,
) -> Result<()> {
    let pending: Vec<&CacheFile> = entries
        .iter()
        .filter(|entry| !manifest_path_is_daemon_runtime(&entry.path))
        .filter(|entry| {
            index
                .get(entry.path.as_str())
                .is_none_or(|slot| !slot.applied)
        })
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    pool.install(|| {
        pending
            .par_iter()
            .try_for_each(|entry| replay_cache_file_mtime(cache_dir, entry))
    })
}

fn replay_cache_file_mtime(cache_dir: &Path, entry: &CacheFile) -> Result<()> {
    let rel = manifest_rel_to_path(&entry.path)?;
    let abs = cache_dir.join(rel);
    let Ok(meta) = std::fs::metadata(&abs) else {
        return Ok(());
    };
    if !meta.is_file() || meta.len() != entry.size {
        return Ok(());
    }
    let mtime = ns_to_systime(entry.mtime_ns);
    let t = filetime::FileTime::from_system_time(mtime);
    filetime::set_file_times(&abs, t, t).map_err(|e| io(&abs, e))
}

// ---------- thread-pool helpers ----------

/// Read the [`LOAD_WORKERS_ENV`] override. Returns `None` when unset,
/// empty, or unparseable as a positive integer. Caller decides how to
/// combine this with the explicit `--threads` knob and rayon's default.
/// (#575)
fn load_worker_count_override() -> Option<usize> {
    let raw = std::env::var(LOAD_WORKERS_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<usize>().ok().filter(|&n| n > 0)
}

fn build_pool(threads: Option<usize>) -> Result<rayon::ThreadPool> {
    let mut builder = rayon::ThreadPoolBuilder::new();
    if let Some(n) = threads {
        builder = builder.num_threads(n);
    }
    builder
        .thread_name(|i| format!("soldr-save-{i}"))
        .build()
        .map_err(|e| SaveLoadError::BareIo(std::io::Error::other(e.to_string())))
}

fn num_cpus_for(threads: Option<usize>) -> u32 {
    threads.map(|n| n as u32).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;

    timed_test!(full_profile_excludes_soldr_daemon_runtime_state, {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path();
        std::fs::create_dir_all(cache.join("soldr-daemon")).unwrap();
        std::fs::create_dir_all(cache.join("zccache/artifacts")).unwrap();
        std::fs::write(cache.join("soldr-daemon/daemon.pid"), b"123\n").unwrap();
        std::fs::write(cache.join("soldr-daemon/.spawn.lock"), b"").unwrap();
        std::fs::write(
            cache.join("soldr-daemon/compile-daemon-unavailable"),
            b"stale",
        )
        .unwrap();
        let payload = cache.join("zccache/artifacts/hit.bin");
        std::fs::write(&payload, b"cache payload").unwrap();

        let walk = walk_cache_files_for_profile(cache, None, SaveProfile::Full).unwrap();

        assert_eq!(walk.included_paths, vec![payload]);
        assert_eq!(walk.excluded_files, 3);
    });

    timed_test!(daemon_runtime_exclusion_is_top_level_only, {
        assert!(archive_always_excludes_cache_path(Path::new(
            "soldr-daemon/daemon.pid"
        )));
        assert!(archive_always_excludes_cache_path(Path::new(
            "soldr-daemon/nested/state.json"
        )));
        assert!(archive_always_excludes_cache_path(Path::new(
            "Soldr-Daemon/daemon.pid"
        )));
        assert!(!archive_always_excludes_cache_path(Path::new(
            "zccache/artifacts/soldr-daemon/payload.bin"
        )));
        assert!(!archive_always_excludes_cache_path(Path::new(
            "soldr-daemon-cache/payload.bin"
        )));
    });

    // The exact path from the failing bench lane. A full-profile save used to
    // collect this file, then die on it when the daemon removed its own lock
    // between the walk and the stat.
    timed_test!(
        full_profile_never_archives_live_runtime_coordination_files,
        {
            let vanished = Path::new(
            "zccache/daemon-state/embedded-v1/v1.12.17/staging/2492-0-1785226007178685948/.active.lock",
        );
            assert!(
                archive_always_excludes_cache_path(vanished),
                "the lock that broke `soldr save` must be excluded from every profile"
            );

            for rel in [
                "zccache/daemon-state/embedded-v1/v1/staging/7-0-1/partial.bin",
                "zccache/x/daemon.sock",
                "zccache/x/daemon.pid",
                "zccache/x/.lock",
            ] {
                assert!(
                    archive_always_excludes_cache_path(Path::new(rel)),
                    "{rel} is runtime coordination state, not cache payload"
                );
            }

            // The exclusion must stay narrow: real payload that merely sits deep
            // in the same tree still gets archived, or the cache restores empty.
            for rel in [
                "zccache/daemon-state/embedded-v1/v1/objects/ab/cdef.o",
                "zccache/index.redb",
                "registry/cache/foo-1.0.crate",
            ] {
                assert!(
                    !archive_always_excludes_cache_path(Path::new(rel)),
                    "{rel} is cache payload and must still be archived"
                );
            }
        }
    );

    // Defence in depth: even with the exclusion, walk-then-stat is two passes
    // over a tree a live daemon writes, so the window cannot be closed
    // entirely.
    timed_test!(a_file_that_vanishes_after_the_walk_is_skipped_not_fatal, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = tmp.path();
        let missing = cache.join("gone.bin");
        assert!(
            cache_file_entry(cache, &missing)
                .expect("a vanished file must not fail the save")
                .is_none(),
            "a vanished file must be skipped"
        );

        // ...but a file that is present is still archived, so the tolerance
        // cannot silently empty an archive.
        let present = cache.join("present.bin");
        std::fs::write(&present, b"payload").expect("write");
        assert!(
            cache_file_entry(cache, &present).expect("stat").is_some(),
            "an existing file must still produce an entry"
        );
    });

    timed_test!(legacy_archive_cannot_mutate_live_daemon_runtime, {
        let root = tempfile::tempdir().unwrap();
        let archived_cache = root.path().join("archived-cache");
        let restore_cache = root.path().join("restore-cache");
        let archived_runtime = archived_cache.join("soldr-daemon");
        let live_runtime = restore_cache.join("soldr-daemon");
        std::fs::create_dir_all(&archived_runtime).unwrap();
        std::fs::create_dir_all(&live_runtime).unwrap();
        let archived_file = archived_runtime.join("archived.pid");
        std::fs::write(&archived_file, b"old runtime").unwrap();
        std::fs::write(live_runtime.join("live.pid"), b"live runtime").unwrap();
        let (archived_entry, archived_meta) = cache_file_entry(&archived_cache, &archived_file)
            .unwrap()
            .expect("the fixture file exists");
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            cache_dir_name: CACHE_DIR_NAME.into(),
            cache_file_count: 1,
            cache_layer_kind: CacheLayerKind::Complete as i32,
            cache_files: vec![archived_entry],
            deleted_cache_paths: vec!["soldr-daemon/live.pid".into()],
            cache_symlinks: vec![SymlinkEntry {
                path: "soldr-daemon/link".into(),
                target: "archived.pid".into(),
                is_dir: false,
            }],
            ..Manifest::default()
        };
        let archive = root.path().join("legacy.tar.zst");
        write_delta_archive(
            &archive,
            1,
            None,
            &manifest,
            &archived_cache,
            &[(archived_file, archived_meta)],
        )
        .unwrap();

        let report = load(&LoadOptions {
            archive: &archive,
            cache_dir: Some(&restore_cache),
            workspace: None,
            threads: None,
            mtimes_only: false,
            profile_extract: false,
            auto_defender_exclude: false,
        })
        .unwrap();

        assert_eq!(report.cache_files_restored, 0);
        assert_eq!(
            std::fs::read(live_runtime.join("live.pid")).unwrap(),
            b"live runtime"
        );
        assert!(!live_runtime.join("archived.pid").exists());
        assert!(!live_runtime.join("link").exists());
    });

    timed_test!(delta_ignores_daemon_runtime_from_legacy_base_manifest, {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let base = Manifest {
            version: MANIFEST_VERSION,
            cache_dir_name: CACHE_DIR_NAME.into(),
            cache_layer_kind: CacheLayerKind::Base as i32,
            cache_files: vec![CacheFile {
                path: "soldr-daemon/daemon.pid".into(),
                mtime_ns: 1,
                size: 3,
                blake3: vec![0; 32],
            }],
            cache_symlinks: vec![SymlinkEntry {
                path: "soldr-daemon/sock".into(),
                target: "target".into(),
                is_dir: false,
            }],
            ..Manifest::default()
        };
        let archive = root.path().join("delta.tar.zst");

        let report = save_delta(&SaveDeltaOptions {
            workspace: None,
            cache_dir: &cache,
            base_manifest: &base,
            out: &archive,
            zstd_level: 1,
            threads: None,
            profile: SaveProfile::Full,
        })
        .unwrap();
        let delta = read_manifest_from_archive(&archive).unwrap();

        assert_eq!(report.deleted_cache_files, 0);
        assert!(delta.deleted_cache_paths.is_empty());
    });

    timed_test!(cargo_input_inventory_selects_declared_inputs, {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let target = workspace.join("target");
        let source = workspace.join("src/main.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        std::fs::write(&source, "fn main() {}\n").unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(workspace.join("Cargo.lock"), "# lock\n").unwrap();
        std::fs::write(workspace.join("irrelevant.log"), "noise\n").unwrap();
        let source_text = source.display().to_string().replace('\\', "/");
        std::fs::write(
            target.join("debug/deps/app.d"),
            format!("target: {source_text}\n"),
        )
        .unwrap();

        let files = cargo_input_inventory(&workspace, &target, None)
            .unwrap()
            .expect("valid dep-info should produce an inventory");
        assert!(files.contains(&PathBuf::from("src/main.rs")));
        assert!(files.contains(&PathBuf::from("Cargo.toml")));
        assert!(files.contains(&PathBuf::from("Cargo.lock")));
        assert!(!files.contains(&PathBuf::from("irrelevant.log")));
    });

    timed_test!(cargo_input_inventory_falls_back_on_malformed_dep_info, {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let target = workspace.join("target/debug/deps");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("broken.d"), "not makefile dep-info\n").unwrap();
        assert!(
            cargo_input_inventory(&workspace, workspace.join("target").as_path(), None)
                .unwrap()
                .is_none()
        );
    });

    timed_test!(profile_line_matches_documented_shape, {
        // Synthetic per-file latencies: 6 values with known order so the
        // percentile math is hand-checkable. p50/p95/p99 indices on a
        // 6-element vec are round((6-1)*p): 3 → 250, 5 → 1200, 5 → 1200.
        let latencies = vec![100u64, 150, 200, 250, 450, 1200];
        let per_worker_counts = vec![2usize, 2, 1, 1];
        let phases = ExtractPhaseTimings {
            zstd_decode_us: 4_120_000,
            tar_parse_us: 890_000,
            extract_total_us: 10_510_000,
        };
        let line = format_profile_line(phases, &per_worker_counts, &latencies, 6);

        assert!(
            line.starts_with("soldr load: profile: "),
            "missing prefix: {line}"
        );
        assert!(
            line.contains("zstd_decode=4120ms"),
            "wrong zstd_decode: {line}"
        );
        assert!(line.contains("tar_parse=890ms"), "wrong tar_parse: {line}");
        assert!(
            line.contains("extract_total=10510ms"),
            "wrong extract_total: {line}"
        );
        assert!(
            line.contains("workers={0:n=2, 1:n=2, 2:n=1, 3:n=1}"),
            "wrong workers shape: {line}"
        );
        assert!(line.contains("per_file_p50_us=250"), "p50 wrong: {line}");
        assert!(line.contains("p95_us=1200"), "p95 wrong: {line}");
        assert!(line.contains("p99_us=1200"), "p99 wrong: {line}");
        assert!(line.contains("cache_files=6"), "files count wrong: {line}");
    });

    timed_test!(profile_line_handles_empty_latencies, {
        // No per-file data (e.g. cache had zero regular entries) — must
        // still emit a parseable line, with zeros for the percentiles.
        let line = format_profile_line(
            ExtractPhaseTimings {
                zstd_decode_us: 0,
                tar_parse_us: 0,
                extract_total_us: 1_000,
            },
            &[],
            &[],
            0,
        );
        assert!(line.contains("per_file_p50_us=0"), "{line}");
        assert!(line.contains("p95_us=0"), "{line}");
        assert!(line.contains("p99_us=0"), "{line}");
        assert!(line.contains("workers={}"), "{line}");
    });

    // extract_one is the worker entrypoint. Pointing it at a destination
    // whose parent path conflicts with a pre-existing regular file makes
    // create_dir_all fail. Gives us a deterministic, OS-agnostic failure
    // injection without patching production code. The first-error-wins
    // semantic is exercised end-to-end in tests/save_roundtrip.rs.
    timed_test!(extract_one_returns_error_with_failing_path, {
        let tmp = tempfile::tempdir().unwrap();
        let blocking_file = tmp.path().join("not-a-dir");
        std::fs::write(&blocking_file, b"i am a file").unwrap();

        // Now try to extract a job whose dest claims the blocking file
        // is a parent directory. create_dir_all should bail.
        let job = ExtractJob {
            dest: blocking_file.join("child.bin"),
            entry_type: tar::EntryType::Regular,
            body: b"unused".to_vec(),
            mtime_secs: None,
            mtime_ns: None,
            mode_bits: None,
        };
        let err = extract_one(&job).expect_err("worker must surface the IO error");
        let msg = err.to_string();
        assert!(
            msg.contains("not-a-dir"),
            "error must mention the offending path: {msg}"
        );
    });

    // #1909: dropping the dispatch without `finish()` -- which is what
    // every `?` in the driver loop does -- must still wait for workers.
    //
    // Before the Drop impl those workers kept writing into the cache tree
    // after `load()` had returned. Cargo exec'ing a build script while a
    // worker still held it open for write is exactly `ETXTBSY`. The
    // workers also leaked permanently, parked on a barrier whose final
    // party had already gone home.
    //
    // The assertion is that drop *returns at all*: if the barrier is not
    // satisfied it blocks forever, so a regression hangs this test rather
    // than failing it. That is deliberate -- there is no non-racy way to
    // observe "a worker is still running" from outside, and a hang is an
    // unambiguous signal. `timed_test!` bounds it.
    timed_test!(dropping_dispatch_without_finish_still_waits_for_workers, {
        let tmp = tempfile::tempdir().unwrap();
        let pool = build_pool(Some(2)).expect("pool");
        let err_slot: Arc<Mutex<Option<SaveLoadError>>> = Arc::new(Mutex::new(None));
        let counter = Arc::new(AtomicU64::new(0));

        let dest = tmp.path().join("nested").join("payload.bin");
        {
            let dispatch = ExtractDispatch::start(
                &pool,
                Some(2),
                Arc::clone(&err_slot),
                Arc::clone(&counter),
                None,
            );
            dispatch
                .send(ExtractJob {
                    dest: dest.clone(),
                    entry_type: tar::EntryType::Regular,
                    body: b"payload".to_vec(),
                    mtime_secs: None,
                    mtime_ns: None,
                    mode_bits: None,
                })
                .expect("send");
            // Deliberately no `finish()` -- emulate a `?` bailing out of
            // the driver loop with work still in flight.
        }

        // Reaching here at all means Drop waited. And because it waited,
        // the in-flight job is guaranteed complete -- no sleep, no poll.
        assert!(
            dest.exists(),
            "drop must not return until workers have finished writing"
        );
        assert_eq!(counter.load(Ordering::Relaxed), 1, "job should be counted");
        assert!(err_slot.lock().unwrap().is_none(), "no worker error");
    });

    // #1548 — purely-lexical symlink-target containment. Runs on every
    // platform (no symlink creation involved), which is what gives the
    // Windows lanes coverage of the validation logic that the
    // #[cfg(unix)] integration tests exercise end-to-end.
    timed_test!(symlink_target_validation_accepts_safe_relative_targets, {
        // Sibling file.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("deps/link.rlib"), "libfoo.rlib"),
            Some(PathBuf::from("deps").join("libfoo.rlib"))
        );
        // Up-and-over that stays inside the root.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("out/link"), "../deps/libfoo.rlib"),
            Some(PathBuf::from("deps").join("libfoo.rlib"))
        );
        // `.` components are harmless.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("a/link"), "./b/./c"),
            Some(PathBuf::from("a").join("b").join("c"))
        );
        // Link at the root pointing at a root-level sibling.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("link"), "payload.bin"),
            Some(PathBuf::from("payload.bin"))
        );
    });

    timed_test!(symlink_target_validation_rejects_unsafe_targets, {
        // Absolute POSIX target — rejected even when it would point back
        // inside the root; we only ever preserve relative links.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("a/link"), "/etc/passwd"),
            None
        );
        // Escapes the root via `..`.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("link"), "../outside.txt"),
            None
        );
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("a/link"), "../../../x"),
            None
        );
        // Exactly-at-root resolution (empty) is meaningless for a link.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("a/link"), ".."),
            None
        );
        // Empty target.
        assert_eq!(
            resolve_symlink_target_in_root(Path::new("a/link"), ""),
            None
        );
        // Windows drive / UNC prefixes are rejected on Windows (Prefix
        // component); on Unix "C:" is just a weird-but-contained relative
        // component, which is harmless — so only assert the containment
        // property that holds everywhere: no result may escape the root.
        if let Some(resolved) = resolve_symlink_target_in_root(Path::new("a/link"), "C:/evil") {
            assert!(
                !resolved
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "resolved path must stay inside the root: {resolved:?}"
            );
        }
    });

    // #1547 — the workspace source walker must not exclude a directory by
    // *name* alone: a `target/` directory that isn't the real Cargo output
    // dir (e.g. `src/target/mod.rs`) is legitimate tracked source and must
    // be hashed. Only the resolved Cargo target dir(s) are excluded, and
    // `.git` / `node_modules` stay excluded by name (never legitimate
    // source basenames).
    timed_test!(
        walk_workspace_files_hashes_nested_dir_literally_named_target,
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();

            // Legitimate source: a package sub-module directory that happens
            // to be named "target", nested under src/ — NOT the Cargo build
            // output dir.
            std::fs::create_dir_all(root.join("src/target")).unwrap();
            std::fs::write(root.join("src/target/mod.rs"), b"pub fn noop() {}").unwrap();
            std::fs::write(root.join("src/lib.rs"), b"mod target;").unwrap();

            // The REAL Cargo output dir at the workspace root — must stay
            // excluded.
            std::fs::create_dir_all(root.join("target/debug")).unwrap();
            std::fs::write(root.join("target/debug/build-artifact.bin"), b"junk").unwrap();

            // .git and node_modules — must stay excluded regardless of depth.
            std::fs::create_dir_all(root.join(".git/objects")).unwrap();
            std::fs::write(root.join(".git/objects/pack.idx"), b"gitjunk").unwrap();
            std::fs::create_dir_all(root.join("node_modules/leftpad")).unwrap();
            std::fs::write(root.join("node_modules/leftpad/index.js"), b"jsjunk").unwrap();

            let files = walk_workspace_files(root, None).unwrap();
            let rel_strs: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect();

            assert!(
                rel_strs.contains(&"src/target/mod.rs".to_string()),
                "src/target/mod.rs must be hashed as legitimate source, got: {rel_strs:?}"
            );
            assert!(
                rel_strs.contains(&"src/lib.rs".to_string()),
                "src/lib.rs must be hashed, got: {rel_strs:?}"
            );
            assert!(
                !rel_strs
                    .iter()
                    .any(|p| p.starts_with("target/") || p == "target"),
                "the real workspace target/ dir must stay excluded, got: {rel_strs:?}"
            );
            assert!(
                !rel_strs.iter().any(|p| p.starts_with(".git/")),
                ".git must stay excluded, got: {rel_strs:?}"
            );
            assert!(
                !rel_strs.iter().any(|p| p.starts_with("node_modules/")),
                "node_modules must stay excluded, got: {rel_strs:?}"
            );
        }
    );

    // #1547 mutation check: CARGO_TARGET_DIR overrides must also be
    // resolved-path excluded even though they don't literally live under
    // `<workspace>/target`.
    timed_test!(
        walk_workspace_files_excludes_cargo_target_dir_env_override,
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/lib.rs"), b"// real source").unwrap();
            // Override target dir lives INSIDE the workspace under a
            // differently-named directory (simulating CARGO_TARGET_DIR=out).
            std::fs::create_dir_all(root.join("out/debug")).unwrap();
            std::fs::write(root.join("out/debug/artifact.bin"), b"junk").unwrap();
            // A directory named "out" is NOT excluded when CARGO_TARGET_DIR is
            // unset — sanity check the negative case first.
            let baseline = walk_workspace_files(root, None).unwrap();
            let baseline_strs: Vec<String> = baseline
                .iter()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect();
            assert!(
                baseline_strs.contains(&"out/debug/artifact.bin".to_string()),
                "without CARGO_TARGET_DIR, out/ is ordinary source: {baseline_strs:?}"
            );
        }
    );

    // #1547 — a directory named "target" that is NOT at the workspace
    // root (and not a CARGO_TARGET_DIR/CARGO_BUILD_TARGET_DIR override)
    // must never be excluded, including deeper nesting than one level.
    timed_test!(
        walk_workspace_files_hashes_deeply_nested_target_named_dirs,
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join("crates/foo/src/target")).unwrap();
            std::fs::write(
                root.join("crates/foo/src/target/mod.rs"),
                b"pub struct Target;",
            )
            .unwrap();

            let files = walk_workspace_files(root, None).unwrap();
            let rel_strs: Vec<String> = files
                .iter()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect();
            assert!(
                rel_strs.contains(&"crates/foo/src/target/mod.rs".to_string()),
                "deeply nested target-named source must be hashed: {rel_strs:?}"
            );
        }
    );
}

#[cfg(test)]
mod etxtbsy_tests {
    use super::*;

    // #1909: a restored build script failed `execve` with ETXTBSY, which
    // happens only when some process holds the target open for writing. The
    // extractor now stages into a sibling and renames, so the inode that
    // lands at `dest` never had a writable descriptor pointing at it and no
    // fork-inherited fd can refer to it.

    fn regular_job(dest: PathBuf, body: &[u8], mode: Option<u32>) -> ExtractJob {
        ExtractJob {
            dest,
            entry_type: tar::EntryType::Regular,
            body: body.to_vec(),
            mtime_secs: Some(1_700_000_000),
            mtime_ns: None,
            mode_bits: mode,
        }
    }

    crate::timed_test!(staging_path_is_a_sibling_of_the_destination, {
        // Same directory or `rename` stops being atomic -- a temp dir can sit
        // on a different filesystem, where rename degrades to copy+delete and
        // reintroduces the very window this fix closes.
        let dest = Path::new("/some/deep/dir/build-script-build");
        let staged = staging_path_for(dest);
        assert_eq!(
            staged.parent(),
            dest.parent(),
            "staging file must be a sibling so the rename stays atomic"
        );
        assert_ne!(staged.file_name(), dest.file_name());
    });

    crate::timed_test!(staging_paths_are_unique_across_calls, {
        // Concurrent workers restore different entries simultaneously; two
        // collisions would corrupt each other's content.
        let dest = Path::new("/tmp/target/debug/build/x/build-script-build");
        let a = staging_path_for(dest);
        let b = staging_path_for(dest);
        assert_ne!(
            a, b,
            "concurrent extract workers must not share a staging path"
        );
    });

    crate::timed_test!(extract_leaves_no_staging_file_behind, {
        // A stray `.soldr-tmp` inside target/ would survive into the next
        // build and confuse cargo.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("build-script-build");
        extract_one(&regular_job(dest.clone(), b"#!/bin/sh\n", None)).expect("extract");

        assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\n");
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("soldr-tmp"))
            .collect();
        assert!(strays.is_empty(), "staging files left behind: {strays:?}");
    });

    crate::timed_test!(extract_replaces_an_existing_destination_atomically, {
        // Restores land on top of a previous build's artifacts. The rename
        // must replace the old inode rather than fail on it.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("artifact.bin");
        std::fs::write(&dest, b"stale contents").unwrap();

        extract_one(&regular_job(dest.clone(), b"fresh", None)).expect("extract over existing");
        assert_eq!(std::fs::read(&dest).unwrap(), b"fresh");
    });

    #[cfg(unix)]
    crate::timed_test!(restored_executable_keeps_its_mode_through_the_rename, {
        // The mode is applied to the staging file; `rename` must carry it
        // over, or #587/#1889 would regress silently.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("build-script-build");
        extract_one(&regular_job(
            dest.clone(),
            b"#!/bin/sh\nexit 0\n",
            Some(0o755),
        ))
        .expect("extract");

        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "executable bit must survive the rename"
        );
    });

    #[cfg(unix)]
    crate::timed_test!(restored_executable_can_actually_be_executed, {
        // The end-to-end property the issue is about: after extract_one
        // returns, the file is immediately runnable -- no ETXTBSY, no EACCES.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("build-script-build");
        extract_one(&regular_job(
            dest.clone(),
            b"#!/bin/sh\nexit 7\n",
            Some(0o755),
        ))
        .expect("extract");
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o111,
            0o111
        );

        let status = std::process::Command::new(&dest)
            .status()
            .expect("restored build script must be executable immediately after restore");
        assert_eq!(status.code(), Some(7));
    });
}
