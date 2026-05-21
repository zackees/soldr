//! Integration tests for `soldr cache install-zccache` (the library
//! half — pure path/archive/system flows, no CLI subprocess).
//!
//! Uses fake binary bytes everywhere; nothing here requires a real
//! zccache. Tests that touch the network are guarded by `#[ignore]`.

use soldr_core::SoldrPaths;
use soldr_fetch::{
    install_zccache_from_source, pinned_version_drift_from_managed, pinned_zccache_dir,
    read_pinned_sidecar, remove_pinned_zccache, resolve_pinned_zccache, InstallSource,
    MANAGED_ZCCACHE_VERSION,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Tests need to invoke install_zccache_from_source with a TargetTriple
/// matching the host so the .exe suffix lines up with the fake binaries
/// we lay down. We use the public surface (which detects the host) and
/// drop `.exe` on Unix / keep `.exe` on Windows.
fn bin_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn write_fake(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn isolate_local_dir_env() -> EnvGuard {
    EnvGuard::unset("SOLDR_ZCCACHE_LOCAL_DIR")
}

/// RAII wrapper that restores an env var on drop. Tests must use this
/// instead of touching env vars directly so failures don't leak state
/// into sibling tests.
struct EnvGuard {
    key: String,
    prior: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn unset(key: &str) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: tests run with `cargo test` which permits env
        // mutation; the harness assumes single-threaded teardown
        // within each test process.
        unsafe {
            std::env::remove_var(key);
        }
        Self {
            key: key.to_string(),
            prior,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: same scope as set in `unset`.
        unsafe {
            match self.prior.take() {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}

fn seed_dir_source(root: &Path) -> PathBuf {
    let src = root.join("bin-dir");
    write_fake(&src, &bin_name("zccache"), b"cli-bytes-v1");
    write_fake(&src, &bin_name("zccache-daemon"), b"daemon-bytes-v1");
    write_fake(&src, &bin_name("zccache-fp"), b"fp-bytes-v1");
    src
}

#[tokio::test]
async fn path_directory_install_records_sidecar_and_copies_all_three() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root.clone());

    let src = seed_dir_source(tmp.path());
    let report = install_zccache_from_source(&InstallSource::Path(src.clone()), &paths)
        .await
        .expect("install from directory");

    // Layout: every binary copied + sidecar exists.
    let pinned = pinned_zccache_dir(&paths);
    assert!(pinned.join(bin_name("zccache")).exists());
    assert!(pinned.join(bin_name("zccache-daemon")).exists());
    assert!(pinned.join(bin_name("zccache-fp")).exists());
    assert!(pinned.join("source.json").exists());

    // Report sanity.
    assert_eq!(report.source_kind, "path");
    assert_eq!(report.source_value, src.display().to_string());
    assert_eq!(report.binaries.len(), 3);
    let cli_record = &report.binaries["zccache"];
    assert_eq!(cli_record.size_bytes, b"cli-bytes-v1".len() as u64);
    assert_eq!(cli_record.sha256.len(), 64);

    // Sidecar roundtrip matches the report.
    let sidecar = read_pinned_sidecar(&paths).unwrap().unwrap();
    assert_eq!(sidecar.source_kind, "path");
    assert_eq!(sidecar.binaries.len(), 3);
    assert_eq!(sidecar.binaries["zccache"].sha256, cli_record.sha256);

    // Resolution chain: pinned wins.
    let resolved = resolve_pinned_zccache(&paths).unwrap().unwrap();
    assert!(resolved.binary_path.ends_with(bin_name("zccache")));
}

#[tokio::test]
async fn path_tar_gz_install_extracts_nested_layout() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    let archive = build_tar_gz_with_nested_dir(tmp.path(), "zccache-v1.8.1");
    let report = install_zccache_from_source(&InstallSource::Path(archive.clone()), &paths)
        .await
        .expect("install from .tar.gz");

    assert_eq!(report.source_kind, "path");
    let pinned = pinned_zccache_dir(&paths);
    assert!(pinned.join(bin_name("zccache")).exists());
    assert!(pinned.join(bin_name("zccache-daemon")).exists());
    assert!(pinned.join(bin_name("zccache-fp")).exists());
    let sidecar = read_pinned_sidecar(&paths).unwrap().unwrap();
    assert_eq!(sidecar.binaries.len(), 3);
}

#[tokio::test]
async fn path_tar_zst_install_extracts_archive() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    let archive = build_tar_zst_with_nested_dir(tmp.path(), "zccache-v1.8.1");
    let report = install_zccache_from_source(&InstallSource::Path(archive), &paths)
        .await
        .expect("install from .tar.zst");

    assert_eq!(report.source_kind, "path");
    let pinned = pinned_zccache_dir(&paths);
    assert!(pinned.join(bin_name("zccache")).exists());
}

#[tokio::test]
async fn path_zip_install_extracts_archive() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    let archive = build_zip_with_nested_dir(tmp.path(), "zccache-v1.8.1");
    let report = install_zccache_from_source(&InstallSource::Path(archive), &paths)
        .await
        .expect("install from .zip");

    assert_eq!(report.source_kind, "path");
    let pinned = pinned_zccache_dir(&paths);
    assert!(pinned.join(bin_name("zccache")).exists());
}

#[tokio::test]
async fn path_archive_missing_one_binary_errors() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    // Build an archive that contains only two of the three binaries.
    let staging = tmp.path().join("staging");
    let inner = staging.join("zccache-v1.8.1");
    write_fake(&inner, &bin_name("zccache"), b"cli");
    write_fake(&inner, &bin_name("zccache-daemon"), b"daemon");
    let archive = tmp.path().join("incomplete.tar.gz");
    write_tar_gz(&archive, &staging).unwrap();

    let err = install_zccache_from_source(&InstallSource::Path(archive), &paths)
        .await
        .expect_err("missing binary should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("zccache-fp") || msg.contains("missing required binary"),
        "error message should name the missing binary: {msg}"
    );
}

#[tokio::test]
async fn path_unknown_extension_errors() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    // A single random file with an unrecognized extension.
    let single = tmp.path().join("zccache-bundle.7z");
    fs::write(&single, b"not actually 7z").unwrap();

    let err = install_zccache_from_source(&InstallSource::Path(single), &paths)
        .await
        .expect_err("unknown ext should fail");
    let msg = err.to_string();
    assert!(
        msg.contains(".zip") && msg.contains(".tar.gz") && msg.contains(".tar.zst"),
        "error must list supported extensions: {msg}"
    );
}

#[tokio::test]
async fn system_source_uses_path_lookup_via_real_path_env() {
    // Uses the public install_zccache_from_source surface, which reads
    // $PATH at runtime. Build a fresh PATH containing only a directory
    // we control (so we don't accidentally resolve a real zccache on
    // the developer's machine).
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    let bin_dir = tmp.path().join("fake-system-bin");
    write_fake(&bin_dir, &bin_name("zccache"), b"system-cli");
    write_fake(&bin_dir, &bin_name("zccache-daemon"), b"system-daemon");
    write_fake(&bin_dir, &bin_name("zccache-fp"), b"system-fp");

    let _path_guard = PathGuard::set_only(&bin_dir);
    let report = install_zccache_from_source(&InstallSource::System, &paths)
        .await
        .expect("system install");
    assert_eq!(report.source_kind, "system");
    assert_eq!(report.source_value, "system");
    let pinned = pinned_zccache_dir(&paths);
    assert!(pinned.join(bin_name("zccache")).exists());
}

#[tokio::test]
async fn system_source_missing_binary_reports_which_one() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    let bin_dir = tmp.path().join("fake-system-bin");
    write_fake(&bin_dir, &bin_name("zccache"), b"system-cli");
    // intentionally skip zccache-daemon
    write_fake(&bin_dir, &bin_name("zccache-fp"), b"system-fp");

    let _path_guard = PathGuard::set_only(&bin_dir);
    let err = install_zccache_from_source(&InstallSource::System, &paths)
        .await
        .expect_err("missing daemon should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("zccache-daemon"),
        "error must name the missing binary: {msg}"
    );
    assert!(msg.contains("PATH"));
}

#[test]
fn remove_is_idempotent() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    // First removal on a fresh root is a no-op.
    assert!(!remove_pinned_zccache(&paths).unwrap());

    // Seed a pinned dir manually and verify removal returns true then
    // false on a second call.
    let pinned = pinned_zccache_dir(&paths);
    fs::create_dir_all(&pinned).unwrap();
    fs::write(pinned.join("dummy"), b"hello").unwrap();
    assert!(remove_pinned_zccache(&paths).unwrap());
    assert!(!remove_pinned_zccache(&paths).unwrap());
}

#[test]
fn status_with_no_install_returns_none() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    assert!(read_pinned_sidecar(&paths).unwrap().is_none());
    assert!(resolve_pinned_zccache(&paths).unwrap().is_none());
}

#[tokio::test]
async fn status_with_install_reports_drift_when_version_differs() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root);

    let src = seed_dir_source(tmp.path());
    install_zccache_from_source(&InstallSource::Path(src), &paths)
        .await
        .expect("install");

    let mut sidecar = read_pinned_sidecar(&paths).unwrap().unwrap();
    // Fake binaries can't be probed for version, so version is "unknown"
    // — no drift.
    assert_eq!(sidecar.version, "unknown");
    assert!(!pinned_version_drift_from_managed(&sidecar));

    // Force a drift scenario.
    sidecar.version = "0.0.1".to_string();
    assert!(pinned_version_drift_from_managed(&sidecar));

    // The managed default never drifts against itself.
    sidecar.version = MANAGED_ZCCACHE_VERSION.to_string();
    assert!(!pinned_version_drift_from_managed(&sidecar));
}

#[tokio::test]
async fn resolution_chain_pinned_overrides_managed_default() {
    let _guard = isolate_local_dir_env();
    let tmp = tempfile::tempdir().unwrap();
    let soldr_root = tmp.path().join("soldr-root");
    let paths = SoldrPaths::with_root(soldr_root.clone());

    let src = seed_dir_source(tmp.path());
    install_zccache_from_source(&InstallSource::Path(src), &paths)
        .await
        .expect("install");

    // cached_zccache_binary picks pinned over managed.
    let cached = soldr_fetch::cached_zccache_binary(&paths)
        .unwrap()
        .expect("pinned must be reported as cached");
    let pinned_dir = pinned_zccache_dir(&paths);
    assert!(
        cached.binary_path.starts_with(&pinned_dir),
        "expected cached path under {} but got {}",
        pinned_dir.display(),
        cached.binary_path.display()
    );
}

// ---------------------------------------------------------------------
// PathGuard — RAII wrapper that sets PATH for the duration of a test
// and restores it on drop. Tests that touch PATH MUST hold the global
// mutex first; otherwise parallel tests race on the env var and the
// install lookup sees an inconsistent PATH.
// ---------------------------------------------------------------------

fn path_mutex() -> &'static std::sync::Mutex<()> {
    static MUTEX: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    MUTEX.get_or_init(|| std::sync::Mutex::new(()))
}

struct PathGuard {
    prior: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl PathGuard {
    fn set_only(dir: &Path) -> Self {
        // Acquire the mutex first; a poisoned lock just means a prior
        // test panicked while holding it. Recover the guard and carry
        // on so a single failure doesn't cascade.
        let lock = path_mutex().lock().unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os("PATH");
        // SAFETY: tests run with `cargo test` which permits env mutation.
        unsafe {
            std::env::set_var("PATH", dir);
        }
        Self { prior, _lock: lock }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // SAFETY: same scope as set_only.
        unsafe {
            match self.prior.take() {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

// ---------------------------------------------------------------------
// Archive builders
// ---------------------------------------------------------------------

fn build_tar_gz_with_nested_dir(root: &Path, inner_dirname: &str) -> PathBuf {
    let staging = root.join(format!("staging-{inner_dirname}-tar-gz"));
    let inner = staging.join(inner_dirname);
    write_fake(&inner, &bin_name("zccache"), b"tar-gz-cli");
    write_fake(&inner, &bin_name("zccache-daemon"), b"tar-gz-daemon");
    write_fake(&inner, &bin_name("zccache-fp"), b"tar-gz-fp");
    let archive = root.join("bundle.tar.gz");
    write_tar_gz(&archive, &staging).unwrap();
    archive
}

fn build_tar_zst_with_nested_dir(root: &Path, inner_dirname: &str) -> PathBuf {
    let staging = root.join(format!("staging-{inner_dirname}-tar-zst"));
    let inner = staging.join(inner_dirname);
    write_fake(&inner, &bin_name("zccache"), b"tar-zst-cli");
    write_fake(&inner, &bin_name("zccache-daemon"), b"tar-zst-daemon");
    write_fake(&inner, &bin_name("zccache-fp"), b"tar-zst-fp");
    let archive = root.join("bundle.tar.zst");
    write_tar_zst(&archive, &staging).unwrap();
    archive
}

fn build_zip_with_nested_dir(root: &Path, inner_dirname: &str) -> PathBuf {
    let staging = root.join(format!("staging-{inner_dirname}-zip"));
    let inner = staging.join(inner_dirname);
    write_fake(&inner, &bin_name("zccache"), b"zip-cli");
    write_fake(&inner, &bin_name("zccache-daemon"), b"zip-daemon");
    write_fake(&inner, &bin_name("zccache-fp"), b"zip-fp");
    let archive = root.join("bundle.zip");
    write_zip(&archive, &staging, inner_dirname).unwrap();
    archive
}

fn write_tar_gz(out: &Path, staging: &Path) -> std::io::Result<()> {
    let file = fs::File::create(out)?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);
    builder.append_dir_all(".", staging)?;
    builder.into_inner()?.finish()?;
    Ok(())
}

fn write_tar_zst(out: &Path, staging: &Path) -> std::io::Result<()> {
    let file = fs::File::create(out)?;
    let encoder = zstd::stream::write::Encoder::new(file, 3).map_err(std::io::Error::other)?;
    let mut builder = tar::Builder::new(encoder.auto_finish());
    builder.append_dir_all(".", staging)?;
    builder.finish()?;
    Ok(())
}

fn write_zip(out: &Path, staging: &Path, inner_dirname: &str) -> std::io::Result<()> {
    let file = fs::File::create(out)?;
    let mut writer = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
    for name in ["zccache", "zccache-daemon", "zccache-fp"] {
        let file_name = bin_name(name);
        let bytes = fs::read(staging.join(inner_dirname).join(&file_name))?;
        let path_in_zip = format!("{inner_dirname}/{file_name}");
        writer
            .start_file(&path_in_zip, opts)
            .map_err(std::io::Error::other)?;
        writer.write_all(&bytes)?;
    }
    writer.finish().map_err(std::io::Error::other)?;
    Ok(())
}
