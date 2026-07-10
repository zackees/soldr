use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const RELOCATED_EXE_ENV_VAR: &str = "SOLDR_RELOCATED_EXE";
pub const ORIGINAL_EXE_ENV_VAR: &str = "SOLDR_ORIGINAL_EXE";
pub(crate) const FORCE_RELOCATION_ENV_VAR: &str = "SOLDR_TEST_SELF_RELOCATE_FORCE";

const RUNTIME_DIR: &str = "runtime";
const SELF_DIR: &str = "soldr-self";
/// Sibling of `SELF_DIR` used by the soldr-daemon trampoline. Same
/// copy / hash / GC machinery, different sub-tree so a stale daemon
/// runtime can be reaped without touching the soldr-self copies.
const DAEMON_DIR: &str = "soldr-daemon";
const LOCK_FILENAME: &str = ".lock";
const GC_MARKER_FILENAME: &str = ".last-gc";
const LAST_USED_FILENAME: &str = "last-used";
const GC_INTERVAL_SECONDS: u64 = 24 * 60 * 60;
/// Non-current version-residue (relocated soldr / soldr-daemon binary
/// copies keyed by `v{VERSION}-{hash}`) is collected after 48h of
/// non-use (soldr#1495 Workstream C). These are cheap to re-materialize
/// (a single file copy on next use), so a tight window keeps
/// `~/.soldr/runtime/` from accreting stale toolchain versions without
/// costing anything on the rare re-use of an older binary. The current
/// version's dir is always exempt (see `purge_stale_runtime_copies`).
const STALE_RUNTIME_SECONDS: u64 = 48 * 60 * 60;

#[derive(Debug, Default, PartialEq, Eq)]
struct RuntimeGcSummary {
    scanned_dirs: usize,
    removed_dirs: usize,
    skipped_current_dirs: usize,
    skipped_fresh_dirs: usize,
    /// Dirs that had no `last-used` ledger stamp and were self-healed
    /// with a fresh stamp instead of being aged out (soldr#1495): the
    /// GC is strictly ledger-based, never mtime-based, so an unstamped
    /// dir is treated as freshly-used this round rather than trusted to
    /// a filesystem mtime (which lies after archive restores).
    stamped_dirs: usize,
    failed_dirs: usize,
}

pub fn maybe_reexec_from_runtime(raw_args: &[String]) -> Result<Option<i32>, SoldrError> {
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

    let relocated_exe = ensure_relocated_exe_in(&runtime_root, &current_exe)?;
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
    run_periodic_gc_in(&runtime_root(paths), current_exe);
}

/// Copy `daemon_src` into `~/.soldr/runtime/soldr-daemon/<hash>/` and
/// return the relocated path. Reuses the soldr-self relocation
/// machinery so the daemon running in long-lived processes is
/// decoupled from the source path (worktree, cargo target/, package
/// installer) — uninstall/upgrade/rm of the source no longer needs to
/// wait for the daemon to exit. Returns `daemon_src` unchanged if the
/// source already lives under the daemon-runtime root.
pub fn ensure_daemon_relocated(
    paths: &SoldrPaths,
    daemon_src: &Path,
) -> Result<PathBuf, SoldrError> {
    // soldr#1300: a maturin-repaired wheel binary loads bundled shared
    // libraries through a path RELATIVE TO THE BINARY'S OWN LOCATION
    // (`@loader_path/../<pkg>.dylibs/...` on macOS). Copying just the
    // executable into the runtime dir strands that reference and dyld
    // kills the daemon at exec — before main(), before any logging —
    // so the wrapper saw NotRunning for the full retry budget on every
    // compile. Run the daemon in place instead; on Unix an upgrade /
    // uninstall can unlink the running binary without waiting for the
    // daemon to exit, so the relocation's motivation doesn't apply.
    if exe_depends_on_bundled_wheel_libs(daemon_src) {
        return Ok(daemon_src.to_path_buf());
    }
    let runtime_root = daemon_runtime_root(paths);
    fs::create_dir_all(&runtime_root)?;
    if path_is_under(daemon_src, &runtime_root) {
        return Ok(daemon_src.to_path_buf());
    }
    ensure_relocated_exe_in(&runtime_root, daemon_src)
}

/// Detect the maturin "repaired wheel" layout (soldr#1300).
///
/// From maturin 1.13.2 the macOS `bin`-bindings wheels are repaired
/// delocate-style: the real executables live under
/// `<platlib>/<pkg>.scripts/`, bundled external dylibs under the
/// sibling `<platlib>/<pkg>.dylibs/`, and every executable's
/// `LC_LOAD_DYLIB` entry for a bundled lib is rewritten to
/// `@loader_path/../<pkg>.dylibs/<lib>` (soldr 0.7.98's macOS wheel
/// ships `liblzma` this way). Such a binary only runs from its
/// original directory — copied anywhere else, dyld aborts it at exec
/// with "Library not loaded".
///
/// The check is pure path logic (parent dir named `<pkg>.scripts`
/// with a sibling `<pkg>.dylibs` / `<pkg>.libs` directory) so it can
/// be exercised on every platform. `<pkg>.libs` is auditwheel's
/// equivalent bundle dir on Linux, covered pre-emptively — today's
/// Linux wheels ship unrepaired binaries.
pub(crate) fn exe_depends_on_bundled_wheel_libs(exe: &Path) -> bool {
    let Some(parent) = exe.parent() else {
        return false;
    };
    let Some(dir_name) = parent.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(pkg) = dir_name.strip_suffix(".scripts") else {
        return false;
    };
    if pkg.is_empty() {
        return false;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    ["dylibs", "libs"]
        .iter()
        .any(|kind| grandparent.join(format!("{pkg}.{kind}")).is_dir())
}

/// Periodic GC sweep for the daemon-runtime sub-tree. Same cadence and
/// stale threshold as the soldr-self GC so a long-lived workspace
/// can't grow unbounded copies.
pub fn run_periodic_daemon_runtime_gc(paths: &SoldrPaths, current_exe: Option<&Path>) {
    run_periodic_gc_in(&daemon_runtime_root(paths), current_exe);
}

fn run_periodic_gc_in(runtime_root: &Path, current_exe: Option<&Path>) {
    let current_dir = current_exe.and_then(Path::parent);
    let Ok(now) = current_unix_seconds() else {
        return;
    };
    let _ = maybe_run_periodic_gc_at(
        runtime_root,
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

fn ensure_relocated_exe_in(runtime_root: &Path, current_exe: &Path) -> Result<PathBuf, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    let _lock = lock_runtime_root(runtime_root)?;

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

pub fn daemon_runtime_root(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(RUNTIME_DIR).join(DAEMON_DIR)
}

fn lock_runtime_root(runtime_root: &Path) -> Result<File, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    let lock_path = runtime_root.join(LOCK_FILENAME);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
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

        let Some(last_used) = runtime_copy_last_used(&path) else {
            // No ledger stamp. The GC is strictly ledger-based — never
            // trust the directory mtime, which lies after an archive
            // restore (an ancient saved mtime would age out a
            // freshly-rehydrated dir mid-build; soldr#1495 GHA safety).
            // Self-heal by stamping `now` and treating the dir as fresh
            // this round; it becomes eligible 48h from now if it stays
            // unused. Guarantees stampless dirs neither leak forever nor
            // get mtime-collected.
            let _ = fs::write(path.join(LAST_USED_FILENAME), now.to_string());
            summary.stamped_dirs += 1;
            continue;
        };
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

/// Read the `last-used` ledger stamp for a runtime copy. Returns `None`
/// when the stamp is absent or unparseable — deliberately with **no
/// mtime fallback**: filesystem mtimes survive archive save/restore and
/// would let a stale saved timestamp age out a freshly-materialized dir
/// (soldr#1495 Workstream C). Callers treat `None` as "unknown → keep
/// and self-heal", never as "old".
fn runtime_copy_last_used(path: &Path) -> Option<u64> {
    fs::read_to_string(path.join(LAST_USED_FILENAME))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

fn current_unix_seconds() -> Result<u64, SoldrError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|e| SoldrError::Other(format!("system clock is before unix epoch: {e}")))
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

        let relocated =
            ensure_relocated_exe_in(&runtime_root(&paths), &source).expect("relocate exe");
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

        let second =
            ensure_relocated_exe_in(&runtime_root(&paths), &source).expect("reuse relocated exe");
        assert_eq!(second, relocated);
    }

    #[test]
    fn ensure_daemon_relocated_copies_into_daemon_subtree() {
        let temp = TempDir::new().expect("tempdir");
        let source = temp.path().join("soldr-daemon.exe");
        fs::write(&source, b"daemon-bin").expect("write daemon");
        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

        let relocated = ensure_daemon_relocated(&paths, &source).expect("relocate daemon");
        assert!(relocated.is_file());
        assert_eq!(fs::read(&relocated).expect("read relocated"), b"daemon-bin");
        // Sub-tree must be the daemon root, NOT soldr-self.
        let daemon_root = daemon_runtime_root(&paths);
        assert!(
            relocated.starts_with(&daemon_root),
            "relocated path {} not under daemon root {}",
            relocated.display(),
            daemon_root.display(),
        );
        assert!(!relocated.starts_with(runtime_root(&paths)));

        // Calling again with a source already under the daemon root
        // is a no-op (returns the same path).
        let reused = ensure_daemon_relocated(&paths, &relocated).expect("noop relocation");
        assert_eq!(reused, relocated);
    }

    // soldr#1300 — the maturin-repaired macOS wheel layout: binaries
    // under `<platlib>/soldr.scripts/` load bundled dylibs via
    // `@loader_path/../soldr.dylibs/`. Relocating the daemon out of
    // that directory strands the reference and dyld kills it at exec,
    // so `ensure_daemon_relocated` must run it in place.
    crate::timed_test!(daemon_in_repaired_wheel_layout_is_not_relocated, {
        let temp = TempDir::new().expect("tempdir");
        let platlib = temp.path().join("site-packages");
        let scripts = platlib.join("soldr.scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::create_dir_all(platlib.join("soldr.dylibs")).expect("dylibs dir");
        let daemon = scripts.join("soldr-daemon");
        fs::write(&daemon, b"daemon-bin").expect("write daemon");

        assert!(exe_depends_on_bundled_wheel_libs(&daemon));

        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
        let resolved = ensure_daemon_relocated(&paths, &daemon).expect("resolve daemon");
        assert_eq!(
            resolved, daemon,
            "repaired-wheel daemon must run in place, not from the runtime copy"
        );
        assert!(
            !daemon_runtime_root(&paths).exists()
                || fs::read_dir(daemon_runtime_root(&paths))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "no runtime copy may be materialized for a repaired-wheel daemon"
        );
    });

    // The auditwheel spelling (`<pkg>.libs`) is covered pre-emptively.
    crate::timed_test!(repaired_wheel_detection_accepts_auditwheel_libs_dir, {
        let temp = TempDir::new().expect("tempdir");
        let scripts = temp.path().join("soldr.scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::create_dir_all(temp.path().join("soldr.libs")).expect("libs dir");
        let daemon = scripts.join("soldr-daemon");
        fs::write(&daemon, b"daemon-bin").expect("write daemon");

        assert!(exe_depends_on_bundled_wheel_libs(&daemon));
    });

    crate::timed_test!(plain_layouts_are_still_relocated, {
        let temp = TempDir::new().expect("tempdir");

        // `.scripts` dir WITHOUT a sibling bundle dir → not repaired
        // (nothing @loader_path-relative to strand): relocate normally.
        let scripts_only = temp.path().join("a").join("soldr.scripts");
        fs::create_dir_all(&scripts_only).expect("scripts dir");
        let daemon = scripts_only.join("soldr-daemon");
        fs::write(&daemon, b"daemon-bin").expect("write daemon");
        assert!(!exe_depends_on_bundled_wheel_libs(&daemon));

        // Bundle dir with a mismatched package prefix → unrelated.
        let other = temp.path().join("b").join("soldr.scripts");
        fs::create_dir_all(&other).expect("scripts dir");
        fs::create_dir_all(temp.path().join("b").join("otherpkg.dylibs")).expect("dylibs dir");
        let daemon_b = other.join("soldr-daemon");
        fs::write(&daemon_b, b"daemon-bin").expect("write daemon");
        assert!(!exe_depends_on_bundled_wheel_libs(&daemon_b));

        // Ordinary sibling layout (dev target/, venv bin/) → relocate.
        let plain = temp.path().join("bin").join("soldr-daemon");
        fs::create_dir_all(plain.parent().unwrap()).expect("bin dir");
        fs::write(&plain, b"daemon-bin").expect("write daemon");
        assert!(!exe_depends_on_bundled_wheel_libs(&plain));

        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
        let relocated = ensure_daemon_relocated(&paths, &plain).expect("relocate daemon");
        assert_ne!(relocated, plain, "plain layout must still relocate");
        assert!(relocated.starts_with(daemon_runtime_root(&paths)));
    });

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

    // soldr#1495 Workstream C: the version-residue window is 48h.
    #[test]
    fn stale_runtime_threshold_is_48_hours() {
        assert_eq!(STALE_RUNTIME_SECONDS, 48 * 60 * 60);
    }

    // soldr#1495 GHA safety: the GC is strictly ledger-based. A dir whose
    // filesystem mtime is ancient but whose `last-used` stamp is fresh
    // must be KEPT — mtimes lie after an archive restore, the ledger
    // stamp is authoritative.
    #[test]
    fn gc_trusts_ledger_stamp_not_mtime() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("runtime").join("soldr-self");
        fs::create_dir_all(&root).expect("create runtime root");
        // Fresh ledger stamp (95), well within a 50s window ending at now=100.
        let restored = seed_runtime_dir(&root, "restored", 95);
        // The dir itself is old on disk, but the ledger says fresh.
        let old_stamp = seed_runtime_dir(&root, "genuinely-old", 10);

        let summary = purge_stale_runtime_copies(&root, None, 100, 50).expect("runtime gc");

        assert!(
            restored.exists(),
            "a dir with a fresh ledger stamp must be kept regardless of mtime"
        );
        assert!(
            !old_stamp.exists(),
            "a dir with a stale ledger stamp must be collected"
        );
        assert_eq!(summary.removed_dirs, 1);
        assert_eq!(summary.skipped_fresh_dirs, 1);
    }

    // soldr#1495 GHA safety: a dir with NO ledger stamp is never
    // mtime-aged-out. It is self-healed with a fresh stamp and kept this
    // round (becomes eligible 48h later if it stays unused), so a
    // freshly-materialized-but-unstamped dir can never be swept mid-build.
    #[test]
    fn gc_self_heals_unstamped_dir_instead_of_mtime_aging() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("runtime").join("soldr-self");
        fs::create_dir_all(&root).expect("create runtime root");
        // Dir with content but no `last-used` file at all.
        let unstamped = root.join("no-stamp");
        fs::create_dir_all(&unstamped).expect("create dir");
        fs::write(unstamped.join("soldr.exe"), b"bin").expect("write bin");

        let summary = purge_stale_runtime_copies(&root, None, 1_000_000, 50).expect("runtime gc");

        assert!(unstamped.exists(), "unstamped dir must not be collected");
        assert_eq!(summary.stamped_dirs, 1);
        assert_eq!(summary.removed_dirs, 0);
        let stamp = fs::read_to_string(unstamped.join(LAST_USED_FILENAME))
            .expect("self-healed stamp written");
        assert_eq!(stamp, "1000000", "self-heal stamps `now`");
    }

    // soldr#1495 GHA safety, structural: the version-residue GC operates
    // only on the `runtime/` sub-tree under `~/.soldr/`. The compile
    // cache (`~/.soldr/cache/`, what CI rehydrates via `soldr load`)
    // lives in a sibling tree and is never walked, so a rehydrated warm
    // cache — however old its saved timestamps — is untouched.
    #[test]
    fn gc_never_touches_the_compile_cache_tree() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

        // A rehydrated compile cache with an ancient stamp/mtime.
        let cache_file = paths.cache.join("artifacts").join("blob.bin");
        fs::create_dir_all(cache_file.parent().unwrap()).expect("cache dir");
        fs::write(&cache_file, b"warm-artifact").expect("write cache");

        // A genuinely stale runtime copy that SHOULD be collected.
        let self_root = runtime_root(&paths);
        fs::create_dir_all(&self_root).expect("self root");
        let stale = seed_runtime_dir(&self_root, "v0.0.0-old", 10);

        // The self-runtime GC root and the cache root are disjoint sub-trees.
        assert!(
            !runtime_root(&paths).starts_with(&paths.cache)
                && !daemon_runtime_root(&paths).starts_with(&paths.cache),
            "runtime GC roots must live outside the compile cache tree"
        );

        purge_stale_runtime_copies(&self_root, None, 100, 50).expect("runtime gc");

        assert!(
            cache_file.exists(),
            "the compile cache must never be touched by the runtime GC"
        );
        assert!(!stale.exists(), "the stale runtime copy is still collected");
    }
}
