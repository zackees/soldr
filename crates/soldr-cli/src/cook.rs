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

use crate::cache_lib::strip_target::{strip_target, StripTargetOptions};
use crate::cargo_front_door;
use crate::core::SoldrError;
use crate::ZccacheSourceArg;
use std::path::{Path, PathBuf};

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
        let code =
            cargo_front_door::run_cargo_front_door(&prepare_args, cache_enabled, zccache_source)
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

    // Phase 2: cook. Heavy — compiles every transitive dep against a stub
    // project. Output lands in `target/`.
    let cook_args = build_chef_cook_args(&ctx, &parsed);
    let cook_result =
        cargo_front_door::run_cargo_front_door(&cook_args, cache_enabled, zccache_source).await;

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

    // Phase 3: post-cook target/ trim (issue #459). Cargo-chef cook
    // leaves cargo-recreatable noise (incremental state, the synthetic
    // stub binary, build-script binaries, large stderr blobs) under
    // target/ that the downstream consumer never reads. Trimming here
    // shrinks the tarball setup-soldr et al ship across CI runners.
    if !parsed.no_trim {
        run_cook_target_trim(&ctx.manifest_dir);
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
