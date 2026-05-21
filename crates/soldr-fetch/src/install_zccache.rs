//! `soldr install-zccache <SOURCE>` implementation.
//!
//! Pins a user-supplied set of three zccache binaries
//! (`zccache`, `zccache-daemon`, `zccache-fp`) into
//! `<SoldrPaths::bin>/zccache-pinned/` together with a `source.json`
//! sidecar. Once pinned, `fetch_zccache_with_paths` resolves the pinned
//! binaries ahead of the managed GitHub-Releases fetch so users no
//! longer have to pass `--zccache=system` on every invocation.
//!
//! Source forms:
//!   * `system` — search PATH for the three binaries
//!   * `<path>` — a directory or a `.zip` / `.tar.gz` / `.tgz` /
//!     `.tar.zst` archive containing the three binaries (any depth —
//!     release tarballs sometimes nest under a `zccache-vX.Y.Z/`
//!     directory)
//!   * `<url>` — `http://` or `https://` URL pointing at an archive
//!     that satisfies the path rules above
//!
//! Debug-info sidecars (`.pdb`, `.dwp`, `.dSYM`) are copied next to
//! every binary, mirroring the `SOLDR_ZCCACHE_LOCAL_DIR` behaviour.

use crate::{
    canonical_zccache_paths, copy_debug_info_sidecars, copy_if_changed, desired_binary_names,
    find_in_dirs, http_client, suppress_windows_console_window, MANAGED_ZCCACHE_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soldr_core::{SoldrError, SoldrPaths, TargetTriple};
use std::path::{Path, PathBuf};

/// Directory name (under `SoldrPaths::bin`) where the pinned zccache
/// binaries live.
pub const PINNED_ZCCACHE_DIRNAME: &str = "zccache-pinned";

/// File name (under the pinned dir) holding install provenance.
pub const PINNED_ZCCACHE_SIDECAR_FILENAME: &str = "source.json";

/// Schema version recorded in `source.json`. Bump on any breaking change.
pub const PINNED_ZCCACHE_SIDECAR_SCHEMA_VERSION: u32 = 1;

/// Canonical binary names (without platform extension). Mirrors
/// `MANAGED_ZCCACHE_PACKAGES` but exposes only the runtime side.
pub const ZCCACHE_PINNED_BINARY_NAMES: [&str; 3] = ["zccache", "zccache-daemon", "zccache-fp"];

/// Source the user passed to `install-zccache`.
#[derive(Debug, Clone)]
pub enum InstallSource {
    /// Search the system `PATH` for the three binaries.
    System,
    /// Local directory or archive file on disk.
    Path(PathBuf),
    /// HTTP / HTTPS URL pointing at an archive.
    Url(String),
}

impl InstallSource {
    /// Best-effort parse of the raw CLI string into a source variant.
    /// `system` → `System`; `http(s)://...` → `Url`; everything else
    /// is treated as a path. Empty input is rejected.
    pub fn parse(raw: &str) -> Result<Self, SoldrError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SoldrError::Other(
                "install-zccache: source must not be empty".into(),
            ));
        }
        if trimmed.eq_ignore_ascii_case("system") {
            return Ok(InstallSource::System);
        }
        if let Some(scheme_end) = trimmed.find("://") {
            let scheme = &trimmed[..scheme_end].to_ascii_lowercase();
            if scheme == "http" || scheme == "https" {
                return Ok(InstallSource::Url(trimmed.to_string()));
            }
        }
        Ok(InstallSource::Path(PathBuf::from(trimmed)))
    }

    /// Original string the user passed (e.g. `"system"`, `"/abs/path"`,
    /// `"https://..."`). Used to roundtrip into `source.json`.
    pub fn raw_value(&self) -> String {
        match self {
            InstallSource::System => "system".to_string(),
            InstallSource::Path(p) => p.display().to_string(),
            InstallSource::Url(u) => u.clone(),
        }
    }

    /// Stable label for the `source_kind` JSON field.
    pub fn kind_str(&self) -> &'static str {
        match self {
            InstallSource::System => "system",
            InstallSource::Path(_) => "path",
            InstallSource::Url(_) => "url",
        }
    }
}

/// Per-binary integrity record persisted in `source.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedBinaryRecord {
    pub sha256: String,
    pub size_bytes: u64,
}

/// Verbatim contents of `<pinned dir>/source.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedSidecar {
    pub schema_version: u32,
    pub source_kind: String,
    pub source_value: String,
    /// `zccache --version` output, parsed. `"unknown"` when the probe
    /// fails (still a valid install — surfaces in `--status`).
    pub version: String,
    pub binaries: std::collections::BTreeMap<String, PinnedBinaryRecord>,
    pub installed_at: String,
    pub soldr_version: String,
}

/// Pretty result of a successful install, returned to the CLI for
/// json/text rendering.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub install_dir: String,
    pub source_kind: String,
    pub source_value: String,
    pub version: String,
    pub binaries: std::collections::BTreeMap<String, PinnedBinaryRecord>,
    pub installed_at: String,
    pub soldr_version: String,
}

/// Resolved pinned-zccache binaries for `fetch_zccache_with_paths` and
/// `zccache_binary_summary`.
#[derive(Debug, Clone)]
pub struct PinnedResolution {
    pub binary_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub version: String,
}

/// Path to the pinned install dir for a given soldr root. The dir may
/// not exist yet; callers must check.
pub fn pinned_zccache_dir(paths: &SoldrPaths) -> PathBuf {
    paths.bin.join(PINNED_ZCCACHE_DIRNAME)
}

fn pinned_sidecar_path(paths: &SoldrPaths) -> PathBuf {
    pinned_zccache_dir(paths).join(PINNED_ZCCACHE_SIDECAR_FILENAME)
}

/// Read the pinned sidecar JSON. Returns `Ok(None)` when the pinned
/// dir doesn't exist or the sidecar is missing. A malformed sidecar is
/// a hard error so misconfigured installs don't silently fall back to
/// the managed fetch.
pub fn read_pinned_sidecar(paths: &SoldrPaths) -> Result<Option<PinnedSidecar>, SoldrError> {
    let sidecar = pinned_sidecar_path(paths);
    if !sidecar.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&sidecar).map_err(|e| {
        SoldrError::Other(format!(
            "install-zccache: failed to read sidecar {}: {e}",
            sidecar.display()
        ))
    })?;
    serde_json::from_slice::<PinnedSidecar>(&bytes)
        .map(Some)
        .map_err(|e| {
            SoldrError::Other(format!(
                "install-zccache: malformed sidecar {}: {e}",
                sidecar.display()
            ))
        })
}

/// Look up the pinned install (if any) and verify the three binaries
/// are present. Returns `Ok(None)` when no pinned install exists.
pub fn resolve_pinned_zccache(paths: &SoldrPaths) -> Result<Option<PinnedResolution>, SoldrError> {
    let target = TargetTriple::detect()?;
    resolve_pinned_zccache_for_target(paths, &target)
}

pub(crate) fn resolve_pinned_zccache_for_target(
    paths: &SoldrPaths,
    target: &TargetTriple,
) -> Result<Option<PinnedResolution>, SoldrError> {
    let Some(sidecar) = read_pinned_sidecar(paths)? else {
        return Ok(None);
    };
    let runtime_dir = pinned_zccache_dir(paths);
    let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, target);
    if !cli.exists() || !daemon.exists() || !fp.exists() {
        return Err(SoldrError::Other(format!(
            "install-zccache: sidecar at {} present but one or more binaries missing in {} \
             (run `soldr install-zccache --remove` to reset)",
            pinned_sidecar_path(paths).display(),
            runtime_dir.display()
        )));
    }
    Ok(Some(PinnedResolution {
        binary_path: cli,
        runtime_dir,
        version: sidecar.version,
    }))
}

/// Install the three zccache binaries from `source` into the pinned
/// directory and write the `source.json` sidecar. Existing pinned files
/// are overwritten so re-running `install-zccache` is idempotent.
pub async fn install_zccache_from_source(
    source: &InstallSource,
    paths: &SoldrPaths,
) -> Result<InstallReport, SoldrError> {
    let target = TargetTriple::detect()?;
    install_zccache_from_source_for_target(source, paths, &target).await
}

pub(crate) async fn install_zccache_from_source_for_target(
    source: &InstallSource,
    paths: &SoldrPaths,
    target: &TargetTriple,
) -> Result<InstallReport, SoldrError> {
    paths.ensure_dirs()?;
    let binary_ext = target.binary_ext();
    let names = desired_binary_names(&ZCCACHE_PINNED_BINARY_NAMES, target);

    // Resolve a staging directory whose layout has the three binaries
    // sitting next to each other (with their `.pdb` / `.dwp` / `.dSYM`
    // sidecars). For `system` PATH lookups each binary may live in a
    // different directory; we copy them into a unified staging dir so
    // downstream logic only sees one path per binary. The
    // `staging_holder` keeps the tempdir alive until the install
    // copy completes; tempfile::TempDir's Drop unlinks on exit
    // (success or error).
    let staging_holder: Option<tempfile::TempDir>;
    let staging_root: PathBuf = match source {
        InstallSource::System => {
            let holder = tempfile::tempdir_in(&paths.bin)?;
            let dir = stage_from_system_path(holder.path(), target)?;
            staging_holder = Some(holder);
            dir
        }
        InstallSource::Path(p) => {
            if !p.exists() {
                return Err(SoldrError::Other(format!(
                    "install-zccache: path does not exist: {}",
                    p.display()
                )));
            }
            if p.is_dir() {
                staging_holder = None;
                p.clone()
            } else if p.is_file() {
                let extract_dir = tempfile::tempdir_in(&paths.bin)?;
                extract_archive_file(p, extract_dir.path())?;
                let unified = tempfile::tempdir_in(&paths.bin)?;
                consolidate_extracted_binaries(extract_dir.path(), unified.path(), &names)?;
                drop(extract_dir);
                let dir = unified.path().to_path_buf();
                staging_holder = Some(unified);
                dir
            } else {
                return Err(SoldrError::Other(format!(
                    "install-zccache: {} is neither a regular file nor a directory",
                    p.display()
                )));
            }
        }
        InstallSource::Url(url) => {
            let download = tempfile::NamedTempFile::new_in(&paths.bin).map_err(|e| {
                SoldrError::Other(format!(
                    "install-zccache: failed to create download temp file: {e}"
                ))
            })?;
            download_url_to(url, download.path()).await?;
            let extract_dir = tempfile::tempdir_in(&paths.bin)?;
            extract_archive_file(download.path(), extract_dir.path())?;
            let unified = tempfile::tempdir_in(&paths.bin)?;
            consolidate_extracted_binaries(extract_dir.path(), unified.path(), &names)?;
            drop(extract_dir);
            drop(download);
            let dir = unified.path().to_path_buf();
            staging_holder = Some(unified);
            dir
        }
    };

    // Verify every binary is present in the staging dir.
    let mut staged: Vec<(String, PathBuf)> = Vec::with_capacity(names.len());
    for name in &names {
        let candidate = staging_root.join(name);
        if !candidate.is_file() {
            return Err(SoldrError::Other(missing_binary_error_message(
                source,
                p_str(&staging_root),
                name,
                binary_ext,
            )));
        }
        staged.push((name.clone(), candidate));
    }

    // Copy into the pinned dir.
    let pinned_dir = pinned_zccache_dir(paths);
    // Wipe any stale install first so leftover sidecars from a previous
    // schema can't cause a malformed-sidecar error on the next read.
    if pinned_dir.exists() {
        std::fs::remove_dir_all(&pinned_dir)?;
    }
    std::fs::create_dir_all(&pinned_dir)?;

    let mut binaries: std::collections::BTreeMap<String, PinnedBinaryRecord> =
        std::collections::BTreeMap::new();
    for (file_name, src) in &staged {
        let dst = pinned_dir.join(file_name);
        copy_if_changed(src, &dst)?;
        copy_debug_info_sidecars(src, &dst)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dst)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dst, perms)?;
        }

        let bytes = std::fs::read(&dst).map_err(|e| {
            SoldrError::Other(format!(
                "install-zccache: failed to read {} for sha256: {e}",
                dst.display()
            ))
        })?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = hex::encode(hasher.finalize());

        // Strip platform extension when keying the sidecar so the JSON
        // is portable across operating systems (a Windows install's
        // sidecar can be diffed against a Linux one).
        let key = strip_platform_ext(file_name);
        binaries.insert(
            key,
            PinnedBinaryRecord {
                sha256: digest,
                size_bytes: bytes.len() as u64,
            },
        );
    }

    // Probe the version. Best-effort: a binary that refuses to run
    // (corrupted, ABI mismatch) still yields a valid install — the
    // sidecar simply records `"unknown"` and the user finds out at
    // first cargo invocation.
    let (cli_binary, _) = staged
        .first()
        .map(|(name, src)| (pinned_dir.join(name), src))
        .ok_or_else(|| SoldrError::Other("install-zccache: no staged binaries".into()))?;
    let version = probe_zccache_version(&cli_binary);

    let installed_at = format_iso8601_utc(std::time::SystemTime::now());
    let sidecar = PinnedSidecar {
        schema_version: PINNED_ZCCACHE_SIDECAR_SCHEMA_VERSION,
        source_kind: source.kind_str().to_string(),
        source_value: source.raw_value(),
        version: version.clone(),
        binaries: binaries.clone(),
        installed_at: installed_at.clone(),
        soldr_version: soldr_core::version().to_string(),
    };
    let sidecar_path = pinned_sidecar_path(paths);
    let mut sidecar_bytes = serde_json::to_vec_pretty(&sidecar).map_err(|e| {
        SoldrError::Other(format!("install-zccache: failed to serialize sidecar: {e}"))
    })?;
    // Trailing newline matches what `serde_json::to_writer_pretty` plus
    // a `println!()` produces elsewhere in the codebase.
    sidecar_bytes.push(b'\n');
    std::fs::write(&sidecar_path, &sidecar_bytes)?;

    // Keep the staging dir alive until everything that reads from it
    // has finished; Drop unlinks the temp tree.
    drop(staging_holder);

    Ok(InstallReport {
        install_dir: pinned_dir.display().to_string(),
        source_kind: sidecar.source_kind,
        source_value: sidecar.source_value,
        version,
        binaries,
        installed_at,
        soldr_version: sidecar.soldr_version,
    })
}

fn missing_binary_error_message(
    source: &InstallSource,
    dir_displayed: String,
    missing: &str,
    binary_ext: &str,
) -> String {
    let _ = binary_ext;
    match source {
        InstallSource::System => format!(
            "install-zccache: `{missing}` not found on PATH; install zccache or pick a different source"
        ),
        InstallSource::Path(p) => format!(
            "install-zccache: expected directory or .zip/.tar.gz/.tar.zst archive containing zccache, zccache-daemon, zccache-fp — `{missing}` was not found under {} (source: {})",
            dir_displayed,
            p.display()
        ),
        InstallSource::Url(url) => format!(
            "install-zccache: expected directory or .zip/.tar.gz/.tar.zst archive containing zccache, zccache-daemon, zccache-fp — `{missing}` was not found in downloaded archive (source: {url})"
        ),
    }
}

fn p_str(p: &Path) -> String {
    p.display().to_string()
}

fn strip_platform_ext(file_name: &str) -> String {
    // Cheap, deterministic: drop a trailing `.exe` only.
    if let Some(stripped) = file_name.strip_suffix(".exe") {
        return stripped.to_string();
    }
    file_name.to_string()
}

/// Search `PATH` for the three required zccache binaries and copy them
/// (with sidecars) into `staging_dir`. Returns the staging dir on
/// success.
fn stage_from_system_path(
    staging_dir: &Path,
    target: &TargetTriple,
) -> Result<PathBuf, SoldrError> {
    let names = desired_binary_names(&ZCCACHE_PINNED_BINARY_NAMES, target);
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    stage_from_system_path_with_dirs(staging_dir, &names, &path_dirs)
}

pub(crate) fn stage_from_system_path_with_dirs(
    staging_dir: &Path,
    names: &[String],
    path_dirs: &[PathBuf],
) -> Result<PathBuf, SoldrError> {
    let mut missing: Vec<String> = Vec::new();
    let mut found: Vec<(String, PathBuf)> = Vec::with_capacity(names.len());
    for name in names {
        match find_in_dirs(path_dirs, name) {
            Some(p) => found.push((name.clone(), p)),
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(SoldrError::Other(format!(
            "install-zccache: source=system but the following binaries were not found on PATH: {}",
            missing.join(", ")
        )));
    }
    std::fs::create_dir_all(staging_dir)?;
    for (name, src) in &found {
        let dst = staging_dir.join(name);
        copy_if_changed(src, &dst)?;
        copy_debug_info_sidecars(src, &dst)?;
    }
    Ok(staging_dir.to_path_buf())
}

/// Pull every desired binary (and adjacent debug-info sidecars) out of
/// `extracted_root` (the messy result of `extract_archive_file`) into a
/// single flat `unified_root` directory. The original release tarball
/// layout often nests under a `zccache-vX.Y.Z/` directory and may
/// contain README / LICENSE files we don't care about; this consolidation
/// step normalizes everything before the install-into-pinned-dir copy.
fn consolidate_extracted_binaries(
    extracted_root: &Path,
    unified_root: &Path,
    names: &[String],
) -> Result<(), SoldrError> {
    std::fs::create_dir_all(unified_root)?;
    let mut found: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    walk_collect_named_files(extracted_root, names, &mut found)?;
    for name in names {
        let Some(src) = found.get(name) else {
            return Err(SoldrError::Other(format!(
                "install-zccache: archive missing required binary `{name}`"
            )));
        };
        let dst = unified_root.join(name);
        copy_if_changed(src, &dst)?;
        copy_debug_info_sidecars(src, &dst)?;
    }
    Ok(())
}

fn walk_collect_named_files(
    root: &Path,
    names: &[String],
    out: &mut std::collections::BTreeMap<String, PathBuf>,
) -> Result<(), SoldrError> {
    // Simple recursive walk; archives are small enough that depth-first
    // without parallelism is fine.
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // First match wins so a shallower copy beats a deeper duplicate.
            for name in names {
                if file_name == name && !out.contains_key(name) {
                    out.insert(name.clone(), path.clone());
                }
            }
        }
    }
    Ok(())
}

/// Dispatch the right archive extractor for the given file path.
/// Rejects unknown extensions with the canonical error message.
pub(crate) fn extract_archive_file(archive: &Path, dest: &Path) -> Result<(), SoldrError> {
    let lower_name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    std::fs::create_dir_all(dest)?;
    if lower_name.ends_with(".zip") {
        extract_zip_to_dir(archive, dest)
    } else if lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tgz") {
        extract_tar_gz_to_dir(archive, dest)
    } else if lower_name.ends_with(".tar.zst") {
        extract_tar_zst_to_dir(archive, dest)
    } else {
        Err(SoldrError::Other(format!(
            "install-zccache: expected directory or .zip/.tar.gz/.tar.zst archive containing zccache, zccache-daemon, zccache-fp — got {} (unsupported extension)",
            archive.display()
        )))
    }
}

fn extract_zip_to_dir(archive: &Path, dest: &Path) -> Result<(), SoldrError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| SoldrError::Archive(e.to_string()))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| SoldrError::Archive(e.to_string()))?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out_path = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

fn extract_tar_gz_to_dir(archive: &Path, dest: &Path) -> Result<(), SoldrError> {
    let file = std::fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(dest)
        .map_err(|e| SoldrError::Archive(e.to_string()))?;
    Ok(())
}

fn extract_tar_zst_to_dir(archive: &Path, dest: &Path) -> Result<(), SoldrError> {
    let file = std::fs::File::open(archive)?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|e| SoldrError::Archive(format!("zstd init failed: {e}")))?;
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)
        .map_err(|e| SoldrError::Archive(e.to_string()))?;
    Ok(())
}

async fn download_url_to(url: &str, dest: &Path) -> Result<(), SoldrError> {
    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(format!("install-zccache: GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "install-zccache: download failed: HTTP {} ({url})",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(format!("install-zccache: read body: {e}")))?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

fn probe_zccache_version(binary: &Path) -> String {
    let mut command = std::process::Command::new(binary);
    command.arg("--version");
    suppress_windows_console_window(&mut command);
    let Ok(output) = command.output() else {
        return "unknown".to_string();
    };
    if !output.status.success() {
        return "unknown".to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_zccache_version(&stdout).unwrap_or_else(|| "unknown".to_string())
}

fn parse_zccache_version(text: &str) -> Option<String> {
    // Accept both `1.8.1` and `zccache 1.8.1` (clap's default --version
    // output). Take the last whitespace-separated token if it looks
    // like a semver-ish triplet.
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let last = trimmed.split_whitespace().last().unwrap_or("");
        if last.split('.').count() >= 2
            && last
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return Some(last.to_string());
        }
        if trimmed
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Remove the pinned install directory. Returns `Ok(true)` if a
/// directory was actually deleted, `Ok(false)` when the pinned dir
/// didn't exist (so callers can report "no-op"). Idempotent.
pub fn remove_pinned_zccache(paths: &SoldrPaths) -> Result<bool, SoldrError> {
    let pinned_dir = pinned_zccache_dir(paths);
    if !pinned_dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&pinned_dir).map_err(|e| {
        SoldrError::Other(format!(
            "install-zccache: failed to remove {}: {e}",
            pinned_dir.display()
        ))
    })?;
    Ok(true)
}

/// True when the pinned sidecar's recorded version differs from the
/// managed default. Used by `--status` to print a "consider --remove"
/// hint and by doctor to flag drift. An `unknown` version never counts
/// as drift (we can't compare what we don't know).
pub fn pinned_version_drift_from_managed(sidecar: &PinnedSidecar) -> bool {
    sidecar.version != "unknown" && sidecar.version != MANAGED_ZCCACHE_VERSION
}

/// Compact ISO-8601 / RFC3339 timestamp (UTC, second precision). Rolls
/// its own formatter so we don't have to take a chrono / time dep just
/// for one timestamp.
pub(crate) fn format_iso8601_utc(now: std::time::SystemTime) -> String {
    let secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = unix_seconds_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn unix_seconds_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    // Civil-from-days algorithm by Howard Hinnant (public domain).
    // Splits a unix timestamp into UTC Y/M/D/h/m/s with no leap-second
    // handling — good enough for filesystem provenance.
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u32;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let z = days + 719_468; // shift epoch to 0000-03-01
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_system_source() {
        let s = InstallSource::parse("system").unwrap();
        assert!(matches!(s, InstallSource::System));
        let s = InstallSource::parse(" SYSTEM ").unwrap();
        assert!(matches!(s, InstallSource::System));
    }

    #[test]
    fn parse_url_source() {
        let s = InstallSource::parse("https://example.com/pkg.tar.gz").unwrap();
        assert!(matches!(s, InstallSource::Url(_)));
        let s = InstallSource::parse("HTTP://example.com/pkg.zip").unwrap();
        assert!(matches!(s, InstallSource::Url(_)));
    }

    #[test]
    fn parse_path_source() {
        let s = InstallSource::parse("/opt/zccache/bin").unwrap();
        assert!(matches!(s, InstallSource::Path(_)));
        let s = InstallSource::parse("./relative").unwrap();
        assert!(matches!(s, InstallSource::Path(_)));
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(InstallSource::parse("").is_err());
        assert!(InstallSource::parse("   ").is_err());
    }

    #[test]
    fn parse_zccache_version_accepts_clap_default() {
        assert_eq!(parse_zccache_version("zccache 1.8.1"), Some("1.8.1".into()));
        assert_eq!(parse_zccache_version("1.8.1"), Some("1.8.1".into()));
        assert_eq!(
            parse_zccache_version("zccache 1.8.1\nbuild: abc"),
            Some("1.8.1".into())
        );
    }

    #[test]
    fn parse_zccache_version_rejects_garbage() {
        assert_eq!(parse_zccache_version(""), None);
        assert_eq!(parse_zccache_version("hello world"), None);
    }

    #[test]
    fn strip_extension_only_drops_exe() {
        assert_eq!(strip_platform_ext("zccache"), "zccache");
        assert_eq!(strip_platform_ext("zccache.exe"), "zccache");
        assert_eq!(strip_platform_ext("zccache-daemon"), "zccache-daemon");
    }

    #[test]
    fn drift_helper_treats_unknown_as_no_drift() {
        let mut sidecar = sample_sidecar("unknown");
        assert!(!pinned_version_drift_from_managed(&sidecar));
        sidecar.version = MANAGED_ZCCACHE_VERSION.to_string();
        assert!(!pinned_version_drift_from_managed(&sidecar));
        sidecar.version = "0.0.1".to_string();
        assert!(pinned_version_drift_from_managed(&sidecar));
    }

    fn sample_sidecar(version: &str) -> PinnedSidecar {
        PinnedSidecar {
            schema_version: PINNED_ZCCACHE_SIDECAR_SCHEMA_VERSION,
            source_kind: "system".into(),
            source_value: "system".into(),
            version: version.into(),
            binaries: std::collections::BTreeMap::new(),
            installed_at: "2026-05-21T00:00:00Z".into(),
            soldr_version: "0.0.0".into(),
        }
    }

    #[test]
    fn iso8601_formatter_matches_known_timestamps() {
        // 1970-01-01T00:00:00Z
        let s = format_iso8601_utc(std::time::UNIX_EPOCH);
        assert_eq!(s, "1970-01-01T00:00:00Z");
        // A few well-known dates.
        let later = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let s = format_iso8601_utc(later);
        // 1_700_000_000 == 2023-11-14T22:13:20Z
        assert_eq!(s, "2023-11-14T22:13:20Z");
    }
}
