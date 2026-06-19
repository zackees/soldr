//! `soldr cook` — cargo-chef-style content-addressable dep prebuild.
//!
//! This is a thin shim that:
//! 1. Validates that `Cargo.toml` (and ideally `Cargo.lock`) live in the
//!    cwd, so cargo-chef has something to read.
//! 2. Resolves the pinned `cargo-chef` binary via the standard fetch
//!    pipeline (registry entry in `crate::fetch::known_tools`).
//! 3. Routes `cargo chef prepare` and `cargo chef cook` through the
//!    existing cargo front door so the underlying compile picks up
//!    zccache (RUSTC_WRAPPER), `ZCCACHE_PATH_REMAP=auto`, the soldr
//!    linker selection, the soldr-managed CARGO_HOME / RUSTUP_HOME, and
//!    every other piece of `soldr cargo` plumbing.
//!
//! See issue zackees/soldr#359 for design context and the companion
//! `zackees/setup-soldr#110` for the GitHub Action that consumes the
//! resulting `target/` tarball.

use crate::cache_lib::cook_archive::{self, cook_cache_dir, sha_abbrev, PackedCookArchive};
use crate::cache_lib::strip_target::{strip_target, StripTargetOptions};
use crate::cargo_front_door;
use crate::core::git::{
    cargo_lock_is_gitignored, cargo_lock_is_tracked, current_branch_name, find_git_worktree_root,
    origin_url,
};
use crate::core::{
    probe_toolchain_binary, read_rust_toolchain_manifest, SoldrError, SoldrPaths, TargetTriple,
};
use crate::daemon::client::{self, CookLookupOutcome};
use crate::ZccacheSourceArg;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Parsed `soldr cook` invocation surface. Mirrors the relevant subset of
/// `cargo build` knobs plus the cargo-chef-specific recipe controls.
#[derive(Debug, Clone, Default)]
pub(crate) struct CookArgs {
    /// Compile dependencies in release mode (`cargo chef cook --release`).
    pub release: bool,
    /// Compile against a non-default target triple. Forwarded to cargo-chef
    /// as `--target <triple>`.
    pub target: Option<String>,
    /// Build the whole workspace. Forwarded to cargo-chef as `--workspace`.
    pub workspace: bool,
    /// Explicit cargo profile (`--profile <name>`). Forwarded to cargo-chef.
    pub profile: Option<String>,
    /// Restrict the cook step to a specific package (`--package`/`-p`).
    pub packages: Vec<String>,
    /// Override the recipe path. Defaults to a per-invocation temp dir so
    /// the recipe is invisible to the project.
    pub recipe_path: Option<PathBuf>,
    /// When set, retain the recipe at `<cwd>/recipe.json` (or at the value
    /// of `--recipe-path` when both are supplied) so the user can inspect
    /// it. Without this the recipe is deleted on exit.
    pub keep_recipe: bool,
    /// Only run `cargo chef prepare`. Useful for Docker users who want the
    /// recipe layer ahead of the heavy `cook` step.
    pub prepare_only: bool,
    /// Only run `cargo chef cook` against an existing recipe. Requires
    /// `--recipe-path`.
    pub cook_only: bool,
    /// Catch-all for tokens past `--`. Forwarded verbatim to
    /// `cargo chef cook` so users can pass `--features foo` etc.
    pub passthrough: Vec<String>,
    /// Skip the post-cook target/ trim (issue #459). Default `false`:
    /// cook always trims cargo-recreatable noise so the downstream
    /// tarball ships dramatically fewer bytes.
    pub no_trim: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct CookContext {
    /// Resolved workspace root (cwd or the nearest ancestor with a
    /// `Cargo.toml`). Surfaced for tests and for future flags that need
    /// to anchor paths relative to the manifest.
    #[allow(dead_code)]
    pub manifest_dir: PathBuf,
    pub recipe_path: PathBuf,
    /// True when the recipe lives inside a `tempfile::TempDir` that we own
    /// and should clean up on exit (unless `--keep-recipe` is set).
    pub recipe_owned_tempdir: bool,
}

/// Parse a Vec<String> argv (everything after `cook`) into a `CookArgs`.
///
/// Recognised flags: `--release`, `--target <triple>`, `--workspace`,
/// `--profile <name>`, `-p`/`--package <name>` (repeatable),
/// `--recipe-path <path>`, `--keep-recipe`, `--prepare-only`,
/// `--cook-only`. Anything after a literal `--` is collected into
/// `passthrough`. Unknown flags before `--` are an error (so typos fail
/// fast).
pub(crate) fn parse_cook_args(args: &[String]) -> Result<CookArgs, SoldrError> {
    let mut out = CookArgs::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--release" => out.release = true,
            "--workspace" | "--all" => out.workspace = true,
            "--keep-recipe" => out.keep_recipe = true,
            "--prepare-only" => out.prepare_only = true,
            "--cook-only" => out.cook_only = true,
            "--no-trim" => out.no_trim = true,
            "--target" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--target requires a value".into()))?;
                out.target = Some(value.clone());
            }
            v if v.starts_with("--target=") => {
                out.target = Some(v.trim_start_matches("--target=").to_string());
            }
            "--profile" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--profile requires a value".into()))?;
                out.profile = Some(value.clone());
            }
            v if v.starts_with("--profile=") => {
                out.profile = Some(v.trim_start_matches("--profile=").to_string());
            }
            "--recipe-path" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--recipe-path requires a value".into()))?;
                out.recipe_path = Some(PathBuf::from(value));
            }
            v if v.starts_with("--recipe-path=") => {
                out.recipe_path = Some(PathBuf::from(v.trim_start_matches("--recipe-path=")));
            }
            "-p" | "--package" => {
                let value = iter
                    .next()
                    .ok_or_else(|| SoldrError::Other("--package requires a value".into()))?;
                out.packages.push(value.clone());
            }
            v if v.starts_with("--package=") => {
                out.packages
                    .push(v.trim_start_matches("--package=").to_string());
            }
            "--" => {
                out.passthrough.extend(iter.by_ref().cloned());
                break;
            }
            other if other.starts_with('-') => {
                return Err(SoldrError::Other(format!(
                    "soldr cook: unknown flag `{other}` — pass project-specific options after `--`"
                )));
            }
            other => {
                // Positional pre-`--` tokens go to the passthrough bag too;
                // cargo-chef cook ignores most of them but we want to be
                // generous (e.g. `--features foo` written without `--`).
                out.passthrough.push(other.to_string());
            }
        }
    }

    if out.prepare_only && out.cook_only {
        return Err(SoldrError::Other(
            "soldr cook: --prepare-only and --cook-only are mutually exclusive".into(),
        ));
    }
    if out.cook_only && out.recipe_path.is_none() {
        return Err(SoldrError::Other(
            "soldr cook: --cook-only requires --recipe-path so cargo-chef can read the recipe"
                .into(),
        ));
    }

    Ok(out)
}

/// Locate the workspace manifest by walking up from the cwd. Emits an
/// actionable error when `Cargo.toml` is missing entirely.
pub(crate) fn resolve_manifest_dir(start: &Path) -> Result<PathBuf, SoldrError> {
    let mut current = start.to_path_buf();
    loop {
        if current.join("Cargo.toml").is_file() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(SoldrError::Other(format!(
                "soldr cook: no Cargo.toml found at or above {} — run from a Rust project root",
                start.display()
            )));
        }
    }
}

/// In-memory snapshot of a project's source-defining files (every
/// `Cargo.toml` / `Cargo.lock` / `*.rs` outside `target/` and `.git/`), used
/// to undo cargo-chef's in-place skeleton reconstruction after the cook
/// compile so `soldr cook` leaves the project pristine (zackees/soldr#566).
pub(crate) struct ProjectSourceSnapshot {
    files: Vec<(PathBuf, Vec<u8>)>,
}

impl ProjectSourceSnapshot {
    /// File count captured (exposed for tests/diagnostics).
    #[allow(clippy::len_without_is_empty)]
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }
}

/// True for the files cargo-chef rewrites in place: crate manifests, the
/// lockfile, and Rust sources (crate roots get stubbed). Restricting the
/// snapshot to these keeps it small (source, not build output).
fn is_project_source_file(name: &str) -> bool {
    name == "Cargo.toml" || name == "Cargo.lock" || name.ends_with(".rs")
}

/// Recurse `dir`, skipping `target/` and `.git/` at any depth, invoking `f`
/// on every regular project-source file with its path relative to `base`.
fn walk_project_source(
    dir: &Path,
    base: &Path,
    f: &mut dyn FnMut(&Path, PathBuf),
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if file_type.is_dir() {
            if name.as_ref() == "target" || name.as_ref() == ".git" {
                continue;
            }
            walk_project_source(&path, base, f)?;
        } else if file_type.is_file() && is_project_source_file(name.as_ref()) {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(path.as_path())
                .to_path_buf();
            f(path.as_path(), rel);
        }
    }
    Ok(())
}

/// Capture the project's source-defining files under `manifest_dir`.
pub(crate) fn snapshot_project_source(
    manifest_dir: &Path,
) -> Result<ProjectSourceSnapshot, SoldrError> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    {
        let mut collect = |abs: &Path, rel: PathBuf| {
            if let Ok(bytes) = std::fs::read(abs) {
                files.push((rel, bytes));
            }
        };
        walk_project_source(manifest_dir, manifest_dir, &mut collect).map_err(|e| {
            SoldrError::Other(format!(
                "soldr cook: failed to snapshot project source under {}: {e}",
                manifest_dir.display()
            ))
        })?;
    }
    Ok(ProjectSourceSnapshot { files })
}

/// Restore the project to its snapshotted state: delete every current
/// project-source file (removing cargo-chef's stubs / any added crate roots),
/// then rewrite the captured originals. `target/`/`.git/` are never touched,
/// so the cooked dependency artifacts survive.
pub(crate) fn restore_project_source(
    manifest_dir: &Path,
    snapshot: &ProjectSourceSnapshot,
) -> Result<(), SoldrError> {
    // 1. Remove chef's in-place rewrites (manifests + .rs outside target/.git).
    let mut to_delete: Vec<PathBuf> = Vec::new();
    {
        let mut mark = |abs: &Path, _rel: PathBuf| to_delete.push(abs.to_path_buf());
        // Best-effort: a read error here just means fewer deletions; the
        // rewrite below still restores originals.
        let _ = walk_project_source(manifest_dir, manifest_dir, &mut mark);
    }
    for path in to_delete {
        let _ = std::fs::remove_file(&path);
    }
    // 2. Rewrite the captured originals.
    for (rel, bytes) in &snapshot.files {
        let dest = manifest_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SoldrError::Other(format!(
                    "soldr cook: failed to restore directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&dest, bytes).map_err(|e| {
            SoldrError::Other(format!(
                "soldr cook: failed to restore {}: {e}",
                dest.display()
            ))
        })?;
    }
    Ok(())
}

/// Build the [`CookContext`] used by [`run_cook`]. Pure enough to unit-test:
/// takes the resolved cwd, parsed args, and (for tests) a hook that creates
/// the tempdir backing an ephemeral recipe.
pub(crate) fn build_cook_context(
    cwd: &Path,
    args: &CookArgs,
) -> Result<(CookContext, Option<tempfile::TempDir>), SoldrError> {
    let manifest_dir = resolve_manifest_dir(cwd)?;

    let lockfile = manifest_dir.join("Cargo.lock");
    if !args.cook_only && !lockfile.is_file() {
        eprintln!(
            "soldr cook: warning: {} is missing — cargo-chef will derive the recipe from Cargo.toml alone, which weakens content-addressability",
            lockfile.display()
        );
    }

    let (recipe_path, tempdir, owned_tempdir) = match (args.recipe_path.as_ref(), args.keep_recipe)
    {
        (Some(path), _) => {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                manifest_dir.join(path)
            };
            (absolute, None, false)
        }
        (None, true) => {
            // --keep-recipe without --recipe-path: drop the recipe next to
            // the project's Cargo.toml so it's easy to spot.
            (manifest_dir.join("recipe.json"), None, false)
        }
        (None, false) => {
            let tmp = tempfile::tempdir().map_err(|e| {
                SoldrError::Other(format!("soldr cook: failed to create temp dir: {e}"))
            })?;
            let recipe = tmp.path().join("recipe.json");
            (recipe, Some(tmp), true)
        }
    };

    Ok((
        CookContext {
            manifest_dir,
            recipe_path,
            recipe_owned_tempdir: owned_tempdir,
        },
        tempdir,
    ))
}

/// Top-level dispatch. Invoked from `Commands::Cook` in `main.rs`.
pub(crate) async fn run_cook(
    args: &[String],
    cache_enabled: bool,
    zccache_source: ZccacheSourceArg,
) -> Result<i32, SoldrError> {
    let parsed = parse_cook_args(args)?;
    let cwd = std::env::current_dir()
        .map_err(|e| SoldrError::Other(format!("soldr cook: failed to read cwd: {e}")))?;
    let (ctx, _tempdir_guard) = build_cook_context(&cwd, &parsed)?;

    // Phase 1: prepare. Cheap, deterministic, reads only the manifest tree.
    if !parsed.cook_only {
        let prepare_args = build_chef_prepare_args(&ctx);
        let code = cargo_front_door::run_cargo_front_door(
            &prepare_args,
            cache_enabled,
            zccache_source,
            false,
        )
        .await?;
        if code != 0 {
            return Ok(code);
        }
        let stripped = sanitize_cargo_chef_recipe(&ctx.recipe_path)?;
        if stripped > 0 {
            eprintln!(
                "soldr cook: removed {stripped} generated cargo-chef plugin manifest keys from recipe"
            );
        }
        if parsed.prepare_only {
            eprintln!(
                "soldr cook: wrote recipe to {} (--prepare-only)",
                ctx.recipe_path.display()
            );
            return Ok(0);
        }
    }

    // Snapshot the project's source-defining files (manifests + Rust sources)
    // before the cook compile. `cargo chef cook` reconstructs the cargo-chef
    // skeleton IN PLACE — it stubs every crate root (`fn main() {}` / empty
    // lib) and normalizes every crate version to `0.0.1` — and does NOT
    // restore the real project. Left unrestored, any build run after
    // `soldr cook` in the same job compiles the stub source under the wrong
    // (`0.0.1`) version, which silently breaks correctness AND churns the
    // compile-cache key for first-party crates (different `-C metadata` than a
    // warm run where cook's cache hits and the tree is pristine) — see
    // zackees/soldr#566, zackees/zccache#448.
    let source_snapshot = snapshot_project_source(&ctx.manifest_dir)?;

    // #621: skip-cook-when-warm. After Phase 1 (prepare) the recipe is
    // fully resolved. If a previous successful cook left a marker file
    // under target/ recording the same recipe + rustc version, the
    // restored target/ from cook-cache covers the entire dep set and
    // Phase 2 would do no net work — cargo-chef would still spend ~5
    // minutes walking the dep graph emitting "Compiling X" lines per
    // crate, with zccache hitting at ~96% (the orchestration cost is
    // the bottleneck, not codegen). When the marker matches, short-
    // circuit Phase 2 entirely.
    let cook_target_dir = resolve_cook_target_dir(&ctx.manifest_dir, &parsed);
    let cook_marker_path = cook_target_dir.join(".soldr-cook-marker.json");
    let expected_marker = compute_cook_marker(&ctx, &parsed);
    let warm_skip = matches!(
        (&expected_marker, read_cook_marker(&cook_marker_path)),
        (Some(expected), Some(existing)) if existing == *expected
    );
    if warm_skip {
        eprintln!(
            "soldr cook: warm-cook detected (recipe + rustc match the prior cook marker at {}) — skipping Phase 2 (cargo chef cook). Estimated savings: ~5 min on Coverage-shape workloads. See soldr#621.",
            cook_marker_path.display()
        );
        // Restore source before returning — same invariant as the
        // normal post-cook path (project tree must be pristine for
        // downstream cargo build).
        if let Err(e) = restore_project_source(&ctx.manifest_dir, &source_snapshot) {
            eprintln!(
                "soldr cook: warning: failed to restore project source after warm-skip \
                 (project may be left in cargo-chef's stub state): {e}"
            );
        }
        return Ok(0);
    }

    // Phase 2: cook. Heavy — compiles every transitive dep against a stub
    // project. Output lands in `target/`.
    let cook_args = build_chef_cook_args(&ctx, &parsed);
    let cook_result =
        cargo_front_door::run_cargo_front_door(&cook_args, cache_enabled, zccache_source, false)
            .await;

    // Restore the project to its pre-cook state regardless of how cook exited,
    // so the tree is pristine for every subsequent build step (#566).
    if let Err(e) = restore_project_source(&ctx.manifest_dir, &source_snapshot) {
        eprintln!(
            "soldr cook: warning: failed to restore project source after cook \
             (project may be left in cargo-chef's stub state): {e}"
        );
    }

    let code = cook_result?;
    if code != 0 {
        return Ok(code);
    }

    // #621: persist the warm-cook marker so the next run with the same
    // recipe + rustc can short-circuit Phase 2. Best-effort — if the
    // write fails, the only impact is the next run pays the full
    // cargo-chef orchestration cost. expected_marker is Some when
    // recipe + rustc could be hashed; None on rare resolution
    // failures (e.g. recipe.json missing — shouldn't happen post-cook).
    if let Some(marker) = expected_marker {
        if let Err(e) = write_cook_marker(&cook_marker_path, &marker) {
            eprintln!(
                "soldr cook: warning: failed to write warm-cook marker {} (next run won't short-circuit Phase 2): {e}",
                cook_marker_path.display()
            );
        }
    }

    // Phase 3: post-cook target/ trim (issue #459). Cargo-chef cook
    // leaves cargo-recreatable noise (incremental state, the synthetic
    // stub binary, build-script binaries, large stderr blobs) under
    // target/ that the downstream consumer never reads. Trimming here
    // shrinks the tarball setup-soldr et al ship across CI runners.
    if !parsed.no_trim {
        run_cook_target_trim(&ctx.manifest_dir);
    }

    // Phase 4: index the cooked target/ into the cross-repo shared
    // `~/.soldr/cache/cook/` (issue #577, meta #579). Best-effort;
    // any failure here surfaces as a warning but never fails the
    // user's cook command — the local `target/` is still valid.
    if let Err(err) = index_cooked_artifact(&ctx, &parsed) {
        eprintln!("{} {err}", yellow_warning_prefix());
    }

    if parsed.keep_recipe {
        eprintln!(
            "soldr cook: recipe retained at {}",
            ctx.recipe_path.display()
        );
    } else if ctx.recipe_owned_tempdir {
        eprintln!("soldr cook: deps built; recipe was ephemeral");
    } else {
        eprintln!(
            "soldr cook: deps built; recipe retained at {}",
            ctx.recipe_path.display()
        );
    }
    Ok(0)
}

/// ANSI-yellow `warning:` prefix gated on stderr being a terminal.
/// Mirrors the existing pattern in `cargo_front_door::disk`.
fn yellow_warning_prefix() -> &'static str {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        "\x1b[33msoldr cook: warning:\x1b[0m"
    } else {
        "soldr cook: warning:"
    }
}

/// ANSI-green `indexed` prefix for the success line.
fn green_indexed_prefix() -> &'static str {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        "\x1b[32msoldr cook: indexed\x1b[0m"
    } else {
        "soldr cook: indexed"
    }
}

/// Resolve the `target/<triple?>/<profile>/` directory cargo-chef
/// just populated. cargo's mapping: `--release` → `release`, no
/// flag → `debug` (the "dev" profile), `--profile=<name>` → `<name>`.
/// With `--target X` the artifacts land under `target/X/<profile>/`.
fn resolve_cook_target_dir(manifest_dir: &Path, args: &CookArgs) -> PathBuf {
    let profile = resolve_profile_dir_name(args);
    let mut root = manifest_dir.join("target");
    if let Some(triple) = args.target.as_deref() {
        root = root.join(triple);
    }
    root.join(profile)
}

fn resolve_profile_dir_name(args: &CookArgs) -> &str {
    if let Some(p) = args.profile.as_deref() {
        // `dev` is special-cased by cargo → `debug` dir on disk.
        return if p == "dev" { "debug" } else { p };
    }
    if args.release {
        "release"
    } else {
        "debug"
    }
}

fn resolve_profile_name(args: &CookArgs) -> &str {
    if let Some(p) = args.profile.as_deref() {
        return p;
    }
    if args.release {
        "release"
    } else {
        "dev"
    }
}

/// Run `rustc -V` and return the trimmed first line, e.g.
/// `rustc 1.94.1 (abcdef0 2026-05-30)`. Returns `None` when rustc is
/// unreachable — the caller treats this as "no rustc identity" and
/// skips indexing (cross-repo sharing without rustc keying would be
/// unsafe).
/// #621 warm-cook marker. Captures the inputs that determine whether
/// the next Phase-2 cook would do net work. When this matches a
/// previously-persisted marker under `target/`, the cook orchestration
/// would walk the dep graph just to confirm zccache hits at ~96% —
/// ~5 minutes of wasted wall clock on Coverage-shape workloads.
#[derive(Debug, PartialEq, Eq)]
struct CookMarker {
    /// Schema version. Bump on incompatible format changes so an old
    /// marker doesn't accidentally satisfy a new check.
    version: u32,
    /// SHA-256 hex of the post-`sanitize_cargo_chef_recipe` recipe.json.
    recipe_sha256: String,
    /// `rustc -V` first line.
    rustc_version: String,
    /// `soldr --version` (CARGO_PKG_VERSION). A different soldr could
    /// theoretically have changed the recipe sanitizer behavior.
    soldr_version: String,
}

const COOK_MARKER_VERSION: u32 = 1;

/// Read + parse the warm-cook marker at `path`. Any error (missing
/// file, malformed JSON, missing field, version mismatch) returns
/// `None` so the caller falls through to the normal cook path.
fn read_cook_marker(path: &Path) -> Option<CookMarker> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let version = value.get("version")?.as_u64()? as u32;
    if version != COOK_MARKER_VERSION {
        return None;
    }
    Some(CookMarker {
        version,
        recipe_sha256: value.get("recipe_sha256")?.as_str()?.to_string(),
        rustc_version: value.get("rustc_version")?.as_str()?.to_string(),
        soldr_version: value.get("soldr_version")?.as_str()?.to_string(),
    })
}

/// Write the marker as JSON. Creates the parent dir if needed.
fn write_cook_marker(path: &Path, marker: &CookMarker) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "version": marker.version,
        "recipe_sha256": &marker.recipe_sha256,
        "rustc_version": &marker.rustc_version,
        "soldr_version": &marker.soldr_version,
    });
    std::fs::write(path, body.to_string())
}

/// Compute the expected warm-cook marker for the current invocation.
/// Returns `None` when any required input can't be resolved (recipe
/// file missing — shouldn't happen post-Phase-1, but if it does we
/// simply skip the optimization).
fn compute_cook_marker(ctx: &CookContext, _parsed: &CookArgs) -> Option<CookMarker> {
    use sha2::{Digest, Sha256};
    let recipe_bytes = std::fs::read(&ctx.recipe_path).ok()?;
    let mut h = Sha256::new();
    h.update(&recipe_bytes);
    let recipe_sha256 = hex_lower(&h.finalize());
    let rustc_version = rustc_version_string(&ctx.manifest_dir).unwrap_or_default();
    Some(CookMarker {
        version: COOK_MARKER_VERSION,
        recipe_sha256,
        rustc_version,
        soldr_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

/// Lower-case hex helper (`crate::cache_lib::cook_archive::sha_abbrev` truncates).
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn rustc_version_string(manifest_dir: &Path) -> Option<String> {
    let rustc = probe_toolchain_binary("rustc", Some(manifest_dir))
        .unwrap_or_else(|| PathBuf::from("rustc"));
    let out = Command::new(rustc).arg("-V").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.lines().next()?.trim().to_string())
}

fn resolve_channel(manifest_dir: &Path) -> Option<String> {
    match read_rust_toolchain_manifest(manifest_dir) {
        Ok(m) => m.channel,
        Err(_) => None,
    }
}

fn resolve_target_triple(manifest_dir: &Path, args: &CookArgs) -> Option<String> {
    if let Some(triple) = args.target.as_deref() {
        return Some(triple.to_string());
    }
    TargetTriple::detect_in_dir(manifest_dir)
        .ok()
        .map(|t| t.to_string())
}

/// Index the cooked `target/<profile>/` tree into the cross-repo
/// shared `~/.soldr/cache/cook/` (issue #577) and register the
/// artifact with the daemon via `CookRecord`. Best-effort: a failure
/// here surfaces as a warning but never fails the cook command.
fn index_cooked_artifact(ctx: &CookContext, args: &CookArgs) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    index_cooked_artifact_with_packer(ctx, args, &paths, cook_archive::pack_cook_archive)
}

fn index_cooked_artifact_with_packer<F>(
    ctx: &CookContext,
    args: &CookArgs,
    paths: &SoldrPaths,
    packer: F,
) -> Result<(), SoldrError>
where
    F: FnOnce(
        &Path,
        &Path,
    ) -> Result<PackedCookArchive, crate::cache_lib::target_registry::RegistryError>,
{
    // 1. Sharing-eligibility checks.
    let lockfile = ctx.manifest_dir.join("Cargo.lock");
    if !lockfile.is_file() {
        // Already warned earlier in build_cook_context.
        return Ok(());
    }
    let has_git = find_git_worktree_root(&ctx.manifest_dir).is_some();
    let lock_tracked = cargo_lock_is_tracked(&ctx.manifest_dir);
    let lock_gitignored = cargo_lock_is_gitignored(&ctx.manifest_dir);

    if !lock_tracked {
        // Includes both "no .git/" and "git but file untracked".
        if has_git {
            eprintln!(
                "{} cross-repo cook sharing disabled — Cargo.lock is not tracked by git. \
                 Commit it to enable.",
                yellow_warning_prefix()
            );
        } else {
            eprintln!(
                "{} no .git/ — cook artifact will be local-only, not indexed for sharing.",
                yellow_warning_prefix()
            );
        }
        return Ok(());
    }
    if lock_gitignored {
        eprintln!(
            "{} Cargo.lock is gitignored — sharing may leak deps that diverge from upstream. \
             Skipping cook-index registration.",
            yellow_warning_prefix()
        );
        return Ok(());
    }

    // 2. Resolve key components.
    let target_dir = resolve_cook_target_dir(&ctx.manifest_dir, args);
    if !target_dir.is_dir() {
        // cargo-chef cook didn't produce a target/<profile>/ tree —
        // most likely because the build failed earlier (we already
        // returned in that case). Defensive fallback: silently skip.
        return Ok(());
    }

    let triple = resolve_target_triple(&ctx.manifest_dir, args).ok_or_else(|| {
        SoldrError::Other("soldr cook: could not resolve target triple for cook-index key".into())
    })?;
    let channel = resolve_channel(&ctx.manifest_dir).unwrap_or_default();
    let rustc_version = rustc_version_string(&ctx.manifest_dir).ok_or_else(|| {
        SoldrError::Other("soldr cook: could not resolve rustc version for cook-index key".into())
    })?;
    // PR 3 (#578): the cook-index recipe hash is the workspace
    // content-fingerprint computed by
    // `cook_archive::compute_recipe_hash_proxy` so PR 3's cargo-front-
    // door pre-flight can compute the same key cheaply (no
    // cargo-chef invocation in the hot path).
    let recipe_hash =
        cook_archive::compute_recipe_hash_proxy(&ctx.manifest_dir).ok_or_else(|| {
            SoldrError::Other(
                "soldr cook: could not compute recipe hash proxy (Cargo.lock unreadable?)".into(),
            )
        })?;
    let origin = origin_url(&ctx.manifest_dir);
    let branch_name = current_branch_name(&ctx.manifest_dir);
    let profile = resolve_profile_name(args).to_string();

    // 3. Resolve paths under ~/.soldr/.
    let cook_dir = cook_cache_dir(paths);
    std::fs::create_dir_all(&cook_dir).map_err(|e| {
        SoldrError::Other(format!(
            "soldr cook: failed to create {}: {e}",
            cook_dir.display()
        ))
    })?;

    // 4. Pre-flight CookLookup so we can render a drift diagnostic
    //    when a previous recipe hash exists for this (origin, triple,
    //    profile, channel, rustc). If this cannot reach the daemon,
    //    CookRecord cannot be relied on either, so skip the expensive
    //    archive pack entirely.
    let sock = client::default_sock_path(paths);
    let prior_drift = match client::cook_lookup(
        &sock,
        recipe_hash,
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc_version.clone(),
        origin.clone(),
    ) {
        Ok(CookLookupOutcome::Hit {
            sha256,
            matched_recipe_hash,
            exact_recipe_match,
            ..
        }) => {
            // Already-cached for this exact key. Re-pack + re-record
            // anyway so the artifact bytes match the freshly built
            // target/ — a previous run might have written from a
            // sibling worktree with slightly different mtimes.
            if exact_recipe_match {
                Some(DriftSignal::AlreadyIndexed(sha256))
            } else {
                matched_recipe_hash.map(DriftSignal::Drifted)
            }
        }
        Ok(CookLookupOutcome::Miss {
            previous_origin_recipe_hashes,
        }) => previous_origin_recipe_hashes
            .first()
            .copied()
            .map(DriftSignal::Drifted),
        Err(e) => {
            eprintln!(
                "{} cook index unavailable; skipping shared cook archive pack ({e:?}).",
                yellow_warning_prefix()
            );
            return Ok(());
        }
    };

    // 5. Pack the trimmed target/<profile>/ tree.
    let packed = packer(&target_dir, &cook_dir)
        .map_err(|e| SoldrError::Other(format!("soldr cook: failed to pack cook archive: {e}")))?;

    // 6. Register with the daemon. If the daemon is unreachable we
    //    still keep the on-disk artifact for the next PR-3 hydrate
    //    attempt, but warn so the user knows sharing is one-sided.
    let cook_cmd_summary = build_cook_cmd_summary(args);
    let register = client::cook_record_with_branch(
        &sock,
        recipe_hash,
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc_version.clone(),
        packed.sha256,
        packed.size_bytes,
        origin.clone(),
        branch_name,
        cook_cmd_summary,
    );
    if let Err(e) = register {
        eprintln!(
            "{} CookRecord to daemon failed: {e:?}. Artifact written at {} but not indexed.",
            yellow_warning_prefix(),
            packed.path.display()
        );
        return Ok(());
    }

    // 7. Recipe-drift diagnostic (printed before the success line so
    //    the indexed line is the last word).
    if let Some(DriftSignal::Drifted(prev)) = prior_drift {
        eprintln!("soldr cook: recipe hash drift since {}", sha_abbrev(&prev));
    }

    // 8. Green success line.
    emit_indexed_line(&packed, origin.as_deref());
    Ok(())
}

#[derive(Debug, Clone)]
enum DriftSignal {
    Drifted([u8; 32]),
    AlreadyIndexed(#[allow(dead_code)] [u8; 32]),
}

fn emit_indexed_line(packed: &PackedCookArchive, origin: Option<&str>) {
    let mib = packed.size_bytes as f64 / 1024.0 / 1024.0;
    let origin_field = origin.unwrap_or("none");
    eprintln!(
        "{}  sha256={}  size={mib:.1} MiB  origin={origin_field}",
        green_indexed_prefix(),
        sha_abbrev(&packed.sha256),
    );
}

fn build_cook_cmd_summary(args: &CookArgs) -> String {
    let mut parts = vec!["cook".to_string()];
    if args.release {
        parts.push("--release".to_string());
    }
    if let Some(p) = args.profile.as_deref() {
        parts.push(format!("--profile={p}"));
    }
    if let Some(t) = args.target.as_deref() {
        parts.push(format!("--target={t}"));
    }
    if args.workspace {
        parts.push("--workspace".to_string());
    }
    for pkg in &args.packages {
        parts.push(format!("-p {pkg}"));
    }
    parts.join(" ")
}

/// Remove cargo-chef-generated `plugin = false` / `plugin = true`
/// target fields from the prepared recipe's embedded TOML manifests.
/// Newer Cargo warns on these obsolete keys during `cargo chef cook`,
/// but they are generated by cargo-chef's metadata round-trip rather
/// than by the user's checked-in manifests.
fn sanitize_cargo_chef_recipe(recipe_path: &Path) -> Result<usize, SoldrError> {
    let raw = std::fs::read_to_string(recipe_path).map_err(|e| {
        SoldrError::Other(format!(
            "soldr cook: failed to read cargo-chef recipe {}: {e}",
            recipe_path.display()
        ))
    })?;
    let mut recipe: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        SoldrError::Other(format!(
            "soldr cook: failed to parse cargo-chef recipe {}: {e}",
            recipe_path.display()
        ))
    })?;

    let mut removed = 0usize;
    let Some(manifests) = recipe
        .get_mut("skeleton")
        .and_then(|value| value.get_mut("manifests"))
        .and_then(|value| value.as_array_mut())
    else {
        return Ok(0);
    };

    for manifest in manifests {
        let Some(contents_value) = manifest.get_mut("contents") else {
            continue;
        };
        let Some(contents) = contents_value.as_str() else {
            continue;
        };
        let (sanitized, count) = strip_generated_plugin_lines(contents);
        if count > 0 {
            *contents_value = serde_json::Value::String(sanitized);
            removed += count;
        }
    }

    if removed > 0 {
        let encoded = serde_json::to_vec(&recipe).map_err(|e| {
            SoldrError::Other(format!(
                "soldr cook: failed to serialize sanitized recipe {}: {e}",
                recipe_path.display()
            ))
        })?;
        std::fs::write(recipe_path, encoded).map_err(|e| {
            SoldrError::Other(format!(
                "soldr cook: failed to write sanitized recipe {}: {e}",
                recipe_path.display()
            ))
        })?;
    }

    Ok(removed)
}

fn strip_generated_plugin_lines(contents: &str) -> (String, usize) {
    let mut out = String::with_capacity(contents.len());
    let mut removed = 0usize;
    for line in contents.split_inclusive('\n') {
        let trimmed = line.trim();
        if matches!(trimmed, "plugin = false" | "plugin = true") {
            removed += 1;
            continue;
        }
        out.push_str(line);
    }
    (out, removed)
}

/// Run the `cook` strip preset against the workspace's `target/` directory.
///
/// Best-effort: a missing or partially-populated `target/` (the heavy
/// case is when cargo wrote into `target/<triple>/...` and the
/// stripper just finds nothing) is not an error — the trim only ever
/// reduces output. Failures are surfaced on stderr but never abort
/// the cook command (the artifacts are already built and valid).
fn run_cook_target_trim(manifest_dir: &Path) {
    let target_dir = manifest_dir.join("target");
    if !target_dir.is_dir() {
        return;
    }
    let opts = StripTargetOptions::cook(target_dir.clone());
    match strip_target(&opts) {
        Ok(report) => {
            if report.deleted == 0 {
                return;
            }
            let mib = report.reclaimed_bytes as f64 / 1024.0 / 1024.0;
            eprintln!(
                "soldr cook: trimmed {} cargo-recreatable entries from target/ ({mib:.1} MiB reclaimed)",
                report.deleted,
            );
        }
        Err(err) => {
            eprintln!(
                "soldr cook: target/ trim failed at {}: {err} (cook output is still valid)",
                target_dir.display()
            );
        }
    }
}

/// Argv for `cargo chef prepare --recipe-path <path>`, ready to feed
/// through the cargo front door.
pub(crate) fn build_chef_prepare_args(ctx: &CookContext) -> Vec<String> {
    vec![
        "chef".to_string(),
        "prepare".to_string(),
        "--recipe-path".to_string(),
        ctx.recipe_path.display().to_string(),
    ]
}

/// Argv for `cargo chef cook ...`, ready to feed through the cargo front
/// door.
pub(crate) fn build_chef_cook_args(ctx: &CookContext, args: &CookArgs) -> Vec<String> {
    let mut out = vec![
        "chef".to_string(),
        "cook".to_string(),
        "--recipe-path".to_string(),
        ctx.recipe_path.display().to_string(),
    ];
    if args.release {
        out.push("--release".to_string());
    }
    if let Some(profile) = args.profile.as_ref() {
        out.push("--profile".to_string());
        out.push(profile.clone());
    }
    if let Some(target) = args.target.as_ref() {
        out.push("--target".to_string());
        out.push(target.clone());
    }
    if args.workspace {
        out.push("--workspace".to_string());
    }
    for pkg in &args.packages {
        out.push("--package".to_string());
        out.push(pkg.clone());
    }
    if !args.passthrough.is_empty() {
        out.push("--".to_string());
        out.extend(args.passthrough.iter().cloned());
    }
    out
}

#[cfg(test)]
#[path = "cook_tests.rs"]
mod tests;
