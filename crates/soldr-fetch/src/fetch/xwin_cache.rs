//! soldr#1012 PR 5 — xwin-cache materialization for the blessed
//! `*-pc-windows-msvc` cross-compile path.
//!
//! Loads the pre-compressed MSVC SDK bundle from the soldr-toolchain
//! catalogue (`https://zackees.github.io/soldr-toolchain/...`),
//! verifies sha256, extracts under `~/.soldr/sdk/<triple>/xwin/`, and
//! returns the local path. Cached after first call — subsequent
//! `soldr build --target *-pc-windows-msvc` invocations are
//! filesystem-only after the cache is warm.
//!
//! ## Architecture
//!
//! The catalogue ships one row per `(version, platform)`:
//!
//! * `xwin-cache/<date>/windows-x86_64-msvc/xwin-cache.tar.zst`
//! * `xwin-cache/<date>/windows-aarch64-msvc/xwin-cache.tar.zst`
//!   (soldr-toolchain PR #30 / soldr#1012 PR 3)
//!
//! Soldr's `Commands::Build` arm calls [`ensure_xwin_cache`] before
//! invoking cargo for those targets; the bundle is materialized, and
//! the blessed path injects matching include/linker flags so plain
//! cargo can build against the managed SDK. `XWIN_CACHE_DIR` is still
//! exported for explicit cargo-xwin fallback consumers.
//!
//! ## Pinned versions
//!
//! For now we ship hardcoded asset URLs + sha256s per target. This
//! mirrors the `apple_sdk.rs` pattern. A future refactor (#1010
//! Phase 4) routes everything through `catalogue.v1.json` resolution
//! at runtime, but the immediate goal of this PR is "win-arm64
//! works"; hardcoded sha256s are the minimum-blast-radius first cut.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

use super::manifest_lookup;

/// Pinned xwin-cache release date currently in the catalogue.
/// Bump when a refreshed bundle ships from soldr-toolchain forge
/// ingest.
pub const MANAGED_XWIN_CACHE_VERSION: &str = "2026-06-22";

const XWIN_CACHE_X86_64_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-x86_64-msvc/xwin-cache.tar.zst";
const XWIN_CACHE_X86_64_SHA256: &str =
    "33c04d8026d99dab4d66f39ddbd93d75f64c68063d4ba58e5450626524bf348d";

const XWIN_CACHE_AARCH64_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-aarch64-msvc/xwin-cache.tar.zst";
const XWIN_CACHE_AARCH64_SHA256: &str =
    "cb7fa0e68ce173a54f0dbc116d3e8f04c7013953529ada8284b0ac149139b9da";

/// Env var consumed by cargo-xwin to short-circuit its own download.
/// The blessed path sets this so anything else in the workflow that
/// invokes cargo-xwin transparently uses our materialized cache.
pub const XWIN_CACHE_DIR_ENV_VAR: &str = "XWIN_CACHE_DIR";

/// Resolve the local xwin-cache directory for `target`. Materializes
/// the bundle on first call; subsequent calls return the cached path
/// immediately.
pub async fn ensure_xwin_cache(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let (url, expected_sha256) = match target_triple {
        "x86_64-pc-windows-msvc" => (XWIN_CACHE_X86_64_URL, XWIN_CACHE_X86_64_SHA256),
        "aarch64-pc-windows-msvc" => (XWIN_CACHE_AARCH64_URL, XWIN_CACHE_AARCH64_SHA256),
        _ => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "ensure_xwin_cache: no managed xwin bundle for triple {target_triple}; \
                 supported: x86_64-pc-windows-msvc, aarch64-pc-windows-msvc"
            )));
        }
    };

    paths.ensure_dirs()?;
    let install_dir = paths
        .root
        .join("sdk")
        .join(target_triple)
        .join("xwin")
        .join(MANAGED_XWIN_CACHE_VERSION);
    let stamp = install_dir.join(".complete");
    if stamp.is_file() {
        if let Some(cache_dir) = resolve_xwin_cache_dir(&install_dir) {
            let aliases = ensure_xwin_case_aliases(&cache_dir)?;
            if aliases > 0 {
                eprintln!("soldr: added {aliases} xwin-cache case aliases");
            }
            return Ok(cache_dir);
        }
    }

    let install_parent = install_dir
        .parent()
        .ok_or_else(|| SoldrError::Archive("xwin-cache install path has no parent".into()))?;
    let _install_lock = super::syslib_common::acquire_install_lock(
        install_parent,
        &format!("xwin-cache-{MANAGED_XWIN_CACHE_VERSION}"),
    )?;

    if stamp.is_file() {
        if let Some(cache_dir) = resolve_xwin_cache_dir(&install_dir) {
            // Repair pass for caches extracted by older soldr versions (or
            // restored from a CI cache) that predate the case-alias fix —
            // idempotent and cheap once the aliases exist (cross-run 28574600982 fix).
            let aliases = ensure_xwin_case_aliases(&cache_dir)?;
            if aliases > 0 {
                eprintln!("soldr: added {aliases} xwin-cache case aliases");
            }
            return Ok(cache_dir);
        }
        // A stamp without a usable tree is incomplete or corrupt. Clear only
        // the marker so the verified download below repairs the installation.
        std::fs::remove_file(&stamp)?;
    }

    let entry = manifest_lookup::get_or_fetch()
        .await
        .entries
        .iter()
        .find(|entry| entry.matches_legacy_url(url))
        .cloned()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "xwin-cache {target_triple} is absent from the soldr-toolchain catalogue"
            ))
        })?;
    if entry.sha256 != expected_sha256 {
        return Err(SoldrError::Other(format!(
            "xwin-cache catalogue pin changed for {target_triple}: expected {expected_sha256}, got {}",
            entry.sha256
        )));
    }
    let resolved_url = manifest_lookup::resolved_download_label(&entry);
    eprintln!(
        "soldr: fetching xwin-cache v{MANAGED_XWIN_CACHE_VERSION} for {target_triple} from {resolved_url}..."
    );

    let downloaded = manifest_lookup::materialize_catalogue_entry(paths, &entry).await?;

    let digest = downloaded.sha256();
    if digest != expected_sha256 {
        return Err(SoldrError::Other(format!(
            "xwin-cache sha256 mismatch for {target_triple}: \
             expected {expected_sha256}, got {digest} \
             (catalogue blob may have been replaced — refusing to extract)"
        )));
    }
    eprintln!(
        "soldr: trust: verified xwin-cache v{MANAGED_XWIN_CACHE_VERSION} \
         for {target_triple} sha256={digest}"
    );

    let staging = tempfile::Builder::new()
        .prefix(".xwin-cache.staging-")
        .tempdir_in(install_parent)?;
    extract_tar_zst_tree(std::fs::File::open(downloaded.path())?, staging.path())?;

    let staged_cache_dir = resolve_xwin_cache_dir(staging.path()).ok_or_else(|| {
        SoldrError::Archive(format!(
            "xwin-cache extract did not produce a crt/sdk root under {} \
             (checked xwin/, package/, and the extraction root)",
            staging.path().display()
        ))
    })?;
    if !staged_cache_dir.is_dir() {
        return Err(SoldrError::Archive(format!(
            "xwin-cache extract did not produce expected directory {}",
            staged_cache_dir.display()
        )));
    }

    // Case-sensitivity repair (cross-run 28574600982 fix): the catalogue bundle ships
    // one file per header with whatever casing the recipe splatted, but
    // Windows SDK headers cross-reference each other with inconsistent
    // casing (`kernelspecs.h` does `#include "DriverSpecs.h"` while the
    // file on disk is `driverspecs.h`). On the case-sensitive Linux CI
    // filesystems that include fails fatally. Materialize the aliases
    // both directions before declaring the cache usable.
    let aliases = ensure_xwin_case_aliases(&staged_cache_dir)?;
    if aliases > 0 {
        eprintln!("soldr: added {aliases} xwin-cache case aliases");
    }

    std::fs::write(staging.path().join(".complete"), MANAGED_XWIN_CACHE_VERSION)?;
    super::archive::promote_staged_tool_dir(staging.path(), &install_dir)?;
    let cache_dir = resolve_xwin_cache_dir(&install_dir)
        .ok_or_else(|| SoldrError::Archive("xwin-cache vanished after atomic promotion".into()))?;
    eprintln!(
        "soldr: extracted xwin-cache to {} (set XWIN_CACHE_DIR there)",
        cache_dir.display()
    );
    Ok(cache_dir)
}

fn resolve_xwin_cache_dir(install_dir: &Path) -> Option<PathBuf> {
    [
        install_dir.join("xwin"),
        install_dir.join("package"),
        install_dir.to_path_buf(),
    ]
    .into_iter()
    .find(|candidate| candidate.join("crt").is_dir() && candidate.join("sdk").is_dir())
}

/// Create the case-variant file aliases that make an xwin splat usable
/// on a case-sensitive filesystem. Returns the number of aliases
/// created (0 on a case-insensitive filesystem or when everything is
/// already materialized — the pass is idempotent).
///
/// Two passes:
///
/// 1. **Lowercase aliases** — for every mixed-case file under
///    `crt/{include,lib}` + `sdk/{include,lib}`, hardlink a lowercase
///    sibling (`Kernel32.Lib` → `kernel32.lib`). Covers lowercase
///    `#include <windows.h>` / `-lkernel32`-style references to
///    mixed-case files.
/// 2. **Include-referenced aliases** (cross-run 28574600982 fix) — scan every file
///    under the include trees for `#include` directives and, for each
///    referenced name that only matches an on-disk file
///    case-INsensitively, hardlink an alias with the referenced casing
///    (`driverspecs.h` → `DriverSpecs.h`, referenced by
///    `kernelspecs.h` which winnt.h pulls into every `windows.h`
///    compile). This is the reverse direction of pass 1 and is what
///    xwin's own `--symlinks` mode does at splat time.
pub fn ensure_xwin_case_aliases(xwin_dir: &Path) -> Result<usize, SoldrError> {
    let roots = [
        xwin_dir.join("crt").join("include"),
        xwin_dir.join("crt").join("lib"),
        xwin_dir.join("sdk").join("include"),
        xwin_dir.join("sdk").join("lib"),
    ];

    let mut created = 0;
    for root in roots {
        if root.is_dir() {
            created += ensure_lowercase_file_aliases(&root)?;
        }
    }
    created += ensure_include_referenced_aliases(xwin_dir)?;
    Ok(created)
}

fn ensure_lowercase_file_aliases(dir: &Path) -> Result<usize, SoldrError> {
    let mut created = 0;
    for entry in std::fs::read_dir(dir)
        .map_err(|e| SoldrError::Other(format!("read xwin cache dir {}: {e}", dir.display())))?
    {
        let entry = entry.map_err(|e| {
            SoldrError::Other(format!(
                "read xwin cache entry under {}: {e}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| {
            SoldrError::Other(format!("stat xwin cache entry {}: {e}", path.display()))
        })?;

        if file_type.is_dir() {
            created += ensure_lowercase_file_aliases(&path)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        let lower = name.to_ascii_lowercase();
        if lower == name {
            continue;
        }

        let alias = path.with_file_name(lower);
        if alias.exists() {
            continue;
        }

        create_xwin_file_alias(&path, &alias)?;
        created += 1;
    }
    Ok(created)
}

/// Pass 2 of [`ensure_xwin_case_aliases`]: alias every include-directive
/// referenced name whose on-disk file only matches case-insensitively.
fn ensure_include_referenced_aliases(xwin_dir: &Path) -> Result<usize, SoldrError> {
    let include_roots = [
        xwin_dir.join("crt").join("include"),
        xwin_dir.join("sdk").join("include"),
    ];

    // (dir, lowercase-name → actual-name) per directory, plus the set
    // of basenames referenced by any `#include` directive anywhere in
    // the splat. Quoted includes resolve relative to the includer's
    // directory and angle includes against every `/imsvc` root, so an
    // alias is created in EVERY directory holding a case-insensitive
    // match — a superset of what resolution needs, and harmless.
    let mut dir_maps: Vec<(PathBuf, HashMap<String, String>)> = Vec::new();
    let mut referenced: HashSet<String> = HashSet::new();
    for root in &include_roots {
        if root.is_dir() {
            collect_names_and_includes(root, &mut dir_maps, &mut referenced)?;
        }
    }

    let mut created = 0;
    for (dir, names) in &dir_maps {
        for reference in &referenced {
            let Some(actual) = names.get(&reference.to_ascii_lowercase()) else {
                continue;
            };
            if actual == reference {
                continue;
            }
            let alias = dir.join(reference);
            // `exists()` is the case-insensitive-filesystem guard: on
            // such filesystems the alias name already resolves to the
            // actual file, so nothing needs materializing.
            if alias.exists() {
                continue;
            }
            create_xwin_file_alias(&dir.join(actual), &alias)?;
            created += 1;
        }
    }
    Ok(created)
}

fn collect_names_and_includes(
    dir: &Path,
    dir_maps: &mut Vec<(PathBuf, HashMap<String, String>)>,
    referenced: &mut HashSet<String>,
) -> Result<(), SoldrError> {
    let mut names: HashMap<String, String> = HashMap::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| SoldrError::Other(format!("read xwin include dir {}: {e}", dir.display())))?
    {
        let entry = entry.map_err(|e| {
            SoldrError::Other(format!(
                "read xwin include entry under {}: {e}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| {
            SoldrError::Other(format!("stat xwin include entry {}: {e}", path.display()))
        })?;

        if file_type.is_dir() {
            collect_names_and_includes(&path, dir_maps, referenced)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue; // non-UTF8 names can't appear in #include directives
        };
        // First name wins on case-insensitive filesystems where two
        // casings can't coexist anyway; on case-sensitive filesystems
        // duplicates only differ by case and either works as a source.
        names.entry(name.to_ascii_lowercase()).or_insert(name);

        // Every file under the include trees is a text header (SDK
        // headers, MSVC STL extensionless headers, .inl bodies). Scan
        // them all rather than maintaining an extension allowlist.
        if let Ok(content) = std::fs::read(&path) {
            scan_include_directives(&content, referenced);
        }
    }
    dir_maps.push((dir.to_path_buf(), names));
    Ok(())
}

/// Collect the basename of every `#include "..."` / `#include <...>`
/// directive in `content` into `referenced`. Tolerant line-oriented
/// parse — a false positive only risks creating an unnecessary alias.
fn scan_include_directives(content: &[u8], referenced: &mut HashSet<String>) {
    for line in content.split(|&b| b == b'\n') {
        let s = trim_ascii_start(line);
        let Some(s) = s.strip_prefix(b"#") else {
            continue;
        };
        let s = trim_ascii_start(s);
        let Some(s) = s.strip_prefix(b"include") else {
            continue;
        };
        let s = trim_ascii_start(s);
        let close = match s.first() {
            Some(b'"') => b'"',
            Some(b'<') => b'>',
            _ => continue,
        };
        let inner = &s[1..];
        let Some(end) = inner.iter().position(|&b| b == close) else {
            continue;
        };
        let name = &inner[..end];
        let base = name
            .rsplit(|&b| b == b'/' || b == b'\\')
            .next()
            .unwrap_or(name);
        if base.is_empty() {
            continue;
        }
        if let Ok(base) = std::str::from_utf8(base) {
            referenced.insert(base.to_string());
        }
    }
}

fn trim_ascii_start(mut s: &[u8]) -> &[u8] {
    while let Some((first, rest)) = s.split_first() {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

fn create_xwin_file_alias(src: &Path, alias: &Path) -> Result<(), SoldrError> {
    match std::fs::hard_link(src, alias) {
        Ok(()) => Ok(()),
        Err(hardlink_err) => {
            if alias.exists() {
                return Ok(());
            }
            std::fs::copy(src, alias).map(|_| ()).map_err(|copy_err| {
                SoldrError::Other(format!(
                    "create xwin cache case alias {} -> {}: \
                     hardlink failed: {hardlink_err}; copy failed: {copy_err}",
                    alias.display(),
                    src.display()
                ))
            })
        }
    }
}

fn extract_tar_zst_tree<R: std::io::Read>(reader: R, dest: &Path) -> Result<(), SoldrError> {
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder init: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    archive
        .unpack(dest)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_well_formed() {
        // Bare smoke: catch typos in the URLs / hex sha256s during refactor.
        for url in [XWIN_CACHE_X86_64_URL, XWIN_CACHE_AARCH64_URL] {
            assert!(url.starts_with("https://"), "URL must be HTTPS: {url}");
            assert!(
                url.ends_with(".tar.zst"),
                "URL must reference a .tar.zst bundle: {url}"
            );
            assert!(
                url.contains(MANAGED_XWIN_CACHE_VERSION),
                "URL must embed MANAGED_XWIN_CACHE_VERSION ({MANAGED_XWIN_CACHE_VERSION}): {url}"
            );
        }
        for sha256 in [XWIN_CACHE_X86_64_SHA256, XWIN_CACHE_AARCH64_SHA256] {
            assert_eq!(sha256.len(), 64);
            assert!(sha256.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn aarch64_asset_pin_matches_catalogue() {
        assert!(XWIN_CACHE_AARCH64_URL.contains("windows-aarch64-msvc"));
        assert_eq!(
            XWIN_CACHE_AARCH64_SHA256,
            "cb7fa0e68ce173a54f0dbc116d3e8f04c7013953529ada8284b0ac149139b9da"
        );
    }

    #[test]
    fn cache_root_accepts_legacy_forge_and_flat_layouts() {
        for relative in [Some("xwin"), Some("package"), None] {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let root =
                relative.map_or_else(|| tmp.path().to_path_buf(), |dir| tmp.path().join(dir));
            std::fs::create_dir_all(root.join("crt")).expect("crt");
            std::fs::create_dir_all(root.join("sdk")).expect("sdk");
            assert_eq!(resolve_xwin_cache_dir(tmp.path()), Some(root));
        }
    }

    /// Probe whether `dir`'s filesystem resolves names case-insensitively
    /// (macOS APFS default, Windows NTFS). On such filesystems no aliases
    /// are needed (or creatable) — tests adjust expectations accordingly.
    fn is_case_insensitive_fs(dir: &Path) -> bool {
        let probe = dir.join("CaseProbe");
        std::fs::write(&probe, b"").expect("write case probe");
        let insensitive = dir.join("caseprobe").exists();
        std::fs::remove_file(&probe).ok();
        insensitive
    }

    #[test]
    fn include_referenced_case_alias_materializes_driverspecs() {
        // cross-run 28574600982 regression fixture: the catalogue xwin bundle ships
        // `driverspecs.h` (lowercase) while `kernelspecs.h` references
        // `#include "DriverSpecs.h"` — every `windows.h` compile on a
        // case-sensitive filesystem died with "file not found".
        let tmp = tempfile::tempdir().expect("tmpdir");
        let xwin = tmp.path().join("xwin");
        let shared = xwin.join("sdk").join("include").join("shared");
        std::fs::create_dir_all(&shared).expect("mkdir shared");
        std::fs::write(
            shared.join("kernelspecs.h"),
            b"#pragma once\n#include \"DriverSpecs.h\"\n",
        )
        .expect("write kernelspecs");
        std::fs::write(shared.join("driverspecs.h"), b"#pragma once\n").expect("write driverspecs");

        let case_insensitive = is_case_insensitive_fs(tmp.path());
        let created = ensure_xwin_case_aliases(&xwin).expect("aliases");
        let expected = if case_insensitive { 0 } else { 1 };
        assert_eq!(created, expected, "DriverSpecs.h alias count");
        // Resolvable under the referenced casing either way.
        assert!(shared.join("DriverSpecs.h").is_file());

        // Idempotent on re-run.
        let again = ensure_xwin_case_aliases(&xwin).expect("aliases again");
        assert_eq!(again, 0);
    }

    #[test]
    fn include_scan_handles_angle_subdir_and_whitespace() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let xwin = tmp.path().join("xwin");
        let um = xwin.join("sdk").join("include").join("um");
        std::fs::create_dir_all(&um).expect("mkdir um");
        // Angle include, subdir-qualified reference, and whitespace
        // between `#` and `include` — all must resolve to basenames.
        std::fs::write(
            um.join("consumer.h"),
            b"  #  include <foo/BarBaz.h>\n#include \"QuuxThing.h\"\nint x;\n",
        )
        .expect("write consumer");
        std::fs::write(um.join("barbaz.h"), b"x").expect("write barbaz");
        std::fs::write(um.join("quuxthing.h"), b"y").expect("write quuxthing");

        let case_insensitive = is_case_insensitive_fs(tmp.path());
        let created = ensure_xwin_case_aliases(&xwin).expect("aliases");
        let expected = if case_insensitive { 0 } else { 2 };
        assert_eq!(created, expected);
        assert!(um.join("BarBaz.h").is_file());
        assert!(um.join("QuuxThing.h").is_file());
    }

    #[test]
    fn scan_include_directives_parses_directive_shapes() {
        let mut refs = HashSet::new();
        scan_include_directives(
            b"#include <a/b/Name.h>\n\
              # include \"Other.H\"\n\
              #include<NoSpace.h>\n\
              // #include commented is still fine to over-collect\n\
              #define X 1\n\
              #include \"backslash\\Sub\\Deep.h\"\n\
              #include \"\"\n\
              #include <unclosed\n",
            &mut refs,
        );
        assert!(refs.contains("Name.h"), "{refs:?}");
        assert!(refs.contains("Other.H"), "{refs:?}");
        assert!(refs.contains("NoSpace.h"), "{refs:?}");
        assert!(refs.contains("Deep.h"), "{refs:?}");
        assert!(!refs.contains(""), "empty names must be dropped: {refs:?}");
        assert!(
            !refs.iter().any(|r| r.contains("unclosed")),
            "unclosed directives must be ignored: {refs:?}"
        );
    }

    #[test]
    fn unsupported_target_yields_unsupported_platform() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_xwin_cache(&paths, "x86_64-unknown-linux-gnu"));
        let err = result.expect_err("non-msvc target must error");
        assert!(
            matches!(err, SoldrError::UnsupportedPlatform(_)),
            "expected UnsupportedPlatform, got: {err:?}"
        );
    }
}
