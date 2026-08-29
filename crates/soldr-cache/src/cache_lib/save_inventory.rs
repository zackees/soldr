/// Stream-hash a file with BLAKE3. Returns the full 32-byte hash.
fn hash_file(path: &Path) -> Result<[u8; 32]> {
    zccache::hash::hash_file(path)
        .map(|hash| *hash.as_bytes())
        .map_err(|e| io(path, e))
}

/// Resolve the absolute path(s) that count as "the Cargo target directory"
/// for `workspace` (#1547). Path-name matching alone (excluding every
/// directory literally named `target` anywhere in the tree) can hide
/// legitimate tracked source such as `src/target/mod.rs`, so the walker
/// only excludes a `target/` entry when its full path matches one of
/// these candidates.
///
/// Candidates, matching Cargo's own resolution order (most specific
/// first) — we do not attempt to parse `.cargo/config.toml`'s
/// `build.target-dir` key here; an override there falls through to
/// the conservative default below, which only means the real output
/// dir gets hashed too (safe: extra work, never a missed input):
/// * `$CARGO_TARGET_DIR` (if absolute),
/// * `$CARGO_BUILD_TARGET_DIR` (if absolute),
/// * `<workspace>/target` (Cargo's default).
fn workspace_target_dir_candidates(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(3);
    for var in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(dir) = std::env::var_os(var) {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                out.push(path);
            }
        }
    }
    out.push(workspace.join("target"));
    out
}

pub fn walk_parallelism(threads: Option<usize>) -> jwalk::Parallelism {
    // Never `RayonDefaultPool` (jwalk's default), for two compounding
    // reasons (soldr#2760).
    //
    // 1. It spawns via `rayon::spawn`, which targets the *ambient* pool,
    //    not the global one, whenever it is called from inside a pool.
    //    `save()` calls these walks from `pool.install(|| rayon::join(..))`,
    //    so the walk lands on the very pool whose threads the join is
    //    already occupying.
    // 2. It carries a 1-second `busy_timeout` and *aborts the walk* when
    //    the pool cannot serve it in time — surfacing as
    //    `SaveLoadError::Walk("rayon thread-pool too busy or dependency
    //    loop detected")`, i.e. a hard save failure rather than a slow save.
    //
    // Together those make a correct save fail purely because the machine was
    // busy, which is what the emulated aarch64-msvc lane kept hitting.
    // `RayonNewPool` reports `timeout() == None`, so the abort cannot fire,
    // and the traversal no longer contends with the hashing work.
    //
    // `RayonNewPool(0)` means "a new pool, rayon's default size" — the cost
    // is one pool per walk, against a walk that is already doing syscalls
    // per directory.
    jwalk::Parallelism::RayonNewPool(threads.filter(|n| *n > 0).unwrap_or(0))
}

/// Walk a workspace and return every regular file's repo-relative POSIX
/// path. We deliberately do NOT shell out to `git ls-files` — soldr's
/// users include sandboxed CI jobs and local-dev runs that don't always
/// have git on PATH at the moment this is invoked. `.git/` and
/// `node_modules/` are excluded by name at any depth — those basenames
/// are never legitimate tracked source. The build-output `target/`
/// directory is excluded by *resolved path*, not by name (#1547): a
/// source directory that happens to be named `target` anywhere other
/// than the actual Cargo target dir (e.g. `src/target/mod.rs`) is real
/// source and must be hashed. See [`workspace_target_dir_candidates`].
///
/// Uses jwalk for a parallel walk; on a 1000-file workspace this is
/// ~4x faster than walkdir at the directory-traversal level. The walk
/// itself is the cheap part — caller still has to hash each file.
///
/// Every walk in this module runs on a pool of its own — see
/// [`walk_parallelism`] for why the default is not usable here.
fn walk_workspace_files(workspace: &Path, threads: Option<usize>) -> Result<Vec<PathBuf>> {
    let target_dirs = workspace_target_dir_candidates(workspace);
    let walker = jwalk::WalkDir::new(workspace)
        .follow_links(false)
        .skip_hidden(false) // we want .cargo, .rustfmt.toml, etc.
        .process_read_dir(move |_depth, dir_path, _read_dir_state, children| {
            children.retain(|res| match res {
                Ok(entry) => {
                    let name = entry.file_name.to_string_lossy();
                    if entry.depth > 0 && (name == ".git" || name == "node_modules") {
                        return false;
                    }
                    if entry.depth > 0 && name == "target" {
                        let candidate = dir_path.join(&entry.file_name);
                        if target_dirs.iter().any(|t| t == &candidate) {
                            return false;
                        }
                    }
                    true
                }
                Err(_) => true,
            });
        });
    let walker = walker.parallelism(walk_parallelism(threads));
    let mut out = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: workspace.to_path_buf(),
            message: err.to_string(),
        })?;
        let file_type = entry.file_type();
        let is_file = file_type.is_file();
        // #1548: symlinked SOURCE files are surfaced via their target
        // content when — and only when — the link target lexically stays
        // inside the workspace and resolves to a regular file. Downstream
        // (hash + mtime snapshot at save, replay at load) uses
        // link-following `fs::metadata` / `hash_file`, so the entry
        // naturally carries the target's content hash and mtime.
        // Absolute, escaping, broken, and non-UTF-8 targets stay
        // conservatively OMITTED (the pre-#1548 behavior for all
        // symlinks): a missing manifest entry can only mean "no mtime
        // replayed", i.e. Cargo rebuilds — never an underbuild.
        let is_surfaced_symlink = !is_file
            && file_type.is_symlink()
            && workspace_symlink_is_surfaced(workspace, &entry.path());
        if !is_file && !is_surfaced_symlink {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(workspace)
            .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
        out.push(rel.to_path_buf());
    }
    out.sort();
    Ok(out)
}

/// Build a conservative Cargo-input inventory from compiler dep-info files.
/// A missing, malformed, stale, or build-script-sensitive inventory returns
/// `None`, so callers retain the broad source walk and cannot underbuild.
fn cargo_input_inventory(
    workspace: &Path,
    target_dir: &Path,
    threads: Option<usize>,
) -> Result<Option<Vec<PathBuf>>> {
    if !target_dir.is_dir() {
        return Ok(None);
    }
    let mut dep_info_files = Vec::new();
    let mut build_script_metadata = false;
    let mut workspace_dep_count = 0usize;
    let walker = jwalk::WalkDir::new(target_dir)
        .follow_links(false)
        .skip_hidden(false);
    let walker = walker.parallelism(walk_parallelism(threads));
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: target_dir.to_path_buf(),
            message: err.to_string(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "d") {
            dep_info_files.push(path.clone());
            if path.strip_prefix(target_dir).ok().is_some_and(|relative| {
                relative
                    .components()
                    .any(|component| component.as_os_str() == "build")
            }) {
                build_script_metadata = true;
            }
        }
        if path.components().any(|c| c.as_os_str() == ".fingerprint")
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("run-build-script"))
        {
            build_script_metadata = true;
        }
    }
    if dep_info_files.is_empty() || build_script_metadata {
        return Ok(None);
    }

    let mut inventory = BTreeSet::new();
    for dep_info in dep_info_files {
        let text = match std::fs::read_to_string(&dep_info) {
            Ok(text) => text,
            Err(_) => return Ok(None),
        };
        let Some((_, dependencies)) = text.split_once(": ") else {
            return Ok(None);
        };
        for token in makefile_tokens(dependencies) {
            let path = PathBuf::from(token);
            let Ok(relative) = path.strip_prefix(workspace) else {
                continue;
            };
            if relative.as_os_str().is_empty() || !path.is_file() {
                return Ok(None);
            }
            workspace_dep_count += 1;
            inventory.insert(relative.to_path_buf());
        }
    }

    // Cargo manifests and toolchain/config files are inputs even when they
    // are absent from rustc dep-info. Walking metadata is cheap; hashing only
    // this set is the optimization target.
    let target_dirs = workspace_target_dir_candidates(workspace);
    let metadata_walker = jwalk::WalkDir::new(workspace)
        .follow_links(false)
        .skip_hidden(false)
        .process_read_dir(move |_depth, dir_path, _state, children| {
            children.retain(|res| match res {
                Ok(entry) => {
                    let name = entry.file_name.to_string_lossy();
                    if entry.depth > 0 && (name == ".git" || name == "node_modules") {
                        return false;
                    }
                    if entry.depth > 0 && name == "target" {
                        return !target_dirs
                            .iter()
                            .any(|t| t == &dir_path.join(&entry.file_name));
                    }
                    true
                }
                Err(_) => true,
            });
        });
    // soldr#2760: this walk had no `.parallelism(..)` at all, so it stayed on
    // jwalk's aborting default even when the caller passed an explicit
    // `--threads`, which the sibling walks honoured.
    let metadata_walker = metadata_walker.parallelism(walk_parallelism(threads));
    for entry in metadata_walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: workspace.to_path_buf(),
            message: err.to_string(),
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name.to_string_lossy();
        if name == "Cargo.toml"
            || name == "Cargo.lock"
            || name == "build.rs"
            || name.starts_with("rust-toolchain")
            || name == "config"
            || name.starts_with("config.")
        {
            inventory.insert(
                entry
                    .path()
                    .strip_prefix(workspace)
                    .map_err(|_| SaveLoadError::BadArchivePath(entry.path().display().to_string()))?
                    .to_path_buf(),
            );
        }
    }
    if inventory.is_empty() || workspace_dep_count == 0 {
        return Ok(None);
    }
    Ok(Some(inventory.into_iter().collect()))
}

fn makefile_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn workspace_files_for_save(workspace: &Path, threads: Option<usize>) -> Result<Vec<PathBuf>> {
    for target_dir in workspace_target_dir_candidates(workspace) {
        if let Some(files) = cargo_input_inventory(workspace, &target_dir, threads)? {
            return Ok(files);
        }
    }
    walk_workspace_files(workspace, threads)
}

/// True when a workspace symlink should appear in the source-file snapshot
/// (#1548): relative target, lexically contained in the workspace root, and
/// resolving to an existing regular file.
fn workspace_symlink_is_surfaced(workspace: &Path, abs: &Path) -> bool {
    let Ok(rel) = abs.strip_prefix(workspace) else {
        return false;
    };
    let Ok(raw) = std::fs::read_link(abs) else {
        return false;
    };
    let Some(target) = symlink_target_to_posix(&raw) else {
        return false;
    };
    if resolve_symlink_target_in_root(rel, &target).is_none() {
        return false;
    }
    std::fs::metadata(abs).map(|m| m.is_file()).unwrap_or(false)
}

/// Like `walk_workspace_files` but does NOT exclude `target/` (because
/// the cache dir itself is often called `cache/` or `zccache/` and we
/// want everything below it). Returns absolute paths of regular files
/// plus, separately, the absolute paths of symlinks encountered (#1548 —
/// the walk never follows them; validation happens in
/// [`walk_cache_files_for_profile`]).
fn walk_cache_files(
    cache_dir: &Path,
    threads: Option<usize>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let walker = jwalk::WalkDir::new(cache_dir)
        .follow_links(false)
        .skip_hidden(false);
    let walker = walker.parallelism(walk_parallelism(threads));
    let mut files = Vec::new();
    let mut symlinks = Vec::new();
    for entry in walker {
        let entry = entry.map_err(|err| SaveLoadError::Walk {
            path: cache_dir.to_path_buf(),
            message: err.to_string(),
        })?;
        let file_type = entry.file_type();
        if file_type.is_file() {
            files.push(entry.path());
        } else if file_type.is_symlink() {
            symlinks.push(entry.path());
        }
    }
    files.sort();
    symlinks.sort();
    Ok((files, symlinks))
}

#[derive(Debug, Default)]
struct CacheWalk {
    included_paths: Vec<PathBuf>,
    excluded_files: u64,
    excluded_bytes: u64,
    /// Validated in-root symlinks to record in the manifest (#1548).
    symlinks: Vec<SymlinkEntry>,
    /// Symlinks skipped (absolute / escaping / broken / unreadable
    /// target). Each one warned on stderr at walk time.
    skipped_symlinks: u64,
}

fn walk_cache_files_for_profile(
    cache_dir: &Path,
    threads: Option<usize>,
    profile: SaveProfile,
) -> Result<CacheWalk> {
    let mut walk = CacheWalk::default();
    let (files, symlinks) = walk_cache_files(cache_dir, threads)?;
    for abs in files {
        let rel = abs
            .strip_prefix(cache_dir)
            .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
        if archive_always_excludes_cache_path(rel)
            || (profile == SaveProfile::Ci && ci_profile_excludes_cache_path(rel))
            || (profile == SaveProfile::Cook && cook_profile_excludes_cache_path(rel))
        {
            let meta = std::fs::metadata(&abs).map_err(|e| io(&abs, e))?;
            walk.excluded_files += 1;
            walk.excluded_bytes = walk.excluded_bytes.saturating_add(meta.len());
        } else {
            walk.included_paths.push(abs);
        }
    }
    for abs in symlinks {
        let rel = abs
            .strip_prefix(cache_dir)
            .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
        if archive_always_excludes_cache_path(rel)
            || (profile == SaveProfile::Ci && ci_profile_excludes_cache_path(rel))
            || (profile == SaveProfile::Cook && cook_profile_excludes_cache_path(rel))
        {
            walk.excluded_files += 1;
            continue;
        }
        match cache_symlink_entry(&abs, rel) {
            Ok(entry) => walk.symlinks.push(entry),
            Err(reason) => {
                // Record-and-skip LOUDLY (#1548): an unsafe or broken
                // symlink is never silently dropped from the archive.
                // Whatever consumed it after a restore sees a missing
                // path and conservatively rebuilds.
                eprintln!(
                    "soldr save: skipping symlink {} ({reason}) — not archived",
                    abs.display()
                );
                walk.skipped_symlinks += 1;
            }
        }
    }
    Ok(walk)
}

/// Build the manifest entry for one on-disk symlink, or explain why it is
/// conservatively excluded from the archive.
fn cache_symlink_entry(abs: &Path, rel: &Path) -> std::result::Result<SymlinkEntry, &'static str> {
    let raw = std::fs::read_link(abs).map_err(|_| "unreadable link target")?;
    let target = symlink_target_to_posix(&raw).ok_or("non-UTF-8 link target")?;
    resolve_symlink_target_in_root(rel, &target).ok_or("absolute or root-escaping link target")?;
    // The link must resolve to something real at save time — a dangling
    // link is never archived (restored consumers go Dirty instead of
    // trusting a target we could not verify).
    let followed = std::fs::metadata(abs).map_err(|_| "broken link target")?;
    Ok(SymlinkEntry {
        path: rel_to_posix(rel),
        target,
        is_dir: followed.is_dir(),
    })
}

/// Runtime coordination files are local to one daemon instance and cache
/// root. Restoring PID files, spawn locks, sockets, or failure markers into a
/// different root can prevent the embedded compile daemon from starting.
/// They are never cache payload, regardless of the requested save profile.
fn archive_always_excludes_cache_path(rel: &Path) -> bool {
    if rel.components().next().is_some_and(|component| {
        matches!(component, std::path::Component::Normal(part)
            if part.to_string_lossy().eq_ignore_ascii_case("soldr-daemon"))
    }) {
        return true;
    }
    path_is_transient_runtime_file(rel)
}

/// Locks, sockets, PID files, and in-flight staging scratch, at any depth.
///
/// The doc above says these are "never cache payload, regardless of the
/// requested save profile", but only the top-level `soldr-daemon/` tree was
/// actually excluded that way -- the lock/socket/pid rules lived solely in
/// the `ci` profile. So a full-profile `soldr save` archived the embedded
/// cache's live coordination files, and hit the obvious consequence:
///
/// ```text
/// soldr save: io error at .../embedded-v1/v1.12.17/staging/2492-0-.../.active.lock:
///   No such file or directory (os error 2)
/// ```
///
/// The daemon deleted its own lock between the directory walk and the stat.
/// Archiving it was never wanted -- restoring a stale lock or socket into a
/// different root is exactly what the doc warns prevents the compile daemon
/// from starting -- so the fix is to stop collecting it, not to widen the
/// error handling around it.
///
/// `staging/` is included by directory name because its contents are
/// partially-written files by construction: a publish in flight is not cache
/// payload, and its name is not predictable enough to match by suffix.
fn path_is_transient_runtime_file(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if parts.iter().any(|part| part == "staging") {
        return true;
    }
    let Some(file_name) = parts.last().map(String::as_str) else {
        return false;
    };
    matches!(file_name, "lock" | ".lock" | "pid" | ".pid")
        || file_name.ends_with(".lock")
        || file_name.ends_with(".sock")
        || file_name.ends_with(".socket")
        || file_name.ends_with(".pid")
}

fn manifest_path_is_daemon_runtime(path: &str) -> bool {
    manifest_rel_to_path(path)
        .ok()
        .is_some_and(|rel| archive_always_excludes_cache_path(&rel))
}

/// Return true when a cache-relative path is intentionally omitted from
/// the `ci` / `minimal` save profile. This is intentionally conservative:
/// it only drops runtime diagnostics, scratch files, locks/sockets, and
/// top-level soldr-managed tool/binary trees that are re-materialized by
/// the installer rather than consumed by rustc cache lookups.
/// Cook payload exclusions for a cargo target directory (soldr#2996).
///
/// Keeps the dependency graph: `deps/*.rlib` / `*.rmeta` / `*.so` / `*.dylib`
/// / `*.dll`, `.fingerprint/`, and everything under `build/` — including the
/// extensionless `build-script-build` executables, which Cargo requires to
/// rematerialize a dependency closure.
///
/// Drops what a build adds on top and what soldr#2931 classifies tier 3:
/// linked binaries and test executables, `incremental/`, and the
/// `examples/` / `doc/` / test trees. Those are the most volatile artifacts a
/// workspace produces (every source edit relinks them) and the largest, which
/// is how a cook-keyed entry reached 1.62 GB against an 83 MiB cook slice.
///
/// The executable test is deliberately lexical — "no extension, or `.exe`" —
/// because the walk classifies paths, not file modes, and a mode check would
/// disagree between the saving host and a cross-built tree.
pub fn cook_profile_excludes_cache_path(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let Some((file_name, dirs)) = parts.split_last() else {
        return false;
    };

    // Dependency payload that the extensionless-executable rule below would
    // otherwise swallow, so both are checked first:
    //   * `build/` -- build-script executables and their `out/` products;
    //     Cargo requires them to rematerialize a dependency closure.
    //   * `.fingerprint/` -- cargo's own freshness metadata, whose entries
    //     (`lib-serde`, `bin-foo`) carry no extension. Dropping these makes
    //     Cargo rebuild every restored unit, which defeats the archive.
    if dirs
        .iter()
        .any(|part| part == "build" || part == ".fingerprint")
    {
        return false;
    }

    if dirs
        .iter()
        .any(|part| matches!(part.as_str(), "incremental" | "examples" | "doc" | "tests"))
    {
        return true;
    }

    // Debug sidecars: large, and never needed to serve a compile.
    if file_name.ends_with(".pdb") || file_name.ends_with(".dwp") || file_name.ends_with(".dwo") {
        return true;
    }
    if dirs.iter().any(|part| part.ends_with(".dsym")) || file_name.ends_with(".dsym") {
        return true;
    }

    // Linked products: an extensionless file (Unix) or a `.exe` (Windows).
    // Dependency libraries all carry a library extension and are kept.
    file_name.ends_with(".exe") || !file_name.contains('.')
}

pub fn ci_profile_excludes_cache_path(rel: &Path) -> bool {
    let parts: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return false;
    }

    if parts.iter().any(|part| {
        matches!(
            part.as_str(),
            "logs" | "log" | "tmp" | "temp" | "scratch" | "sockets" | "locks" | "runtime-binaries"
        )
    }) {
        return true;
    }

    let first = parts[0].as_str();
    if matches!(first, "bin" | "downloads" | "sdk" | "toolchains") {
        return true;
    }

    let file_name = parts.last().map(String::as_str).unwrap_or_default();
    matches!(file_name, "lock" | ".lock" | "pid" | ".pid")
        || file_name.ends_with(".log")
        || file_name.ends_with(".lock")
        || file_name.ends_with(".sock")
        || file_name.ends_with(".socket")
        || file_name.ends_with(".pid")
        || file_name.ends_with(".tmp")
        || file_name.ends_with(".temp")
}

/// Purely-LEXICAL symlink-target containment check (#1548). Resolves
/// `target` relative to `link_rel`'s parent directory (both relative to
/// the same root) and returns the normalized root-relative path of the
/// resolved target, or `None` when the link is unsafe to preserve:
///
/// * absolute targets (`/x`, `C:\x`, UNC prefixes) — rejected outright,
///   even if they happen to point back inside the root;
/// * targets whose `..` traversal escapes the root;
/// * empty targets or targets resolving to the root itself.
///
/// Never touches the filesystem — callers separately decide whether the
/// resolved path must exist (save does; load does not, because the link's
/// payload may legitimately be extracted after the link is examined).
fn resolve_symlink_target_in_root(link_rel: &Path, target: &str) -> Option<PathBuf> {
    if target.is_empty() {
        return None;
    }
    let mut resolved: Vec<std::ffi::OsString> = link_rel
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(s) => Some(s.to_os_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for component in Path::new(target).components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => return None,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Escaping above the root ends validation immediately.
                resolved.pop()?;
            }
            std::path::Component::Normal(part) => resolved.push(part.to_os_string()),
        }
    }
    if resolved.is_empty() {
        return None;
    }
    Some(resolved.iter().collect())
}

/// Convert a raw `read_link` value into the forward-slashed UTF-8 string
/// stored in the manifest. `None` for non-UTF-8 targets (conservatively
/// skipped — they can't round-trip through the protobuf string field).
fn symlink_target_to_posix(raw: &Path) -> Option<String> {
    let s = raw.to_str()?;
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        Some(s.replace('\\', "/"))
    } else {
        Some(s.to_string())
    }
}

fn rel_to_posix(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn manifest_rel_to_path(path: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return Err(SaveLoadError::BadArchivePath(path.to_string()));
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return Err(SaveLoadError::BadArchivePath(path.to_string()));
    }
    Ok(out)
}

fn archive_rel_to_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            _ => return Err(SaveLoadError::BadArchivePath(path.display().to_string())),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(SaveLoadError::BadArchivePath(path.display().to_string()));
    }
    Ok(out)
}

/// Manifest entry + the `Metadata` it was derived from. The metadata is
/// reused for the tar header when the file is appended, so `save` /
/// `save_delta` stat each cache file exactly once instead of once in
/// the hash pre-pass and again at append time (#1541). This also keeps
/// the tar header and the manifest byte-for-byte consistent even if
/// the file mutates between the two phases.
/// `Ok(None)` when the file vanished between the directory walk and this
/// stat.
///
/// Defence in depth behind the exclusion above. The walk and the archive are
/// necessarily two separate passes over a tree a live daemon is still
/// writing, so *some* window exists no matter how good the filter is, and a
/// file that no longer exists cannot be cache payload worth failing a whole
/// save over. Scoped to `NotFound` specifically -- a permissions error or a
/// bad disk still fails loudly, because those mean the archive would be
/// silently incomplete.
fn cache_file_entry(
    cache_dir: &Path,
    abs: &Path,
) -> Result<Option<(CacheFile, std::fs::Metadata)>> {
    let rel = abs
        .strip_prefix(cache_dir)
        .map_err(|_| SaveLoadError::BadArchivePath(abs.display().to_string()))?;
    let meta = match std::fs::metadata(abs) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(abs, e)),
    };
    let hash = hash_file(abs)?;
    let entry = CacheFile {
        path: rel_to_posix(rel),
        mtime_ns: mtime_ns(&meta),
        size: meta.len(),
        blake3: hash.to_vec(),
    };
    Ok(Some((entry, meta)))
}

// ---------- save ----------
