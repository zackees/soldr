//! `soldr archive` — bundle a built `soldr` binary plus its managed-
//! tool sidecars (`zccache`, `zccache-daemon`, `zccache-fp`, `crgx`,
//! `cargo-chef`) into a single deployable `.tar.zst` archive.
//!
//! Issue #858 (sub-issue of #853): the release workflow currently
//! hand-rolls this packaging in a long YAML shell step that fetches the
//! matching zccache release, source-builds crgx + cargo-chef, stages
//! everything under `dist/package/`, and pipes a `tar` into `zstd -19`.
//! That logic is brittle (cross-platform `find`/`sed`/`tar` flag drift)
//! and hard to test outside CI. This subcommand ports the staging +
//! compression half into Rust so any developer can locally produce a
//! deployable bundle with:
//!
//! ```text
//! soldr cargo build --release --target x86_64-pc-windows-msvc
//! soldr archive --target x86_64-pc-windows-msvc \
//!     --output dist/soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst
//! ```
//!
//! ## What lands inside
//!
//! Output is a flat tar.zst with the following entries at the archive
//! root (every entry is intentionally one level deep — matches the
//! release-workflow contract `setup-soldr` already consumes):
//!
//! - `soldr` / `soldr.exe`                  — the staged binary.
//! - `zccache`, `zccache-daemon`, `zccache-fp` (+ `.exe` on Windows) —
//!   the managed zccache trio for the target.
//! - `crgx` / `crgx.exe`                    — bundled crgx (optional).
//! - `cargo-chef` / `cargo-chef.exe`        — bundled cargo-chef (optional).
//! - `manifest.json`                        — minimal descriptor (soldr
//!   version + target + zccache version + entry list).
//!
//! ## Sidecar resolution
//!
//! By default the command pulls each sidecar from
//! `<SoldrPaths::bin>/zccache-<MANAGED_ZCCACHE_VERSION>/` (the same dir
//! `fetch_zccache_with_paths` populates). When the sidecar is missing
//! from the local cache the command errors with a directive naming the
//! exact warm-up command, instead of attempting a network fetch — this
//! keeps `soldr archive` hermetic and testable. `crgx` and `cargo-chef`
//! are looked up via the same managed-cache pattern; both are optional
//! (missing entry just isn't included).
//!
//! Tests inject fake binaries via the `ArchiveSources` struct so the
//! whole pipeline can be exercised without touching `~/.soldr`.
//!
//! ## Compression
//!
//! Uses the same `zstd::stream::write::Encoder` as
//! `cache_lib::save::save` but at level **19** (the release-workflow
//! default — substantially smaller archives at the cost of CPU during
//! `soldr archive` itself). zstd's built-in multithreading is enabled
//! so the level-19 cost lands on every available core.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};
use crate::fetch::MANAGED_ZCCACHE_VERSION;

/// zstd compression level for `soldr archive` output. Matches the
/// release-workflow setting documented in `.github/workflows/release-auto.yml`
/// ("Package combined archive (tar.zst level 19)").
pub const ARCHIVE_ZSTD_LEVEL: i32 = 19;

/// Schema version emitted in the archive's `manifest.json`. Bump when
/// fields are added/removed.
pub const ARCHIVE_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// One staged file to include in the archive.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    /// Name under the archive root. Must not contain path separators.
    pub archive_name: String,
    /// Source path on disk.
    pub source: PathBuf,
    /// Optional sha256 hash recorded in `manifest.json`. Filled by
    /// `build_archive`; callers leave this `None`.
    pub sha256: Option<String>,
}

/// Inputs to `build_archive` — fully resolved file paths for every
/// component soldr wants to include. The CLI front door (`run`)
/// produces this struct from a target triple + soldr paths; tests
/// build it directly with synthetic files.
#[derive(Debug, Clone)]
pub struct ArchiveSources {
    pub soldr_binary: PathBuf,
    /// Required sidecars: missing → error.
    pub required: Vec<ArchiveEntry>,
    /// Optional sidecars: missing → silently skipped.
    pub optional: Vec<ArchiveEntry>,
    pub target_triple: String,
    pub soldr_version: String,
}

/// Summary returned by `build_archive`. Printed by `run` so users see
/// the archive's size + entry count without re-stating it.
#[derive(Debug, Clone)]
pub struct ArchiveReport {
    pub output: PathBuf,
    pub bytes: u64,
    pub entries: Vec<ArchiveEntry>,
}

/// Top-level CLI dispatch. Wired from `Commands::Archive` in `main.rs`.
pub fn run(target: Option<String>, output: PathBuf) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let target = match target {
        Some(t) => TargetTriple::from_triple(&t)?,
        None => TargetTriple::detect()?,
    };
    let sources = resolve_sources(&paths, &target)?;
    let report = build_archive(&sources, &output)?;
    println!(
        "soldr archive: wrote {} ({} bytes, {} entries)",
        report.output.display(),
        report.bytes,
        report.entries.len(),
    );
    Ok(())
}

/// Resolve every input path for the host's archive-staging step against
/// `paths` (the live `~/.soldr/` layout) and a target triple.
pub fn resolve_sources(
    paths: &SoldrPaths,
    target: &TargetTriple,
) -> Result<ArchiveSources, SoldrError> {
    let triple = target.triple();
    let exe_suffix = target.binary_ext();
    let soldr_name = format!("soldr{exe_suffix}");

    let cwd = std::env::current_dir().map_err(SoldrError::Io)?;
    let soldr_binary = cwd
        .join("target")
        .join(&triple)
        .join("release")
        .join(&soldr_name);
    if !soldr_binary.is_file() {
        return Err(SoldrError::Other(format!(
            "soldr binary not found at {} — run `soldr cargo build --release --target {}` first",
            soldr_binary.display(),
            triple,
        )));
    }

    // Probe two candidate dirs: the managed cache populated by
    // `fetch_zccache_with_paths` (versioned by `MANAGED_ZCCACHE_VERSION`)
    // and the pinned-install dir written by `soldr install-zccache`.
    // The pinned dir wins when present — that's the workflow setup-soldr
    // uses to lock the trio at a specific version, and on hosts that
    // installed via pin, the managed dir may only carry the cli binary.
    let pinned_dir = paths.bin.join("zccache-pinned");
    let managed_dir = paths.bin.join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"));
    let stems = ["zccache", "zccache-daemon", "zccache-fp"];
    let zccache_dir = if stems
        .iter()
        .all(|s| pinned_dir.join(format!("{s}{exe_suffix}")).is_file())
    {
        pinned_dir
    } else {
        managed_dir.clone()
    };
    let mut required = Vec::new();
    for stem in stems {
        let name = format!("{stem}{exe_suffix}");
        let source = zccache_dir.join(&name);
        if !source.is_file() {
            return Err(SoldrError::Other(format!(
                "managed zccache sidecar missing at {} — run `soldr cargo build` once to populate {}/zccache-{MANAGED_ZCCACHE_VERSION}/, or pre-install with `soldr install-zccache`",
                source.display(),
                paths.bin.display(),
            )));
        }
        required.push(ArchiveEntry {
            archive_name: name,
            source,
            sha256: None,
        });
    }

    // Optional sidecars: crgx + cargo-chef are looked up under the
    // same managed-cache layout (`<bin>/<crate>-<version>/<binary>`).
    // The exact version dirs are discovered by globbing the cache root
    // since soldr-cli doesn't expose a "find newest cached binary"
    // helper. Missing → just omit, matches the "optional" contract.
    let mut optional = Vec::new();
    for stem in ["crgx", "cargo-chef"] {
        let binary_name = format!("{stem}{exe_suffix}");
        if let Some(source) = find_latest_cached_tool(&paths.bin, stem, &binary_name) {
            optional.push(ArchiveEntry {
                archive_name: binary_name,
                source,
                sha256: None,
            });
        }
    }

    Ok(ArchiveSources {
        soldr_binary,
        required,
        optional,
        target_triple: triple,
        soldr_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Walk `<bin>/<stem>-*` directories and return the path to the
/// matching binary in the alphabetically-largest dir name. `cargo-chef`
/// uses `<bin>/cargo-chef-<version>/cargo-chef[.exe]`; same for crgx.
/// Returns `None` when no matching cached version exists.
fn find_latest_cached_tool(bin_root: &Path, stem: &str, binary_name: &str) -> Option<PathBuf> {
    let prefix = format!("{stem}-");
    let read = std::fs::read_dir(bin_root).ok()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            let candidate = entry.path().join(binary_name);
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }
    candidates.sort();
    candidates.pop()
}

/// Hash + copy every input into a `tar.zst` at `output`. The tar
/// payload is intentionally flat (every entry at archive root) so
/// consumers can `tar -xf` into a release dir without nested
/// stripping. Compression: `ARCHIVE_ZSTD_LEVEL` with built-in
/// multithreading.
pub fn build_archive(sources: &ArchiveSources, output: &Path) -> Result<ArchiveReport, SoldrError> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(SoldrError::Io)?;
        }
    }

    // Build the entry list. soldr binary always goes first so consumers
    // streaming the archive see the marquee bit immediately.
    let exe_suffix = if sources.target_triple.contains("-pc-windows-") {
        ".exe"
    } else {
        ""
    };
    let soldr_entry_name = format!("soldr{exe_suffix}");
    let mut entries: Vec<ArchiveEntry> = Vec::new();
    entries.push(ArchiveEntry {
        archive_name: soldr_entry_name,
        source: sources.soldr_binary.clone(),
        sha256: None,
    });
    entries.extend(sources.required.iter().cloned());
    entries.extend(sources.optional.iter().cloned());

    // Hash everything before opening the tar writer so the manifest
    // (which lands inside the tar) can carry the per-entry sha256.
    for entry in &mut entries {
        let hash = sha256_of_file(&entry.source)?;
        entry.sha256 = Some(hash);
    }

    let manifest = render_manifest(sources, &entries);

    let out_file = File::create(output)
        .map_err(|e| SoldrError::Other(format!("create {}: {e}", output.display())))?;
    let out_buf = BufWriter::with_capacity(4 * 1024 * 1024, out_file);
    let mut encoder = zstd::stream::write::Encoder::new(out_buf, ARCHIVE_ZSTD_LEVEL)
        .map_err(|e| SoldrError::Other(format!("zstd init: {e}")))?;
    // Use all cores; this is a release-packaging command, not a
    // wrapper-fast-path operation.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    encoder
        .multithread(workers)
        .map_err(|e| SoldrError::Other(format!("zstd multithread: {e}")))?;

    {
        let mut builder = tar::Builder::new(&mut encoder);
        builder.mode(tar::HeaderMode::Deterministic);

        // 1) every binary entry.
        for entry in &entries {
            append_file(&mut builder, &entry.source, &entry.archive_name)?;
        }

        // 2) manifest.json.
        let manifest_bytes = manifest.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(manifest_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, "manifest.json", manifest_bytes)
            .map_err(|e| SoldrError::Other(format!("append manifest.json: {e}")))?;

        builder
            .finish()
            .map_err(|e| SoldrError::Other(format!("tar finish: {e}")))?;
    }

    let writer = encoder
        .finish()
        .map_err(|e| SoldrError::Other(format!("zstd finish: {e}")))?;
    writer
        .into_inner()
        .map_err(|e| SoldrError::Other(format!("flush archive: {}", e.into_error())))?;

    let bytes = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    Ok(ArchiveReport {
        output: output.to_path_buf(),
        bytes,
        entries,
    })
}

fn append_file<W: Write>(
    builder: &mut tar::Builder<W>,
    source: &Path,
    archive_name: &str,
) -> Result<(), SoldrError> {
    let meta = std::fs::metadata(source)
        .map_err(|e| SoldrError::Other(format!("stat {}: {e}", source.display())))?;
    let mut file = File::open(source)
        .map_err(|e| SoldrError::Other(format!("open {}: {e}", source.display())))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(meta.len());
    // Mark every binary executable: even on Windows the mode is
    // ignored at extract time, but POSIX consumers (the Linux/macOS
    // tar fallbacks in setup-soldr) need 0o755 to run them straight
    // out of the archive.
    header.set_mode(0o755);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, archive_name, &mut file)
        .map_err(|e| SoldrError::Other(format!("append {archive_name}: {e}")))?;
    Ok(())
}

fn render_manifest(sources: &ArchiveSources, entries: &[ArchiveEntry]) -> String {
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!(
        "  \"schema_version\": {ARCHIVE_MANIFEST_SCHEMA_VERSION},\n",
    ));
    json.push_str(&format!(
        "  \"soldr_version\": \"{}\",\n",
        json_escape(&sources.soldr_version),
    ));
    json.push_str(&format!(
        "  \"target\": \"{}\",\n",
        json_escape(&sources.target_triple),
    ));
    json.push_str(&format!(
        "  \"zccache_version\": \"{}\",\n",
        json_escape(MANAGED_ZCCACHE_VERSION),
    ));
    json.push_str("  \"entries\": [\n");
    for (i, entry) in entries.iter().enumerate() {
        let comma = if i + 1 == entries.len() { "" } else { "," };
        let sha = entry.sha256.as_deref().unwrap_or("");
        json.push_str(&format!(
            "    {{ \"name\": \"{}\", \"sha256\": \"{}\" }}{comma}\n",
            json_escape(&entry.archive_name),
            json_escape(sha),
        ));
    }
    json.push_str("  ]\n");
    json.push_str("}\n");
    json
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn sha256_of_file(path: &Path) -> Result<String, SoldrError> {
    use sha2::{Digest, Sha256};
    let mut file =
        File::open(path).map_err(|e| SoldrError::Other(format!("open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|e| SoldrError::Other(format!("hash {}: {e}", path.display())))?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest.iter() {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    fn write_fake(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn synthesize_sources(stage: &Path, target: &str) -> ArchiveSources {
        let soldr = write_fake(stage, "soldr", b"fake-soldr-bin");
        let zcc = write_fake(stage, "zccache", b"fake-zccache");
        let zccd = write_fake(stage, "zccache-daemon", b"fake-zccache-daemon");
        let zccfp = write_fake(stage, "zccache-fp", b"fake-zccache-fp");
        let crgx = write_fake(stage, "crgx", b"fake-crgx");
        let chef = write_fake(stage, "cargo-chef", b"fake-cargo-chef");
        ArchiveSources {
            soldr_binary: soldr,
            required: vec![
                ArchiveEntry {
                    archive_name: "zccache".into(),
                    source: zcc,
                    sha256: None,
                },
                ArchiveEntry {
                    archive_name: "zccache-daemon".into(),
                    source: zccd,
                    sha256: None,
                },
                ArchiveEntry {
                    archive_name: "zccache-fp".into(),
                    source: zccfp,
                    sha256: None,
                },
            ],
            optional: vec![
                ArchiveEntry {
                    archive_name: "crgx".into(),
                    source: crgx,
                    sha256: None,
                },
                ArchiveEntry {
                    archive_name: "cargo-chef".into(),
                    source: chef,
                    sha256: None,
                },
            ],
            target_triple: target.to_string(),
            soldr_version: "0.0.0-test".into(),
        }
    }

    fn list_archive_entries(path: &Path) -> Vec<String> {
        let file = File::open(path).expect("open archive");
        let dec = zstd::stream::read::Decoder::new(file).expect("zstd decoder");
        let mut archive = tar::Archive::new(dec);
        let mut names = Vec::new();
        for entry in archive.entries().expect("entries") {
            let entry = entry.expect("entry");
            let path = entry.path().expect("path").into_owned();
            names.push(path.to_string_lossy().into_owned());
        }
        names.sort();
        names
    }

    fn read_archive_file(path: &Path, name: &str) -> Option<Vec<u8>> {
        let file = File::open(path).expect("open archive");
        let dec = zstd::stream::read::Decoder::new(file).expect("zstd decoder");
        let mut archive = tar::Archive::new(dec);
        for entry in archive.entries().expect("entries") {
            let mut entry = entry.expect("entry");
            let entry_path = entry.path().expect("path").into_owned();
            if entry_path.to_string_lossy() == name {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf).expect("read entry");
                return Some(buf);
            }
        }
        None
    }

    crate::timed_test!(archive_writes_to_output_path, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let sources = synthesize_sources(&stage, "x86_64-unknown-linux-gnu");
        let out = tmp.path().join("out.tar.zst");
        let report = build_archive(&sources, &out).expect("build_archive");
        assert!(out.is_file(), "archive must exist at {}", out.display());
        let meta = std::fs::metadata(&out).expect("metadata");
        assert!(meta.len() > 0, "archive must be non-empty");
        assert_eq!(report.bytes, meta.len());
        assert!(
            report
                .entries
                .iter()
                .all(|e| e.sha256.as_deref().is_some_and(|s| s.len() == 64)),
            "every entry must carry a 64-char sha256"
        );
    });

    crate::timed_test!(archive_errors_when_binary_missing, {
        let tmp = tempfile::tempdir().expect("tempdir");
        // SoldrPaths::with_root is the test-construction path used by
        // integration tests — it produces a synthetic-root layout
        // that doesn't touch the real ~/.soldr.
        let root = tmp.path().join("soldr-home");
        std::fs::create_dir_all(&root).unwrap();
        let paths = SoldrPaths::with_root(root);
        std::fs::create_dir_all(&paths.bin).unwrap();
        // chdir into a fresh empty dir so the `target/<triple>/release/soldr`
        // path resolve_sources computes is guaranteed to not exist.
        let workdir = tmp.path().join("workdir");
        std::fs::create_dir_all(&workdir).unwrap();
        let prev_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workdir).expect("chdir");
        let target = TargetTriple::from_triple("x86_64-unknown-linux-gnu").expect("triple");
        let result = resolve_sources(&paths, &target);
        std::env::set_current_dir(prev_cwd).expect("restore cwd");
        let err = result.expect_err("must error when binary missing");
        let msg = format!("{err}");
        assert!(
            msg.contains("soldr cargo build --release --target"),
            "error must name the directive command, got: {msg}",
        );
    });

    crate::timed_test!(archive_includes_expected_sidecars, {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let sources = synthesize_sources(&stage, "x86_64-unknown-linux-gnu");
        let out = tmp.path().join("out.tar.zst");
        build_archive(&sources, &out).expect("build_archive");
        let entries = list_archive_entries(&out);
        for expected in [
            "soldr",
            "zccache",
            "zccache-daemon",
            "zccache-fp",
            "crgx",
            "cargo-chef",
            "manifest.json",
        ] {
            assert!(
                entries.iter().any(|e| e == expected),
                "archive must contain `{expected}`, got: {entries:?}",
            );
        }
        let manifest = read_archive_file(&out, "manifest.json").expect("manifest in archive");
        let manifest = String::from_utf8(manifest).expect("manifest is utf8");
        assert!(
            manifest.contains("\"schema_version\": 1"),
            "manifest: {manifest}"
        );
        assert!(
            manifest.contains("x86_64-unknown-linux-gnu"),
            "manifest: {manifest}"
        );
        assert!(
            manifest.contains("\"zccache_version\""),
            "manifest: {manifest}"
        );
    });

    crate::timed_test!(archive_windows_target_uses_exe_suffix, {
        // When the target triple is windows-msvc the soldr entry name
        // in the archive gets `.exe`. Sidecar entries already include
        // their own suffix in the ArchiveEntry::archive_name field; the
        // synthesis helper used here mirrors the non-suffix case for
        // simplicity, so we only assert the soldr entry was renamed.
        let tmp = tempfile::tempdir().expect("tempdir");
        let stage = tmp.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let mut sources = synthesize_sources(&stage, "x86_64-pc-windows-msvc");
        // Simulate Windows binary layout: rename sidecars + soldr to
        // include the `.exe` suffix so the staging is realistic.
        let new_soldr = stage.join("soldr.exe");
        std::fs::rename(&sources.soldr_binary, &new_soldr).unwrap();
        sources.soldr_binary = new_soldr;
        for entry in sources
            .required
            .iter_mut()
            .chain(sources.optional.iter_mut())
        {
            let new_name = format!("{}.exe", entry.archive_name);
            let new_source = stage.join(&new_name);
            std::fs::rename(&entry.source, &new_source).unwrap();
            entry.source = new_source;
            entry.archive_name = new_name;
        }
        let out = tmp.path().join("out.tar.zst");
        build_archive(&sources, &out).expect("build_archive");
        let entries = list_archive_entries(&out);
        assert!(
            entries.iter().any(|e| e == "soldr.exe"),
            "windows archive must contain soldr.exe, got: {entries:?}",
        );
    });
}
