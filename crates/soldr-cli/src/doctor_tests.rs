//! Unit coverage split from `doctor.rs` to keep the production module below
//! the soldr#2493 1,000-line ceiling.

use super::*;
use std::sync::Mutex;

/// Guards the `SOLDR_TEST_ZCCACHE_SCAN_ROOT` /
/// `SOLDR_TEST_PROCESS_LIST_FILE` seam env vars against parallel
/// test interference (standard in-crate pattern, e.g.
/// `core::toolchain_resolve`).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII env-var override that restores the previous value on drop.
struct EnvGuard {
    key: &'static str,
    prior: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// soldr#1467: a stale per-launch daemon copy under
// `<root>/<version>/runtime-binaries/` is reported; unrelated files
// in the same tree are not.
#[test]
fn stale_runtime_binaries_scan_finds_daemon_copies() {
    let root = tempfile::tempdir().expect("tempdir");
    let rb = root.path().join("v1.12.14").join("runtime-binaries");
    std::fs::create_dir_all(&rb).expect("create runtime-binaries");
    let daemon_copy = rb.join("zccache-daemon.123.exe");
    std::fs::write(&daemon_copy, b"stub").expect("write daemon copy");
    std::fs::write(rb.join("zccache.exe"), b"stub").expect("write non-daemon file");
    std::fs::write(root.path().join("v1.12.14").join("last-used.txt"), b"x")
        .expect("write sibling file");

    let found = scan_stale_runtime_binaries_in(root.path());
    assert_eq!(
        found,
        vec![daemon_copy.display().to_string()],
        "scan must report exactly the zccache-daemon copy"
    );
}

// soldr#1467: a clean root (runtime-binaries absent or without
// daemon copies) reports nothing; a missing root is silent.
#[test]
fn stale_runtime_binaries_scan_empty_when_clean() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(root.path().join("v1.12.15").join("runtime-binaries"))
        .expect("create empty runtime-binaries");
    assert!(scan_stale_runtime_binaries_in(root.path()).is_empty());
    assert!(scan_stale_runtime_binaries_in(&root.path().join("does-not-exist")).is_empty());
}

// soldr#1467: the `SOLDR_TEST_ZCCACHE_SCAN_ROOT` seam drives the
// full scan entry point.
#[test]
fn stale_runtime_binaries_env_seam_overrides_root() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().expect("tempdir");
    let rb = root.path().join("v1.12.14").join("runtime-binaries");
    std::fs::create_dir_all(&rb).expect("create runtime-binaries");
    let daemon_copy = rb.join("zccache-daemon.772359644.exe");
    std::fs::write(&daemon_copy, b"stub").expect("write daemon copy");

    let _guard = EnvGuard::set(SOLDR_TEST_ZCCACHE_SCAN_ROOT_ENV, root.path().as_os_str());
    assert_eq!(
        scan_stale_runtime_binaries(),
        vec![daemon_copy.display().to_string()]
    );
}

// soldr#1467: the `SOLDR_TEST_PROCESS_LIST_FILE` seam replaces the
// real process scan; only `zccache-daemon*` image names match.
#[test]
fn process_list_seam_reports_daemon_rows() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let list = dir.path().join("procs.txt");
    std::fs::write(
        &list,
        "4242 zccache-daemon.99\n10 cargo\n11 zccache\n12 zccache-daemon.5.exe\n",
    )
    .expect("write process list");

    let _guard = EnvGuard::set(SOLDR_TEST_PROCESS_LIST_FILE_ENV, list.as_os_str());
    assert_eq!(
        scan_standalone_daemon_processes(),
        vec![
            "zccache-daemon.99 (pid 4242)".to_string(),
            "zccache-daemon.5.exe (pid 12)".to_string(),
        ]
    );
}

// soldr#1467: clean seams (empty scan root + empty process list)
// produce two empty vecs — the healthy-box baseline.
#[test]
fn standalone_zccache_probe_clean_baseline_is_empty() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().expect("tempdir");
    let list = root.path().join("procs.txt");
    std::fs::write(&list, "").expect("write empty process list");

    let _root_guard = EnvGuard::set(SOLDR_TEST_ZCCACHE_SCAN_ROOT_ENV, root.path().as_os_str());
    let _list_guard = EnvGuard::set(SOLDR_TEST_PROCESS_LIST_FILE_ENV, list.as_os_str());
    assert!(scan_stale_runtime_binaries().is_empty());
    assert!(scan_standalone_daemon_processes().is_empty());
}

// soldr#1467: Windows `tasklist /FO CSV /NH` rows — image name
// first, PID second, both quoted.
#[test]
fn tasklist_csv_parser_filters_daemon_images() {
    let csv = "\"zccache-daemon.123.exe\",\"4567\",\"Console\",\"1\",\"10,000 K\"\r\n\
                   \"cargo.exe\",\"99\",\"Console\",\"1\",\"5,000 K\"\r\n\
                   \"zccache.exe\",\"100\",\"Console\",\"1\",\"5,000 K\"\r\n";
    assert_eq!(
        parse_tasklist_csv(csv),
        vec!["zccache-daemon.123.exe (pid 4567)".to_string()]
    );
}

/// #590: cover the byte formatter at every unit boundary so the
/// human output stays readable as cache dirs grow.
#[test]
fn fmt_bytes_renders_each_unit() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(1023), "1023 B");
    assert_eq!(fmt_bytes(1024), "1.00 KiB");
    assert_eq!(fmt_bytes(1536), "1.50 KiB");
    assert_eq!(fmt_bytes(1024 * 1024), "1.00 MiB");
    assert_eq!(fmt_bytes(1024 * 1024 * 1024), "1.00 GiB");
    assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
}
