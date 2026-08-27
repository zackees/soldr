//! Integration test that drives `cache_lib::save::load` with
//! `auto_defender_exclude = true` and asserts the RAII guard issued
//! both an `Add-MpPreference` on entry and a matching
//! `Remove-MpPreference` on Drop.
//!
//! Uses the `SOLDR_TEST_ASSUME_ADMIN`, `SOLDR_TEST_DEFENDER_LOG`, and
//! `SOLDR_TEST_DEFENDER_EXISTING` seams already wired through
//! `crate::defender` so the real PowerShell never runs. Windows-only:
//! the guard short-circuits on non-Windows.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use soldr_cli::cache_lib::save::{
    load, save, LoadOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
};
use soldr_cli::defender::{
    SOLDR_TEST_ASSUME_ADMIN_ENV, SOLDR_TEST_DEFENDER_EXISTING_ENV, SOLDR_TEST_DEFENDER_LOG_ENV,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn load_auto_defender_exclude_adds_then_removes_on_drop() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let dir = tempfile::tempdir().expect("tempdir");
    let ws = dir.path().join("workspace");
    let cache = dir.path().join("cache");
    let archive = dir.path().join("snap.tar.zst");

    write(
        &ws.join("Cargo.toml"),
        b"[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    );
    write(&ws.join("src/main.rs"), b"fn main() {}\n");
    write(&cache.join("ab/cd/object.bin"), &[0xAA; 256]);

    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    fs::remove_dir_all(&cache).unwrap();
    fs::create_dir_all(&cache).unwrap();

    let defender_log = dir.path().join("defender.tsv");
    // Why: `existing` is empty so apply_exclusions takes the real Add
    // path (not AlreadyApplied) and the guard tracks the path for Drop.
    let existing_path = dir.path().join("existing.txt");
    write(&existing_path, b"");

    let _admin = EnvVarGuard::set(SOLDR_TEST_ASSUME_ADMIN_ENV, "1");
    let _log = EnvVarGuard::set(SOLDR_TEST_DEFENDER_LOG_ENV, &defender_log);
    let _existing = EnvVarGuard::set(SOLDR_TEST_DEFENDER_EXISTING_ENV, &existing_path);

    load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: true,
    })
    .expect("load ok");

    let log_body = fs::read_to_string(&defender_log).expect("defender log written");
    let mut lines = log_body.lines();
    let first = lines.next().expect("Add-MpPreference line present");
    let second = lines.next().expect("Remove-MpPreference line present");

    let expected_path = cache.display().to_string();
    assert_eq!(
        first,
        format!("Add-MpPreference\t{expected_path}"),
        "first line should be the Add issued on guard creation"
    );
    assert_eq!(
        second,
        format!("Remove-MpPreference\t{expected_path}"),
        "second line should be the Remove issued on guard Drop"
    );
    assert!(
        lines.next().is_none(),
        "exactly two cmdlet invocations expected"
    );
}
