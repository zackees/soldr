use fs2::FileExt;
use sha2::{Digest, Sha256};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const RELOCATED_EXE_ENV_VAR: &str = "SOLDR_RELOCATED_EXE";
pub(crate) const ORIGINAL_EXE_ENV_VAR: &str = "SOLDR_ORIGINAL_EXE";
pub(crate) const FORCE_RELOCATION_ENV_VAR: &str = "SOLDR_TEST_SELF_RELOCATE_FORCE";

const RUNTIME_DIR: &str = "runtime";
const SELF_DIR: &str = "soldr-self";
const LOCK_FILENAME: &str = ".lock";
const GC_MARKER_FILENAME: &str = ".last-gc";
const LAST_USED_FILENAME: &str = "last-used";
const GC_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
const STALE_RUNTIME_SECONDS: u64 = 14 * 24 * 60 * 60;

#[derive(Debug, Default, PartialEq, Eq)]
struct RuntimeGcSummary {
    scanned_dirs: usize,
    removed_dirs: usize,
    skipped_current_dirs: usize,
    skipped_fresh_dirs: usize,
    failed_dirs: usize,
}

pub(crate) fn maybe_reexec_from_runtime(raw_args: &[String]) -> Result<Option<i32>, SoldrError> {
    if !relocation_requested() || relocation_guard_active() {
        return Ok(None);
    }

    let current_exe = std::env::current_exe()?;
    let paths = SoldrPaths::new()?;
    let runtime_root = runtime_root(&paths);
    fs::create_dir_all(&runtime_root)?;

    if path_is_under(&current_exe, &runtime_root) {
        return Ok(None);
    }

    let relocated_exe = ensure_relocated_exe(&paths, &current_exe)?;
    run_periodic_runtime_gc(&paths, Some(&relocated_exe));

    let mut command = Command::new(&relocated_exe);
    command
        .args(raw_args.iter().skip(1))
        .env(RELOCATED_EXE_ENV_VAR, "1")
        .env(ORIGINAL_EXE_ENV_VAR, &current_exe);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(Some(status.code().unwrap_or(1)))
}

pub(crate) fn run_periodic_runtime_gc(paths: &SoldrPaths, current_exe: Option<&Path>) {
    let runtime_root = runtime_root(paths);
    let current_dir = current_exe.and_then(Path::parent);
    let Ok(now) = current_unix_seconds() else {
        return;
    };
    let _ = maybe_run_periodic_gc_at(
        &runtime_root,
        current_dir,
        now,
        GC_INTERVAL_SECONDS,
        STALE_RUNTIME_SECONDS,
    );
}

fn relocation_requested() -> bool {
    cfg!(windows) || truthy_env(FORCE_RELOCATION_ENV_VAR)
}

fn relocation_guard_active() -> bool {
    std::env::var_os(RELOCATED_EXE_ENV_VAR).is_some()
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            let value = value.trim();
            !(value.is_empty()
                || value == "0"
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
                || value.eq_ignore_ascii_case("off"))
        })
        .unwrap_or(false)
}

fn ensure_relocated_exe(paths: &SoldrPaths, current_exe: &Path) -> Result<PathBuf, SoldrError> {
    let runtime_root = runtime_root(paths);
    fs::create_dir_all(&runtime_root)?;
    let _lock = lock_runtime_root(&runtime_root)?;

    let identity = exe_identity(current_exe)?;
    let dest_dir = runtime_root.join(&identity.dir_name);
    fs::create_dir_all(&dest_dir)?;

    let file_name = current_exe.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine current executable filename from {}",
            current_exe.display()
        ))
    })?;
    let dest = dest_dir.join(file_name);

    if exe_hash_matches(&dest, &identity.hash_hex) {
        touch_last_used(&dest_dir)?;
        return Ok(dest);
    }

    let temp = dest_dir.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    fs::copy(current_exe, &temp)?;
    let permissions = fs::metadata(current_exe)?.permissions();
    fs::set_permissions(&temp, permissions)?;

    if dest.exists() && !exe_hash_matches(&dest, &identity.hash_hex) {
        let _ = fs::remove_file(&dest);
    }

    match fs::rename(&temp, &dest) {
        Ok(()) => {}
        Err(_err) if exe_hash_matches(&dest, &identity.hash_hex) => {
            let _ = fs::remove_file(&temp);
        }
        Err(err) => {
            let _ = fs::remove_file(&temp);
            return Err(SoldrError::Io(err));
        }
    }

    touch_last_used(&dest_dir)?;
    Ok(dest)
}

fn runtime_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(RUNTIME_DIR).join(SELF_DIR)
}

fn lock_runtime_root(runtime_root: &Path) -> Result<File, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    let lock_path = runtime_root.join(LOCK_FILENAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

struct ExeIdentity {
    dir_name: String,
    hash_hex: String,
}

fn exe_identity(path: &Path) -> Result<ExeIdentity, SoldrError> {
    let hash_hex = hash_file(path)?;
    Ok(ExeIdentity {
        dir_name: format!("v{}-{hash_hex}", env!("CARGO_PKG_VERSION")),
        hash_hex,
    })
}

fn exe_hash_matches(path: &Path, expected_hash: &str) -> bool {
    path.is_file() && hash_file(path).is_ok_and(|actual| actual == expected_hash)
}

fn hash_file(path: &Path) -> Result<String, SoldrError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn touch_last_used(dir: &Path) -> Result<(), SoldrError> {
    fs::write(
        dir.join(LAST_USED_FILENAME),
        current_unix_seconds()?.to_string(),
    )?;
    Ok(())
}

fn maybe_run_periodic_gc_at(
    runtime_root: &Path,
    current_dir: Option<&Path>,
    now: u64,
    interval_seconds: u64,
    stale_seconds: u64,
) -> Result<Option<RuntimeGcSummary>, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    if !runtime_gc_due(runtime_root, now, interval_seconds) {
        return Ok(None);
    }

    let _lock = lock_runtime_root(runtime_root)?;
    if !runtime_gc_due(runtime_root, now, interval_seconds) {
        return Ok(None);
    }

    let summary = purge_stale_runtime_copies(runtime_root, current_dir, now, stale_seconds)?;
    fs::write(runtime_root.join(GC_MARKER_FILENAME), now.to_string())?;
    Ok(Some(summary))
}

fn runtime_gc_due(runtime_root: &Path, now: u64, interval_seconds: u64) -> bool {
    let marker = runtime_root.join(GC_MARKER_FILENAME);
    let Ok(raw) = fs::read_to_string(marker) else {
        return true;
    };
    let Ok(last_run) = raw.trim().parse::<u64>() else {
        return true;
    };
    now.saturating_sub(last_run) >= interval_seconds
}

fn purge_stale_runtime_copies(
    runtime_root: &Path,
    current_dir: Option<&Path>,
    now: u64,
    stale_seconds: u64,
) -> Result<RuntimeGcSummary, SoldrError> {
    let mut summary = RuntimeGcSummary::default();
    let cutoff = now.saturating_sub(stale_seconds);

    for entry in fs::read_dir(runtime_root)? {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let path = entry.path();
        summary.scanned_dirs += 1;
        if current_dir.is_some_and(|current| same_path(current, &path)) {
            summary.skipped_current_dirs += 1;
            continue;
        }

        let last_used = runtime_copy_last_used(&path).unwrap_or(now);
        if last_used > cutoff {
            summary.skipped_fresh_dirs += 1;
            continue;
        }

        match fs::remove_dir_all(&path) {
            Ok(()) => summary.removed_dirs += 1,
            Err(_) => summary.failed_dirs += 1,
        }
    }

    Ok(summary)
}

fn runtime_copy_last_used(path: &Path) -> Option<u64> {
    fs::read_to_string(path.join(LAST_USED_FILENAME))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .or_else(|| {
            fs::metadata(path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(system_time_to_unix_seconds)
        })
}

fn current_unix_seconds() -> Result<u64, SoldrError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|e| SoldrError::Other(format!("system clock is before unix epoch: {e}")))
}

fn system_time_to_unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => path.starts_with(root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::{OsStr, OsString},
        sync::Mutex,
    };
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
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

    fn seed_runtime_dir(root: &Path, name: &str, last_used: u64) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("create runtime dir");
        fs::write(dir.join(LAST_USED_FILENAME), last_used.to_string()).expect("write last-used");
        dir
    }

    #[test]
    fn relocation_guard_prevents_recursive_reexec_even_when_forced() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _force = EnvVarGuard::set(FORCE_RELOCATION_ENV_VAR, "1");
        let _marker = EnvVarGuard::set(RELOCATED_EXE_ENV_VAR, "1");

        let result =
            maybe_reexec_from_runtime(&["soldr".to_string(), "version".to_string()]).unwrap();

        assert_eq!(result, None);
    }

    #[test]
    fn ensure_relocated_exe_copies_to_hash_keyed_runtime_dir() {
        let temp = TempDir::new().expect("tempdir");
        let source = temp.path().join("soldr-test.exe");
        fs::write(&source, b"binary-content").expect("write source");
        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

        let relocated = ensure_relocated_exe(&paths, &source).expect("relocate exe");
        let expected_hash = hash_file(&source).expect("hash source");

        assert!(relocated.is_file());
        assert_eq!(
            fs::read(&relocated).expect("read relocated"),
            b"binary-content"
        );
        assert!(relocated
            .parent()
            .expect("relocated exe has parent")
            .file_name()
            .and_then(OsStr::to_str)
            .expect("dir is utf-8")
            .contains(&expected_hash));
        assert!(relocated
            .parent()
            .expect("relocated exe has parent")
            .join(LAST_USED_FILENAME)
            .is_file());

        let second = ensure_relocated_exe(&paths, &source).expect("reuse relocated exe");
        assert_eq!(second, relocated);
    }

    #[test]
    fn runtime_gc_removes_stale_dirs_and_skips_current_and_fresh() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("runtime").join("soldr-self");
        fs::create_dir_all(&root).expect("create runtime root");
        let stale = seed_runtime_dir(&root, "stale", 10);
        let fresh = seed_runtime_dir(&root, "fresh", 90);
        let current = seed_runtime_dir(&root, "current", 10);

        let summary =
            purge_stale_runtime_copies(&root, Some(&current), 100, 50).expect("runtime gc");

        assert_eq!(summary.scanned_dirs, 3);
        assert_eq!(summary.removed_dirs, 1);
        assert_eq!(summary.skipped_current_dirs, 1);
        assert_eq!(summary.skipped_fresh_dirs, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(current.exists());
    }

    #[test]
    fn periodic_runtime_gc_respects_marker_interval() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("runtime").join("soldr-self");
        fs::create_dir_all(&root).expect("create runtime root");
        let stale = seed_runtime_dir(&root, "stale", 10);
        fs::write(root.join(GC_MARKER_FILENAME), "95").expect("write gc marker");

        let summary = maybe_run_periodic_gc_at(&root, None, 100, 10, 50).expect("periodic gc");

        assert!(summary.is_none());
        assert!(stale.exists());
    }

    #[test]
    fn periodic_runtime_gc_deletes_stale_dirs_when_due() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("runtime").join("soldr-self");
        fs::create_dir_all(&root).expect("create runtime root");
        let stale = seed_runtime_dir(&root, "stale", 10);
        fs::write(root.join(GC_MARKER_FILENAME), "80").expect("write gc marker");

        let summary = maybe_run_periodic_gc_at(&root, None, 100, 10, 50)
            .expect("periodic gc")
            .expect("gc should run");

        assert_eq!(summary.removed_dirs, 1);
        assert!(!stale.exists());
        assert_eq!(
            fs::read_to_string(root.join(GC_MARKER_FILENAME)).expect("read marker"),
            "100"
        );
    }
}
