//! Resolution for tools repackaged by soldr-toolchain across every target.

use super::{
    archive, check_cache, current_unix_ms, manifest_lookup, smoke_test_or_evict, FetchResult,
};
use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

/// Exact filename for binaries that soldr-toolchain republishes across all
/// eight supported targets. Upstream Dylint 6.0.3 only publishes Linux GNU;
/// these explicitly opted-in packages keep the binary-only contract intact on
/// Windows, macOS, and musl without weakening catalogue SHA verification.
pub(super) fn asset_name(cache_name: &str, version: &str, target: &TargetTriple) -> Option<String> {
    // The catalogued asset prefix usually equals the cache name. maturin is
    // the exception: soldr caches and invokes the binary as `maturin`, but
    // the toolchain catalogues the forge-built blobs under the fork/package
    // identity `soldr-maturin` (soldr#2573), so the prefix is mapped rather
    // than assumed.
    let asset_prefix = match cache_name {
        "cargo-dylint" | "dylint-link" | "dylint-driver" => cache_name,
        "maturin" => "soldr-maturin",
        _ => return None,
    };
    Some(format!(
        "{}-{}-{}.tar.gz",
        asset_prefix,
        version.trim_start_matches('v'),
        target.triple()
    ))
}

/// Resolve an explicitly supported soldr-toolchain repackaged binary by its
/// exact versioned filename. The catalogue owner is intentionally not the
/// upstream repository: these rows are produced and hosted by
/// `zackees/soldr-toolchain`, and a unique exact filename plus its SHA-256 pin
/// is the complete identity needed by this path.
pub(super) async fn try_binary(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    version: &str,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let Some(asset_name) = asset_name(cache_name, version, target) else {
        return Ok(None);
    };
    let manifest = manifest_lookup::get_or_fetch().await;
    let matches = manifest.lookup_asset(&asset_name);
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 {
        return Err(SoldrError::Other(format!(
            "toolchain catalogue has {} rows for exact asset {asset_name}",
            matches.len()
        )));
    }
    let entry = matches[0];
    let bare_version = version.trim_start_matches('v');
    if let Some(result) = check_cache(paths, cache_name, bare_version, binary_names, target)? {
        return Ok(Some(result));
    }

    eprintln!(
        "soldr: toolchain catalogue hit for {} v{} {} -> {}",
        cache_name,
        bare_version,
        target.triple(),
        entry.asset
    );
    let download_started_at_ms = current_unix_ms();
    let download_started = std::time::Instant::now();
    let downloaded = manifest_lookup::materialize_catalogue_entry(paths, entry).await?;
    let binary_path = archive::extract_catalogue_asset_with_pin(
        paths,
        cache_name,
        bare_version,
        entry,
        downloaded.path(),
        target,
        binary_names,
    )
    .await?;
    if cache_name != "dylint-driver" {
        smoke_test_or_evict(&binary_path, cache_name, target)?;
    }
    soldr_core::build_log_meta::fetch_timing::record(
        soldr_core::build_log_meta::fetch_timing::FetchTiming {
            name: cache_name.to_string(),
            source: "catalogue".to_string(),
            started_at_ms: download_started_at_ms,
            duration_ms: download_started.elapsed().as_millis() as u64,
        },
    );

    Ok(Some(FetchResult {
        binary_path,
        version: bare_version.to_string(),
        cached: false,
    }))
}

/// Materialize the exact catalogued Dylint driver in cargo-dylint's cache.
///
/// Catalogue archives use the native Windows `.exe` filename, but
/// cargo-dylint's cross-platform cache contract is deliberately extensionless:
/// `$DYLINT_DRIVER_PATH/<nightly>-<host>/dylint-driver`.
pub async fn ensure_dylint_driver(
    paths: &SoldrPaths,
    dylint_version: &str,
    channel: &str,
    driver_root: &Path,
) -> Result<Option<PathBuf>, SoldrError> {
    let target = TargetTriple::host()?;
    let Some(dated_channel) = dated_nightly_prefix(channel) else {
        return Err(SoldrError::Other(format!(
            "Dylint driver catalogue lookup requires a dated nightly, got `{channel}`"
        )));
    };
    let asset_version = format!("{}-{dated_channel}", dylint_version.trim_start_matches('v'));
    let Some(result) = try_binary(
        paths,
        "dylint-driver",
        &["dylint-driver"],
        &asset_version,
        &target,
    )
    .await?
    else {
        return Ok(None);
    };

    let qualified_channel = format!("{dated_channel}-{}", target.triple());
    let destination =
        install_extensionless_driver(&result.binary_path, driver_root, &qualified_channel)?;
    eprintln!(
        "soldr: installed catalogued Dylint driver {} at {}",
        asset_version,
        destination.display()
    );
    Ok(Some(destination))
}

fn install_extensionless_driver(
    source: &Path,
    driver_root: &Path,
    qualified_channel: &str,
) -> Result<PathBuf, SoldrError> {
    let driver_dir = driver_root.join(qualified_channel);
    std::fs::create_dir_all(&driver_dir)?;
    let destination = driver_dir.join("dylint-driver");
    // Unix hosts get a loader-env wrapper in the `dylint-driver` slot and
    // the real binary beside it (soldr#2634 finding 4): the catalogued
    // driver's baked rpath names the *builder's* rustup layout, so a host
    // with any other `RUSTUP_HOME` fails
    // `error while loading shared libraries: librustc_driver-…` the
    // moment cargo-dylint execs the driver directly — soldr's own probe
    // survives only because it injects the toolchain `lib/` itself. The
    // wrapper derives that directory from the invoking environment at run
    // time, so the staged driver is layout-portable. Windows resolves
    // rustc DLLs through PATH, which cargo-dylint already provides.
    let payload_destination =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            destination.clone()
        } else {
            driver_dir.join("dylint-driver-real")
        };
    install_driver_file_atomically(source, &driver_dir, &payload_destination, |src, tmp| {
        std::fs::copy(src, tmp).map(|_| ())
    })?;
    if payload_destination != destination {
        let script = driver_loader_wrapper_script(qualified_channel);
        install_driver_file_atomically(source, &driver_dir, &destination, |_, tmp| {
            std::fs::write(tmp, &script)
        })?;
    }
    Ok(destination)
}

/// Write one staged driver file through a part-file + rename so a
/// concurrent reader never observes a torn executable.
fn install_driver_file_atomically(
    source: &Path,
    driver_dir: &Path,
    destination: &Path,
    materialize: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), SoldrError> {
    let file_name = destination
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("dylint-driver");
    let temporary = driver_dir.join(format!(".{file_name}.part-{}", std::process::id()));
    materialize(source, &temporary)?;
    crate::platform::fs::permissions::make_executable(&temporary)?;
    if destination.is_file() {
        std::fs::remove_file(destination)?;
    }
    if let Err(error) = std::fs::rename(&temporary, destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// The Unix `dylint-driver` slot: export the toolchain's shared-library
/// directory for the loader, then exec the real catalogued binary.
///
/// The toolchain directory is derived from `RUSTUP_HOME` (or its default)
/// at *run* time, so one staged driver works across hosts whose rustup
/// layouts differ from the catalogue builder's. A missing directory adds
/// nothing and lets the exec proceed — an rpath-compatible layout still
/// works, and an incompatible one fails with the loader's own message.
fn driver_loader_wrapper_script(qualified_channel: &str) -> String {
    format!(
        r#"#!/bin/sh
# soldr: loader-env wrapper for the catalogued Dylint driver (soldr#2634).
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
lib="${{RUSTUP_HOME:-$HOME/.rustup}}/toolchains/{qualified_channel}/lib"
if [ -d "$lib" ]; then
  if [ "$(uname)" = "Darwin" ]; then
    DYLD_FALLBACK_LIBRARY_PATH="$lib${{DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}}"
    export DYLD_FALLBACK_LIBRARY_PATH
  else
    LD_LIBRARY_PATH="$lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
    export LD_LIBRARY_PATH
  fi
fi
exec "$dir/dylint-driver-real" "$@"
"#
    )
}

fn dated_nightly_prefix(channel: &str) -> Option<&str> {
    let prefix = channel.get(..18)?;
    (prefix.starts_with("nightly-")
        && prefix.as_bytes()[8..]
            .iter()
            .enumerate()
            .all(|(index, byte)| {
                matches!(index, 4 | 7)
                    .then_some(*byte == b'-')
                    .unwrap_or(byte.is_ascii_digit())
            }))
    .then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// soldr#2634 finding 4: the Unix `dylint-driver` slot must be the
    /// loader-env wrapper with the real catalogued binary beside it, so
    /// cargo-dylint's direct exec survives rustup layouts that differ
    /// from the catalogue builder's baked rpath. Windows keeps the plain
    /// copy (DLL resolution goes through PATH there).
    #[test]
    fn staged_driver_layout_is_loader_portable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source = temp.path().join("fetched-driver");
        std::fs::write(&source, b"driver-payload").expect("source");
        let root = temp.path().join("drivers");
        let channel = "nightly-2026-05-28-x86_64-unknown-linux-gnu";

        let destination =
            install_extensionless_driver(&source, &root, channel).expect("stage driver");
        assert_eq!(destination, root.join(channel).join("dylint-driver"));
        let staged = std::fs::read(&destination).expect("read staged slot");

        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert_eq!(staged, b"driver-payload", "Windows stages the plain copy");
            return;
        }
        let script = String::from_utf8(staged).expect("wrapper script is text");
        assert!(script.starts_with("#!/bin/sh"), "wrapper must be a script");
        assert!(
            script.contains(&format!("toolchains/{channel}/lib")),
            "wrapper must derive the staged channel's lib dir: {script}"
        );
        assert!(
            script.contains("exec \"$dir/dylint-driver-real\""),
            "wrapper must exec the real driver: {script}"
        );
        let payload = std::fs::read(root.join(channel).join("dylint-driver-real"))
            .expect("real driver beside the wrapper");
        assert_eq!(payload, b"driver-payload");
    }

    /// Restaging over an existing installation must replace both files.
    #[test]
    fn restaging_replaces_an_existing_driver() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let source_one = temp.path().join("driver-one");
        let source_two = temp.path().join("driver-two");
        std::fs::write(&source_one, b"one").expect("source one");
        std::fs::write(&source_two, b"two").expect("source two");
        let root = temp.path().join("drivers");
        let channel = "nightly-2026-05-28-x86_64-unknown-linux-gnu";

        install_extensionless_driver(&source_one, &root, channel).expect("first stage");
        install_extensionless_driver(&source_two, &root, channel).expect("second stage");

        let payload_path = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            root.join(channel).join("dylint-driver")
        } else {
            root.join(channel).join("dylint-driver-real")
        };
        assert_eq!(std::fs::read(payload_path).expect("payload"), b"two");
    }

    #[test]
    fn dylint_packages_cover_all_supported_targets() {
        let targets = [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ];
        for triple in targets {
            let target = TargetTriple::from_triple(triple).unwrap();
            assert_eq!(
                asset_name("cargo-dylint", "v6.0.3", &target),
                Some(format!("cargo-dylint-6.0.3-{triple}.tar.gz"))
            );
            assert_eq!(
                asset_name("dylint-link", "6.0.3", &target),
                Some(format!("dylint-link-6.0.3-{triple}.tar.gz"))
            );
            assert_eq!(
                asset_name("dylint-driver", "6.0.3-nightly-2026-05-28", &target),
                Some(format!(
                    "dylint-driver-6.0.3-nightly-2026-05-28-{triple}.tar.gz"
                ))
            );
        }
        let host = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset_name("cargo-nextest", "1", &host), None);
    }

    // The forge-built maturin blobs are catalogued under the fork/package
    // identity `soldr-maturin`, while soldr's cache name for the tool is
    // plain `maturin` (soldr#2573). The mapping must produce the catalogued
    // filename exactly, for every supported target, or the sha-pinned CDN
    // rung silently never fires and the fetch falls through to GitHub.
    #[test]
    fn maturin_maps_to_the_soldr_maturin_catalogue_prefix() {
        let targets = [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ];
        for triple in targets {
            let target = TargetTriple::from_triple(triple).unwrap();
            assert_eq!(
                asset_name("maturin", "1.14.1.post1", &target),
                Some(format!("soldr-maturin-1.14.1.post1-{triple}.tar.gz"))
            );
        }
        // The mapped prefix must not leak to lookalike cache names.
        let host = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset_name("soldr-maturin", "1.14.1.post1", &host), None);
    }

    #[test]
    fn dated_nightly_accepts_qualified_channel() {
        assert_eq!(
            dated_nightly_prefix("nightly-2026-05-28-x86_64-pc-windows-msvc"),
            Some("nightly-2026-05-28")
        );
        assert_eq!(dated_nightly_prefix("1.94.1"), None);
    }

    #[test]
    fn driver_installation_is_extensionless_on_every_host() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("dylint-driver.exe");
        std::fs::write(&source, b"driver").unwrap();
        let destination = install_extensionless_driver(
            &source,
            &dir.path().join("drivers"),
            "nightly-2026-05-28-x86_64-pc-windows-msvc",
        )
        .unwrap();
        assert_eq!(destination.file_name().unwrap(), "dylint-driver");
        // The extensionless slot holds the payload directly on Windows
        // and the loader-env wrapper on Unix (soldr#2634 finding 4); the
        // payload then lives beside it. Either way the payload bytes are
        // staged under the extensionless contract.
        let payload = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            destination
        } else {
            destination.with_file_name("dylint-driver-real")
        };
        assert_eq!(std::fs::read(payload).unwrap(), b"driver");
    }

    #[test]
    fn catalogued_dylint_binary_is_smoked_and_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("cargo-dylint");
        std::fs::write(&bogus, b"not an executable").unwrap();
        let target = TargetTriple::host().unwrap();

        let error = smoke_test_or_evict(&bogus, "cargo-dylint", &target).unwrap_err();

        assert!(error.to_string().contains("smoke test failed"));
        assert!(!bogus.exists(), "failed catalogue binary must be evicted");
    }
}
