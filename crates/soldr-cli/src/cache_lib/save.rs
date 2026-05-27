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

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

use prost::Message as _;
use std::path::{Path, PathBuf};
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

pub use proto::{CacheFile, CacheLayerKind, Manifest, SourceFile};

/// Well-known filename for the manifest inside an archive.
pub const MANIFEST_NAME: &str = "SOLDR_MANIFEST.pb";

/// Directory name for the bundled cache contents inside an archive.
pub const CACHE_DIR_NAME: &str = "cache";

/// Current manifest schema version. Bump only on breaking format changes.
pub const MANIFEST_VERSION: u32 = 1;

/// Default zstd compression level — matches what setup-soldr's TS impl
/// has been using (level 3 gives ~0.26 ratio on Rust artifact caches at
/// roughly 1 GB/s on a Linux CI runner).
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

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
    let f = File::open(path).map_err(|e| io(path, e))?;
    let mut reader = BufReader::with_capacity(64 * 1024, f);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Walk a workspace and return every regular file's repo-relative POSIX
/// path. We deliberately do NOT shell out to `git ls-files` — soldr's
/// users include sandboxed CI jobs and local-dev runs that don't always
/// have git on PATH at the moment this is invoked. `.git/`, `target/`,
/// and `node_modules/` are excluded — they're never source.
///
/// Uses jwalk for a parallel walk; on a 1000-file workspace this is
/// ~4x faster than walkdir at the directory-traversal level. The walk
/// itself is the cheap part — caller still has to hash each file.
fn walk_workspace_files(workspace: &Path, threads: Option<usize>) -> Result<Vec<PathBuf>> {
    let walker = jwalk::WalkDir::new(workspace)
        .follow_links(false)
        .skip_hidden(false) // we want .cargo, .rustfmt.toml, etc.
        .process_read_dir(move |_depth, _path, _read_dir_state, children| {
            children.retain(|res| match res {
                Ok(entry) => {
                    let name = entry.file_name.to_string_lossy();
                    !(entry.depth > 0
                        && (name == ".git" || name == "target" || name == "node_modules"))
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
        if !entry.file_type().is_file() {
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

/// Like `walk_workspace_files` but does NOT exclude `target/` (because
/// the cache dir itself is often called `cache/` or `zccache/` and we
/// want everything below it). Returns absolute paths.
fn walk_cache_files(cache_dir: &Path, threads: Option<usize>) -> Result<Vec<PathBuf>> {
    let walker = jwalk::WalkDir::new(cache_dir)
        .follow_links(false)
        .skip_hidden(false);
    let walker = match threads {
        Some(n) if n > 0 => walker.parallelism(jwalk::Parallelism::RayonNewPool(n)),
        _ => walker,
    };
    let mut out = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: cache_dir.to_path_buf(),
            message: err.to_string(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    Ok(out)
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

fn cache_file_entry(cache_dir: &Path, abs: &Path) -> Result<CacheFile> {
    let rel = abs
        .strip_prefix(cache_dir)
        .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
    let meta = std::fs::metadata(abs).map_err(|e| io(abs, e))?;
    let hash = hash_file(abs)?;
    Ok(CacheFile {
        path: rel_to_posix(rel),
        mtime_ns: mtime_ns(&meta),
        size: meta.len(),
        blake3: hash.to_vec(),
    })
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
}

#[derive(Debug, Clone, Default)]
pub struct SaveReport {
    pub source_files: u64,
    pub cache_files: u64,
    pub deleted_cache_files: u64,
    pub archive_bytes: u64,
    pub elapsed_ms: u64,
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
    let (source_result, cache_files_paths): (Result<Vec<SourceFile>>, Result<Vec<PathBuf>>) = pool
        .install(|| {
            rayon::join(
                || -> Result<Vec<SourceFile>> {
                    let Some(ws) = opts.workspace else {
                        return Ok(Vec::new());
                    };
                    let files = walk_workspace_files(ws, opts.threads)?;
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
                || -> Result<Vec<PathBuf>> {
                    if opts.mtimes_only {
                        return Ok(Vec::new());
                    }
                    match opts.cache_dir {
                        Some(dir) if dir.exists() => walk_cache_files(dir, opts.threads),
                        _ => Ok(Vec::new()),
                    }
                },
            )
        });
    let manifest_files = source_result?;
    let cache_files_paths = cache_files_paths?;
    let cache_manifest_files =
        build_cache_manifest_entries(&pool, opts.cache_dir, &cache_files_paths)?;

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
            for abs in &cache_files_paths {
                append_cache_file_entry(&mut tar_builder, cache_dir, abs)?;
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
        source_files: manifest.source_file_count,
        cache_files,
        deleted_cache_files: 0,
        archive_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

pub fn save_delta(opts: &SaveDeltaOptions<'_>) -> Result<SaveReport> {
    let start = std::time::Instant::now();
    let pool = build_pool(opts.threads)?;

    let (source_result, cache_files_paths): (Result<Vec<SourceFile>>, Result<Vec<PathBuf>>) = pool
        .install(|| {
            rayon::join(
                || -> Result<Vec<SourceFile>> {
                    let Some(ws) = opts.workspace else {
                        return Ok(Vec::new());
                    };
                    let files = walk_workspace_files(ws, opts.threads)?;
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
                || -> Result<Vec<PathBuf>> {
                    if opts.cache_dir.exists() {
                        walk_cache_files(opts.cache_dir, opts.threads)
                    } else {
                        Ok(Vec::new())
                    }
                },
            )
        });
    let manifest_files = source_result?;
    let cache_files_paths = cache_files_paths?;
    let cache_manifest_files =
        build_cache_manifest_entries(&pool, Some(opts.cache_dir), &cache_files_paths)?;

    let base_by_path: BTreeMap<&str, &CacheFile> = opts
        .base_manifest
        .cache_files
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let current_by_path: BTreeMap<&str, (&CacheFile, &PathBuf)> = cache_manifest_files
        .iter()
        .zip(cache_files_paths.iter())
        .map(|(entry, path)| (entry.path.as_str(), (entry, path)))
        .collect();

    let mut delta_entries = Vec::new();
    let mut delta_paths = Vec::new();
    for (path, (entry, abs)) in &current_by_path {
        match base_by_path.get(path) {
            Some(base) if cache_file_metadata_matches(base, entry) => {}
            Some(base) if cache_file_content_matches(base, entry) => {
                delta_entries.push((*entry).clone());
            }
            _ => {
                delta_entries.push((*entry).clone());
                delta_paths.push((*abs).clone());
            }
        }
    }

    let current_paths: BTreeSet<&str> = current_by_path.keys().copied().collect();
    let deleted_cache_paths: Vec<String> = base_by_path
        .keys()
        .copied()
        .filter(|path| !current_paths.contains(path))
        .map(ToOwned::to_owned)
        .collect();

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
        source_files: manifest.source_file_count,
        cache_files: manifest.cache_file_count,
        deleted_cache_files: manifest.deleted_cache_paths.len() as u64,
        archive_bytes,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

fn build_cache_manifest_entries(
    pool: &rayon::ThreadPool,
    cache_dir: Option<&Path>,
    cache_files_paths: &[PathBuf],
) -> Result<Vec<CacheFile>> {
    let Some(cache_dir) = cache_dir else {
        return Ok(Vec::new());
    };
    pool.install(|| {
        cache_files_paths
            .par_iter()
            .map(|abs| cache_file_entry(cache_dir, abs))
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

fn append_cache_file_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    cache_dir: &Path,
    abs: &Path,
) -> Result<()> {
    let rel = abs
        .strip_prefix(cache_dir)
        .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
    let mut archive_path = PathBuf::from(CACHE_DIR_NAME);
    archive_path.push(rel);
    let archive_path_str = rel_to_posix(&archive_path);
    let mut file = File::open(abs).map_err(|e| io(abs, e))?;
    let mut header = tar::Header::new_gnu();
    let meta = std::fs::metadata(abs).map_err(|e| io(abs, e))?;
    header.set_metadata(&meta);
    header.set_cksum();
    tar_builder
        .append_data(&mut header, archive_path_str, &mut file)
        .map_err(SaveLoadError::BareIo)
}

fn manifest_digest(manifest: &Manifest) -> Result<Vec<u8>> {
    Ok(blake3::hash(&encode_manifest(manifest)?)
        .as_bytes()
        .to_vec())
}

fn write_delta_archive(
    out: &Path,
    zstd_level: i32,
    threads: Option<usize>,
    manifest: &Manifest,
    cache_dir: &Path,
    cache_files_paths: &[PathBuf],
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

        for abs in cache_files_paths {
            append_cache_file_entry(&mut tar_builder, cache_dir, abs)?;
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
/// The implementation pipelines two operations:
///   1. Stream-decompress + extract tar entries (sequential — zstd's
///      read-side decoder is single-threaded by design).
///   2. Once the manifest is parsed (it's the FIRST entry in every
///      archive `save` produces), hand the mtime-replay work off to
///      the thread pool so it runs concurrently with the remaining
///      cache-file extraction.
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
    tar_reader.set_preserve_mtime(true);
    tar_reader.set_preserve_permissions(false);

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut manifest_decoded: Option<Manifest> = None;
    let mut cache_files_restored: u64 = 0;
    let pool = build_pool(opts.threads)?;
    // Holds the mtime-replay job once we've parsed the manifest and
    // dispatched the work onto rayon. We poll on it after the tar
    // stream is fully drained.
    let mut replay_handle: Option<std::sync::mpsc::Receiver<Vec<MtimeOutcome>>> = None;

    for entry in tar_reader.entries().map_err(SaveLoadError::BareIo)? {
        let mut entry = entry.map_err(SaveLoadError::BareIo)?;
        let path = entry.path().map_err(SaveLoadError::BareIo)?.into_owned();

        if path.as_os_str() == MANIFEST_NAME {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(SaveLoadError::BareIo)?;
            let manifest: Manifest = prost::Message::decode(&buf[..])?;
            if let Some(cache_dir) = opts.cache_dir {
                apply_cache_tombstones(cache_dir, &manifest)?;
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
        if entry.header().entry_type() == tar::EntryType::Directory {
            std::fs::create_dir_all(&dest).map_err(|e| io(&dest, e))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        entry.unpack(&dest).map_err(SaveLoadError::BareIo)?;
        if entry.header().entry_type() == tar::EntryType::Regular {
            cache_files_restored += 1;
        }
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
        replay_cache_file_mtimes(cache_dir, &manifest.cache_files)?;
    }

    // If we kicked off the replay early, wait for it. Otherwise (no
    // workspace, or first-run before manifest seen) run it inline
    // here for completeness.
    if let Some(rx) = replay_handle {
        let outcomes = rx
            .recv()
            .map_err(|_| SaveLoadError::BareIo(std::io::Error::other("replay worker dropped")))?;
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
        let dest = cache_dir.join(rel);
        match std::fs::metadata(&dest) {
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

fn replay_cache_file_mtimes(cache_dir: &Path, entries: &[CacheFile]) -> Result<()> {
    for entry in entries {
        let rel = manifest_rel_to_path(&entry.path)?;
        let abs = cache_dir.join(rel);
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue;
        };
        if !meta.is_file() || meta.len() != entry.size {
            continue;
        }
        let mtime = ns_to_systime(entry.mtime_ns);
        let t = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_times(&abs, t, t).map_err(|e| io(&abs, e))?;
    }
    Ok(())
}

// ---------- thread-pool helpers ----------

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
