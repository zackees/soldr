//! `soldr cook` — cargo-chef-style content-addressable dep prebuild.
//!
//! This is a thin shim that:
//! 1. Validates that `Cargo.toml` (and ideally `Cargo.lock`) live in the
//!    cwd, so cargo-chef has something to read.
//! 2. Resolves the pinned `cargo-chef` binary via the standard fetch
//!    pipeline (registry entry in `soldr_fetch::known_tools`).
//! 3. Routes `cargo chef prepare` and `cargo chef cook` through the
//!    existing cargo front door so the underlying compile picks up
//!    zccache (RUSTC_WRAPPER), `ZCCACHE_PATH_REMAP=auto`, the soldr
//!    linker selection, the soldr-managed CARGO_HOME / RUSTUP_HOME, and
//!    every other piece of `soldr cargo` plumbing.
//!
//! See issue zackees/soldr#359 for design context and the companion
//! `zackees/setup-soldr#110` for the GitHub Action that consumes the
//! resulting `target/` tarball.

use crate::cargo_front_door;
use crate::ZccacheSourceArg;
use soldr_core::SoldrError;
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
        if parsed.prepare_only {
            eprintln!(
                "soldr cook: wrote recipe to {} (--prepare-only)",
                ctx.recipe_path.display()
            );
            return Ok(0);
        }
    }

    // Phase 2: cook. Heavy — compiles every transitive dep against a stub
    // project. Output lands in `target/` next to the (untouched) project
    // source code.
    let cook_args = build_chef_cook_args(&ctx, &parsed);
    let code =
        cargo_front_door::run_cargo_front_door(&cook_args, cache_enabled, zccache_source).await?;
    if code != 0 {
        return Ok(code);
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
