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
    ensure_daemon_relocated_with_progress(paths, daemon_src, |_, _, _| {})
}

/// [`ensure_daemon_relocated`] with observable byte progress for callers that
/// must keep a bounded route-acquisition connection alive while hashing or
/// copying a large daemon image.
pub fn ensure_daemon_relocated_with_progress(
    paths: &SoldrPaths,
    daemon_src: &Path,
    mut progress: impl FnMut(&'static str, u64, u64),
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
    ensure_relocated_exe_in_with_progress(&runtime_root, daemon_src, &mut progress)
}

/// Place a daemon at a route-specific executable path, retaining the repaired
/// wheel directory layout when the executable loads bundled libraries through
/// `@loader_path/..`.
///
/// Unlike [`ensure_daemon_relocated_with_progress`], this function never
/// returns a position-dependent wheel executable in place. Broker routes use
/// the returned executable path as their endpoint identity, so every route
/// must own a distinct, real path even when multiple roots share one wheel.
pub fn ensure_daemon_relocated_for_route_with_progress(
    paths: &SoldrPaths,
    daemon_src: &Path,
    mut progress: impl FnMut(&'static str, u64, u64),
) -> Result<PathBuf, SoldrError> {
    if !exe_depends_on_bundled_wheel_libs(daemon_src) {
        return ensure_daemon_relocated_with_progress(paths, daemon_src, progress);
    }

    let source_scripts = daemon_src.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine repaired-wheel scripts directory for {}",
            daemon_src.display()
        ))
    })?;
    let scripts_name = source_scripts.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine repaired-wheel scripts name for {}",
            daemon_src.display()
        ))
    })?;
    let package_root = source_scripts.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine repaired-wheel package root for {}",
            daemon_src.display()
        ))
    })?;
    let package = scripts_name
        .to_str()
        .and_then(|name| name.strip_suffix(".scripts"))
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "invalid repaired-wheel scripts directory {}",
                source_scripts.display()
            ))
        })?;

    let runtime_root = daemon_runtime_root(paths);
    fs::create_dir_all(&runtime_root)?;
    let _lock = lock_runtime_root(&runtime_root)?;
    let identity = exe_identity_with_progress(daemon_src, &mut progress)?;
    let dest_root = runtime_root.join(&identity.dir_name);
    let dest_scripts = dest_root.join(scripts_name);
    let file_name = daemon_src.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine daemon filename from {}",
            daemon_src.display()
        ))
    })?;
    let dest = dest_scripts.join(file_name);
    let complete = dest_root.join(".wheel-bundle-complete");
    if complete.is_file()
        && exe_hash_matches_with_progress(&dest, &identity.hash_hex, &mut progress)
    {
        touch_last_used(&dest_root)?;
        return Ok(dest);
    }

    fs::create_dir_all(&dest_scripts)?;
    copy_file_with_progress(daemon_src, &dest, &mut progress)?;
    fs::set_permissions(&dest, fs::metadata(daemon_src)?.permissions())?;
    for kind in ["dylibs", "libs"] {
        let source_bundle = package_root.join(format!("{package}.{kind}"));
        if source_bundle.is_dir() {
            copy_directory_tree(
                &source_bundle,
                &dest_root.join(format!("{package}.{kind}")),
                &mut progress,
            )?;
        }
    }
    File::create(&complete)?;
    touch_last_used(&dest_root)?;
    Ok(dest)
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
/// `pub` rather than `pub(crate)` because soldr-cli's shim writers need the
/// same guard (soldr#1856): a hardlink/copy of a repaired wheel binary into a
/// shim dir strands the same `@loader_path` reference the daemon path already
/// avoids.
pub fn exe_depends_on_bundled_wheel_libs(exe: &Path) -> bool {
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

/// True when `exe` is position-dependent: it names a shared library
/// relative to its own location, so copying or hardlinking it elsewhere
/// strands that reference and dyld aborts it at exec.
///
/// This asks the binary directly instead of inferring from directory
/// layout. [`exe_depends_on_bundled_wheel_libs`] is a *proxy* — it
/// recognises the one producer (maturin/delocate) whose output happens to
/// look that way. The proxy is both too narrow (any other repair tool, or
/// a hand-rolled `install_name_tool` run, is invisible to it) and too
/// fragile (it is opt-in per writer, so every new shim writer is a fresh
/// chance to forget the guard — which is exactly how soldr#1908 happened
/// after soldr#1856 was fixed).
///
/// Scans only the load-command region, never the whole file: `@loader_path`
/// can legitimately appear in a data section as an ordinary string, and
/// matching that would misclassify unrelated binaries.
///
/// Returns `false` for anything it cannot parse — a non-Mach-O, a
/// truncated file, an unreadable path. The caller's fallback is the
/// hardlink fast path, so a false negative preserves today's behaviour
/// while a false positive would needlessly slow every shim.
pub fn exe_has_loader_path_reference(exe: &Path) -> bool {
    let Ok(bytes) = fs::read(exe) else {
        return false;
    };
    macho_load_commands_mention_loader_path(&bytes)
}

/// `@loader_path` is the only Mach-O prefix that is relative to the
/// *image being loaded*. `@executable_path` resolves against the main
/// executable and `@rpath` against `LC_RPATH` entries, which is why
/// neither is checked here: moving the file does not by itself break them.
const LOADER_PATH_TOKEN: &[u8] = b"@loader_path";

fn macho_load_commands_mention_loader_path(bytes: &[u8]) -> bool {
    // Universal ("fat") binary: a big-endian table of slices, each a
    // complete Mach-O. Any slice needing the trampoline condemns the file,
    // since we cannot know which slice will be executed.
    const FAT_MAGIC: u32 = 0xcafe_babe;
    const FAT_MAGIC_64: u32 = 0xcafe_babf;
    if let Some(magic) = read_u32(bytes, 0, false) {
        if magic == FAT_MAGIC || magic == FAT_MAGIC_64 {
            let Some(nfat) = read_u32(bytes, 4, false) else {
                return false;
            };
            // fat_arch is cputype, cpusubtype, offset, size, align (5 x u32);
            // fat_arch_64 widens offset/size to u64 and adds a reserved
            // word. In both layouts `offset` starts at byte 8.
            let entry_size = if magic == FAT_MAGIC { 20 } else { 32 };
            const OFFSET_FIELD: usize = 8;
            // Cap the arch count so a corrupt header cannot spin.
            for i in 0..nfat.min(64) {
                let base = 8 + (i as usize) * entry_size;
                let off = if magic == FAT_MAGIC {
                    read_u32(bytes, base + OFFSET_FIELD, false).map(|v| v as usize)
                } else {
                    read_u64(bytes, base + OFFSET_FIELD, false).map(|v| v as usize)
                };
                let Some(off) = off else { continue };
                if let Some(slice) = bytes.get(off..) {
                    if thin_macho_mentions_loader_path(slice) {
                        return true;
                    }
                }
            }
            return false;
        }
    }
    thin_macho_mentions_loader_path(bytes)
}

fn thin_macho_mentions_loader_path(bytes: &[u8]) -> bool {
    const MH_MAGIC: u32 = 0xfeed_face; // 32-bit, host-endian
    const MH_CIGAM: u32 = 0xcefa_edfe; // 32-bit, byte-swapped
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const MH_CIGAM_64: u32 = 0xcffa_edfe;

    let Some(raw) = read_u32(bytes, 0, true) else {
        return false;
    };
    // `swapped` means the file's fields are big-endian relative to us.
    let (is_64, swapped) = match raw {
        MH_MAGIC_64 => (true, false),
        MH_CIGAM_64 => (true, true),
        MH_MAGIC => (false, false),
        MH_CIGAM => (false, true),
        _ => return false,
    };

    // mach_header: magic, cputype, cpusubtype, filetype, ncmds,
    // sizeofcmds, flags [, reserved on 64-bit].
    let Some(sizeofcmds) = read_u32(bytes, 20, !swapped) else {
        return false;
    };
    let header_len: usize = if is_64 { 32 } else { 28 };
    let end = header_len.saturating_add(sizeofcmds as usize);
    let Some(region) = bytes.get(header_len..end.min(bytes.len())) else {
        return false;
    };
    region
        .windows(LOADER_PATH_TOKEN.len())
        .any(|w| w == LOADER_PATH_TOKEN)
}

/// Read a `u32`, `little` selecting the interpretation. Returns `None`
/// rather than panicking on a short buffer so a truncated file is simply
/// "not position-dependent" instead of a crash in a shim writer.
fn read_u32(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    })
}

fn read_u64(bytes: &[u8], at: usize, little: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
    Some(if little {
        u64::from_le_bytes(raw)
    } else {
        u64::from_be_bytes(raw)
    })
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
    ensure_relocated_exe_in_with_progress(runtime_root, current_exe, &mut |_, _, _| {})
}

fn ensure_relocated_exe_in_with_progress(
    runtime_root: &Path,
    current_exe: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<PathBuf, SoldrError> {
    fs::create_dir_all(runtime_root)?;
    let _lock = lock_runtime_root(runtime_root)?;

    let identity = exe_identity_with_progress(current_exe, progress)?;
    let dest_dir = runtime_root.join(&identity.dir_name);
    fs::create_dir_all(&dest_dir)?;

    let file_name = current_exe.file_name().ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to determine current executable filename from {}",
            current_exe.display()
        ))
    })?;
    let dest = dest_dir.join(file_name);

    if exe_hash_matches_with_progress(&dest, &identity.hash_hex, progress) {
        touch_last_used(&dest_dir)?;
        return Ok(dest);
    }

    let temp = dest_dir.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    copy_file_with_progress(current_exe, &temp, progress)?;
    let permissions = fs::metadata(current_exe)?.permissions();
    fs::set_permissions(&temp, permissions)?;

    if dest.exists() && !exe_hash_matches_with_progress(&dest, &identity.hash_hex, progress) {
        let _ = fs::remove_file(&dest);
    }

    match fs::rename(&temp, &dest) {
        Ok(()) => {}
        Err(_err) if exe_hash_matches_with_progress(&dest, &identity.hash_hex, progress) => {
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

/// Set on the re-executed child so the hop can never happen twice.
///
/// This is the safety property, not an optimisation. If
/// [`daemon_should_reexec`]'s "am I already relocated?" answer were ever wrong
/// in the direction of "no", an unguarded hop would re-exec forever and no
/// daemon would ever start -- every build on the machine broken, and the
/// failure looks like a daemon that will not come up rather than like a bad
/// predicate. With a one-shot marker the worst case degrades to today's
/// behaviour: running from wherever we were launched.
pub const DAEMON_REEXEC_MARKER_ENV_VAR: &str = "SOLDR_INTERNAL_DAEMON_REEXECED";

/// Where a daemon launched from `current_exe` should re-exec itself from, if
/// anywhere.
///
/// soldr#1987: a daemon spawned from a directory that is later deleted -- a
/// `uv` or `pip` build materialises soldr into a temp dir and removes it --
/// keeps the root-ownership lock forever, and nothing outside the process can
/// clear it. `resolve_daemon_spawn_image` already relocates on the *spawner*
/// side, but that is advisory: it falls back to the original path on any error
/// (soldr#1998 made that audible), and a daemon started by any other route
/// never passes through it at all. Deciding here, inside the process that will
/// take the lock, is the only unconditional place.
///
/// Returns `None` when no hop is needed or possible:
/// * the marker is set -- we already hopped once;
/// * the image is already under the runtime root;
/// * the source is a maturin-repaired wheel, which must run in place
///   (soldr#1300) because relocating strands its bundled dylibs;
/// * relocation failed -- a daemon pinning the wrong directory still beats no
///   daemon, which is the same trade the spawner makes.
pub fn daemon_should_reexec(paths: &SoldrPaths, current_exe: &Path) -> Option<PathBuf> {
    if std::env::var_os(DAEMON_REEXEC_MARKER_ENV_VAR).is_some() {
        return None;
    }
    if exe_depends_on_bundled_wheel_libs(current_exe) {
        return None;
    }
    let runtime_root = daemon_runtime_root(paths);
    if path_is_under(current_exe, &runtime_root) {
        return None;
    }
    let relocated = ensure_daemon_relocated(paths, current_exe).ok()?;
    // Equal paths mean `ensure_daemon_relocated` declined to move it; hopping
    // to where we already are would be a no-op exec, and with a marker that
    // failed to stick it would be a loop.
    (relocated != current_exe).then_some(relocated)
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
    exe_identity_with_progress(path, &mut |_, _, _| {})
}

fn exe_identity_with_progress(
    path: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<ExeIdentity, SoldrError> {
    let hash_hex = hash_file_with_progress(path, "source-hash", progress)?;
    Ok(ExeIdentity {
        dir_name: relocation_dir_name(env!("CARGO_PKG_VERSION"), &hash_hex),
        hash_hex,
    })
}

/// soldr#1597 Phase 3: official (`release-auto.yml`-published) builds
/// relocate to a hash-free `v{VERSION}/` directory. Dev/manual builds
/// keep the hash-keyed `v{VERSION}-{hash}` name unconditionally — the
/// hash is the only thing that distinguishes two dev rebuilds sharing a
/// version but differing in content, and without it a rebuild could try
/// to overwrite a locked, running daemon binary on Windows. Broker v2
/// discovery is pointer-file-based (PID file + `.servicedef.v2`), never
/// directory-scanning, so this naming choice is invisible to it either
/// way — no broker changes are needed.
fn relocation_dir_name(version: &str, hash_hex: &str) -> String {
    if crate::build_provenance::is_official_build() {
        format!("v{version}")
    } else {
        format!("v{version}-{hash_hex}")
    }
}

fn exe_hash_matches(path: &Path, expected_hash: &str) -> bool {
    exe_hash_matches_with_progress(path, expected_hash, &mut |_, _, _| {})
}

fn exe_hash_matches_with_progress(
    path: &Path,
    expected_hash: &str,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> bool {
    path.is_file()
        && hash_file_with_progress(path, "placed-hash", progress)
            .is_ok_and(|actual| actual == expected_hash)
}

fn hash_file(path: &Path) -> Result<String, SoldrError> {
    hash_file_with_progress(path, "hash", &mut |_, _, _| {})
}

fn hash_file_with_progress(
    path: &Path,
    stage: &'static str,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<String, SoldrError> {
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut completed = 0_u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        completed = completed.saturating_add(read as u64);
        progress(stage, completed, total);
    }
    Ok(to_hex(&hasher.finalize()))
}

fn copy_file_with_progress(
    source: &Path,
    target: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<(), SoldrError> {
    use std::io::Write as _;

    let mut input = File::open(source)?;
    let total = input.metadata()?.len();
    let mut output = File::create(target)?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut completed = 0_u64;
    loop {
        let read = input.read(&mut buf)?;
        if read == 0 {
            break;
        }
        output.write_all(&buf[..read])?;
        completed = completed.saturating_add(read as u64);
        progress("copy", completed, total);
    }
    output.flush()?;
    Ok(())
}

fn copy_directory_tree(
    source: &Path,
    target: &Path,
    progress: &mut dyn FnMut(&'static str, u64, u64),
) -> Result<(), SoldrError> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory_tree(&source_path, &target_path, progress)?;
        } else {
            copy_file_with_progress(&source_path, &target_path, progress)?;
            fs::set_permissions(&target_path, fs::metadata(&source_path)?.permissions())?;
        }
    }
    fs::set_permissions(target, fs::metadata(source)?.permissions())?;
    Ok(())
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
    fn relocation_dir_name_is_hash_free_only_for_official_builds() {
        // soldr#1597 Phase 3: dev/manual builds keep the hash-keyed name
        // unconditionally (same-version-different-content safety); only
        // an official (release-auto.yml-stamped) build gets the
        // hash-free, version-rooted name.
        assert_eq!(relocation_dir_name("1.2.3", "deadbeef"), "v1.2.3-deadbeef");
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

    #[test]
    fn daemon_relocation_reports_real_byte_progress() {
        let temp = TempDir::new().expect("tempdir");
        let source = temp.path().join("soldr-daemon");
        let contents = vec![0x5a_u8; 2 * 1024 * 1024 + 17];
        fs::write(&source, &contents).expect("write daemon");
        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));
        let mut events = Vec::new();

        let relocated =
            ensure_daemon_relocated_with_progress(&paths, &source, |stage, completed, total| {
                events.push((stage, completed, total))
            })
            .expect("relocate with progress");

        assert_eq!(fs::read(relocated).expect("read relocated"), contents);
        for stage in ["source-hash", "copy"] {
            let stage_events: Vec<_> = events
                .iter()
                .filter(|(reported, _, _)| *reported == stage)
                .collect();
            assert!(stage_events.len() >= 2, "missing streaming {stage} events");
            assert_eq!(
                stage_events.last().map(|(_, done, _)| *done),
                Some(contents.len() as u64)
            );
            assert!(stage_events.iter().all(|(_, done, total)| done <= total));
        }
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

    crate::timed_test!(route_placement_preserves_repaired_wheel_layout, {
        let temp = TempDir::new().expect("tempdir");
        let platlib = temp.path().join("site-packages");
        let scripts = platlib.join("soldr.scripts");
        let libs = platlib.join("soldr.dylibs");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::create_dir_all(&libs).expect("dylibs dir");
        let daemon = scripts.join("soldr-daemon");
        fs::write(&daemon, b"daemon-bin").expect("write daemon");
        fs::write(libs.join("libexample.dylib"), b"library").expect("write library");

        let paths = SoldrPaths::with_root(temp.path().join("route-a"));
        let resolved =
            ensure_daemon_relocated_for_route_with_progress(&paths, &daemon, |_, _, _| {})
                .expect("place route daemon");

        assert_ne!(resolved, daemon);
        assert_eq!(
            resolved.parent().and_then(Path::file_name),
            Some(OsStr::new("soldr.scripts"))
        );
        let placed_root = resolved
            .parent()
            .and_then(Path::parent)
            .expect("placed bundle root");
        assert_eq!(
            fs::read(placed_root.join("soldr.dylibs/libexample.dylib"))
                .expect("read placed library"),
            b"library"
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

    /// Build a minimal 64-bit little-endian Mach-O whose load-command
    /// region contains `payload`. Enough to exercise the parser on every
    /// platform — the real question is header arithmetic, not linking.
    fn synthetic_macho_le64(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0xfeed_facfu32.to_le_bytes()); // magic
        out.extend_from_slice(&0u32.to_le_bytes()); // cputype
        out.extend_from_slice(&0u32.to_le_bytes()); // cpusubtype
        out.extend_from_slice(&2u32.to_le_bytes()); // filetype
        out.extend_from_slice(&1u32.to_le_bytes()); // ncmds
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // sizeofcmds
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved
        out.extend_from_slice(payload);
        out
    }

    // #1908: ask the binary, not the directory it happens to sit in.
    crate::timed_test!(loader_path_reference_is_detected_in_load_commands, {
        let temp = TempDir::new().expect("tempdir");
        let exe = temp.path().join("repaired");
        fs::write(
            &exe,
            synthetic_macho_le64(b"\x0c\x00\x00\x00@loader_path/../soldr.dylibs/liblzma.dylib\x00"),
        )
        .expect("write");
        assert!(
            exe_has_loader_path_reference(&exe),
            "a load command naming @loader_path must be detected"
        );
    });

    // The scan must stay inside the load commands. `@loader_path` is a
    // perfectly ordinary string for a binary to carry in its data --
    // soldr's own source mentions it -- and matching that would condemn
    // unrelated binaries to the slow trampoline path.
    crate::timed_test!(loader_path_in_the_body_is_not_a_reference, {
        let temp = TempDir::new().expect("tempdir");
        let exe = temp.path().join("innocent");
        let mut bytes = synthetic_macho_le64(b"\x0c\x00\x00\x00/usr/lib/libSystem.dylib\x00");
        bytes.extend_from_slice(b"... @loader_path mentioned in a data section ...");
        fs::write(&exe, bytes).expect("write");
        assert!(
            !exe_has_loader_path_reference(&exe),
            "a data-section mention must not count"
        );
    });

    // Unparseable input must fall back to today's behaviour (hardlink),
    // never panic: these run inside shim writers on every platform.
    crate::timed_test!(non_macho_inputs_are_not_position_dependent, {
        let temp = TempDir::new().expect("tempdir");

        let script = temp.path().join("script");
        fs::write(&script, b"#!/bin/sh\nexec soldr cargo \"$@\"\n").expect("write");
        assert!(!exe_has_loader_path_reference(&script));

        let elf = temp.path().join("elf");
        fs::write(&elf, b"\x7fELF\x02\x01\x01\x00 @loader_path").expect("write");
        assert!(!exe_has_loader_path_reference(&elf));

        let empty = temp.path().join("empty");
        fs::write(&empty, b"").expect("write");
        assert!(!exe_has_loader_path_reference(&empty));

        // Truncated: header claims more load commands than exist.
        let truncated = temp.path().join("truncated");
        let mut bytes = synthetic_macho_le64(b"@loader_path/x");
        bytes.truncate(20);
        fs::write(&truncated, bytes).expect("write");
        assert!(!exe_has_loader_path_reference(&truncated));

        assert!(!exe_has_loader_path_reference(&temp.path().join("nope")));
    });

    // A universal binary hides its slices behind an offset table, so the
    // parser has to follow them; a naive whole-file scan would pass this
    // test for the wrong reason, hence the offset padding.
    crate::timed_test!(fat_binary_slices_are_followed, {
        let temp = TempDir::new().expect("tempdir");
        let exe = temp.path().join("universal");

        let slice = synthetic_macho_le64(b"\x0c\x00\x00\x00@loader_path/../soldr.dylibs/x\x00");
        let slice_offset: u32 = 4096;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xcafe_babeu32.to_be_bytes()); // FAT_MAGIC
        bytes.extend_from_slice(&1u32.to_be_bytes()); // nfat_arch
        bytes.extend_from_slice(&0u32.to_be_bytes()); // cputype
        bytes.extend_from_slice(&0u32.to_be_bytes()); // cpusubtype
        bytes.extend_from_slice(&slice_offset.to_be_bytes()); // offset
        bytes.extend_from_slice(&(slice.len() as u32).to_be_bytes()); // size
        bytes.extend_from_slice(&0u32.to_be_bytes()); // align
        bytes.resize(slice_offset as usize, 0);
        bytes.extend_from_slice(&slice);
        fs::write(&exe, bytes).expect("write");

        assert!(
            exe_has_loader_path_reference(&exe),
            "fat slices must be followed through the offset table"
        );
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
    fn runtime_gc_treats_hash_free_dirs_identically_to_hash_keyed_dirs() {
        // soldr#1597 Phase 4: purge_stale_runtime_copies operates on any
        // directory under the runtime root by ledger stamp alone, with no
        // dependence on the naming scheme. A hash-free `v{VERSION}/` dir
        // (official builds, Phase 3) must GC exactly like a hash-keyed
        // `v{VERSION}-{hash}/` dir (dev builds) — no code change needed
        // here, this locks that in as a regression test.
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("runtime").join("soldr-daemon");
        fs::create_dir_all(&root).expect("create runtime root");
        let stale_hash_keyed = seed_runtime_dir(&root, "v0.8.5-deadbeef", 10);
        let stale_hash_free = seed_runtime_dir(&root, "v0.8.5", 10);
        let fresh_hash_free = seed_runtime_dir(&root, "v0.9.0", 90);

        let summary = purge_stale_runtime_copies(&root, None, 100, 50).expect("runtime gc");

        assert_eq!(summary.scanned_dirs, 3);
        assert_eq!(summary.removed_dirs, 2);
        assert_eq!(summary.skipped_fresh_dirs, 1);
        assert!(!stale_hash_keyed.exists());
        assert!(!stale_hash_free.exists());
        assert!(fresh_hash_free.exists());
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

#[cfg(test)]
mod reexec_hop_tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Shares the module's env barrier: these read/write the marker variable
    // that the tests above also disturb.
    static HOP_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_daemon(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let exe = dir.join(name);
        std::fs::write(&exe, b"daemon-bin").expect("write");
        exe
    }

    // THE safety property. If the "already relocated?" answer were ever wrong
    // in the direction of "no", an unguarded hop would re-exec forever and no
    // daemon would ever start. The marker makes that impossible regardless of
    // how the predicate behaves, so the worst case is the status quo -- running
    // from wherever we were launched.
    crate::timed_test!(the_marker_stops_a_second_hop_whatever_the_predicate_says, {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        // Deliberately outside the runtime root: without the marker this is
        // exactly the case that WOULD hop.
        let exe = fake_daemon(&temp.path().join("elsewhere"), "soldr-daemon.exe");
        assert!(
            daemon_should_reexec(&paths, &exe).is_some(),
            "precondition: this image should hop when unmarked"
        );

        std::env::set_var(DAEMON_REEXEC_MARKER_ENV_VAR, "1");
        let decision = daemon_should_reexec(&paths, &exe);
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);

        assert!(
            decision.is_none(),
            "a marked process must never hop again -- that is the loop guard"
        );
    });

    // An image already under the runtime root is where we want it; hopping
    // would be a pointless exec of ourselves.
    crate::timed_test!(an_already_relocated_daemon_does_not_hop, {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let exe = fake_daemon(&daemon_runtime_root(&paths).join("abc"), "soldr-daemon.exe");
        assert!(daemon_should_reexec(&paths, &exe).is_none());
    });

    // soldr#1300: a maturin-repaired wheel resolves bundled dylibs relative to
    // its own location, so relocating strands them and the daemon dies before
    // main(). It must run in place even though it is outside the runtime root.
    crate::timed_test!(a_repaired_wheel_layout_runs_in_place, {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let platlib = temp.path().join("platlib");
        std::fs::create_dir_all(platlib.join("soldr.dylibs")).expect("dylibs");
        let exe = fake_daemon(&platlib.join("soldr.scripts"), "soldr-daemon");
        assert!(
            daemon_should_reexec(&paths, &exe).is_none(),
            "relocating a repaired wheel strands its bundled dylibs (soldr#1300)"
        );
    });

    // The #1987 case itself: a temp-dir image that is free to move.
    crate::timed_test!(a_temp_dir_daemon_hops_into_the_runtime_root, {
        let _lock = HOP_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DAEMON_REEXEC_MARKER_ENV_VAR);
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let exe = fake_daemon(
            &temp.path().join("uv-build-tmp").join("Scripts"),
            "soldr-daemon.exe",
        );

        let target = daemon_should_reexec(&paths, &exe).expect("should hop");
        assert!(
            path_is_under(&target, &daemon_runtime_root(&paths)),
            "must land under the runtime root, got {}",
            target.display()
        );
        assert_ne!(
            target, exe,
            "hopping to the same path would be a no-op exec"
        );
    });
}
