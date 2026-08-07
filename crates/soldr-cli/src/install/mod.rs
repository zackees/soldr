//! `soldr install <github-url|path>` — prebuilt-first tool install
//! (soldr#2310, Phase 1).
//!
//! Analog of `pip install .` / `cargo install --git`: install a Rust tool
//! from a GitHub URL or a local path. Phase 1 implements the source-build
//! lanes (local path, codeload zip, `git clone --depth 1` fallback) that
//! ride soldr's compile cache; prebuilt release assets and run-artifact
//! installs are Phase 2/3.
//!
//! Flow: classify TARGET → resolve ref/release to a commit sha → build the
//! acquisition + resolution plan → (`--dry-run`? print : acquire → build →
//! place on PATH).

pub(crate) mod acquire;
pub(crate) mod cache;
pub(crate) mod place;
pub(crate) mod plan;
pub(crate) mod refs;
pub(crate) mod target;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use plan::{render_resolution_line, ResolvedInstall};
use refs::{Form, Ref, ReleaseSel};
use target::InstallTarget;

/// Parsed `soldr install` arguments. Flattened into the clap
/// `Commands::Install` variant (`#[command(flatten)]`) so the argument
/// surface lives beside the install logic and the hot `cli_args.rs` /
/// dispatch arm stay tiny (soldr#2310 + soldr#1966 LOC ratchet).
#[derive(clap::Args, Debug, Clone)]
pub struct InstallArgs {
    /// Install target: a remote git URL (`https://…`, `git@…`, `ssh://…`)
    /// or a local path (`.`, `./crates/foo`, `/abs`, `~/x`). If it is not
    /// a URL it is treated as a path and errors if absent — never GitHub.
    #[arg(value_name = "TARGET")]
    pub target: String,

    // ---- RELEASE selection (prebuilt-first) ----
    /// Install a release. Bare `--release` = latest; `--release <tag>` =
    /// that release; `--release ~<N>` = N releases back (`~1` = previous).
    /// Distinct from `--tag <t>` (which builds SOURCE at a git tag); the
    /// two are contradictory, so `--release` conflicts with the raw
    /// source-ref group.
    #[arg(
        long,
        value_name = "TAG|~N",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with = "source_ref"
    )]
    pub release: Option<String>,

    // ---- RAW SOURCE ref (build from git; mutually exclusive) ----
    /// Build the default-branch HEAD from source.
    #[arg(long, group = "source_ref")]
    pub head: bool,
    /// Build source at a branch.
    #[arg(long, value_name = "BRANCH", group = "source_ref")]
    pub branch: Option<String>,
    /// Build source at a git tag (NOT a release — see `--release <tag>`).
    #[arg(long, value_name = "TAG", group = "source_ref")]
    pub tag: Option<String>,
    /// Build source at a commit sha.
    #[arg(long, value_name = "SHA", group = "source_ref")]
    pub rev: Option<String>,

    // ---- FORM ----
    /// Force a prebuilt asset; error if none matches the host triple.
    #[arg(long, conflicts_with = "build")]
    pub prebuilt: bool,
    /// Force a source compile (skip prebuilt asset lookup).
    #[arg(long)]
    pub build: bool,

    // ---- BUILD knobs ----
    /// Rare: build an unoptimized/debug binary (default is optimized).
    #[arg(long)]
    pub debug: bool,
    /// Restrict to these binary targets (repeatable). Mirrors `cargo --bin`.
    #[arg(long = "bin", value_name = "NAME")]
    pub bins: Vec<String>,
    /// Cargo features to enable (repeatable / comma-list).
    #[arg(long, value_name = "FEATURES")]
    pub features: Vec<String>,
    /// Target triple to build/install for. Defaults to the host triple.
    #[arg(long, value_name = "TRIPLE")]
    pub target_triple: Option<String>,

    // ---- OTHER ----
    /// Install root override (the PATH bin dir). Defaults to
    /// `~/.soldr/bin/installed/`.
    #[arg(long, value_name = "DIR")]
    pub root: Option<std::path::PathBuf>,
    /// Overwrite an existing install of the same tool.
    #[arg(long)]
    pub force: bool,
    /// Print the resolution plan and exit without fetching/building.
    #[arg(long)]
    pub dry_run: bool,
    /// Record a trust-on-first-use sha256 pin (Phase 3 honors it fully).
    #[arg(long)]
    pub locked: bool,
}

/// Top-level dispatch for `Commands::Install`.
pub(crate) async fn run(args: InstallArgs) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    run_with_paths(args, &paths).await
}

/// Inner entry point that installs into an explicit [`SoldrPaths`], so
/// tests can drive a synthetic root without mutating process env.
pub(crate) async fn run_with_paths(
    args: InstallArgs,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    paths.ensure_dirs()?;

    let resolved = resolve(&args, paths).await?;
    let acquisition = acquire::plan_acquisition(&resolved);

    // The resolution line is the entire output of `--dry-run`.
    render_resolution_line(&resolved, &acquisition);
    if args.dry_run {
        return Ok(());
    }

    // Best-effort: reclaim expired source clones before acquiring more.
    if let Ok(config) = paths.load_config() {
        let _ = cache::sweep_with_config(paths, config.install.source_ttl_days);
    }

    let source_dir = acquire::acquire_source(paths, &resolved, &acquisition).await?;

    let staging = tempfile::tempdir_in(&paths.cache).map_err(|e| {
        SoldrError::Other(format!("install: failed to create build staging dir: {e}"))
    })?;
    let built = acquire::cargo_install_from_path(&source_dir, &resolved, staging.path())?;
    let placement = place::place_binary(
        &resolved.name,
        &built,
        &resolved.install_root,
        &resolved.triple,
        args.force,
    )?;
    drop(staging);

    let pin = if resolved.sha.is_empty() {
        "local".to_string()
    } else {
        resolved.sha.clone()
    };
    eprintln!(
        "soldr: installed {} \u{2192} {}  (sha {pin})",
        resolved.name,
        placement.binary.display()
    );
    Ok(())
}

/// Resolve args (consulting the network for GitHub sha/release) into a
/// fully-materialized [`ResolvedInstall`].
async fn resolve(args: &InstallArgs, paths: &SoldrPaths) -> Result<ResolvedInstall, SoldrError> {
    let target = target::classify(&args.target)?;
    let triple = match &args.target_triple {
        Some(t) => TargetTriple::from_triple(t)?.triple(),
        None => TargetTriple::detect()?.triple(),
    };
    let form = if args.prebuilt {
        Form::Prebuilt
    } else if args.build {
        Form::Build
    } else {
        Form::Auto
    };
    let install_root = place::install_root(paths, args.root.as_deref());

    let base = ResolvedInstall {
        name: String::new(),
        target: target.clone(),
        git_ref: Ref::Head,
        sha: String::new(),
        release: None,
        release_note: None,
        form,
        triple,
        debug: args.debug,
        bins: args.bins.clone(),
        features: args.features.clone(),
        locked: args.locked,
        install_root,
    };

    match &target {
        InstallTarget::Local(path) => {
            let name = local_crate_name(path)
                .or_else(|| target.inferred_name())
                .ok_or_else(|| {
                    SoldrError::Other(format!(
                        "install: could not determine tool name for local path {}",
                        path.display()
                    ))
                })?;
            Ok(ResolvedInstall { name, ..base })
        }
        InstallTarget::GitHub {
            host,
            owner,
            repo,
            url_ref,
            url_release,
            ..
        } => {
            let name = repo.clone();
            let token = crate::fetch::source_zip::github_token_from_env();

            // Explicit ref flag wins over everything.
            let flag_ref = if args.head {
                Some(Ref::Head)
            } else if let Some(b) = &args.branch {
                Some(Ref::Branch(b.clone()))
            } else if let Some(t) = &args.tag {
                Some(Ref::Tag(t.clone()))
            } else {
                args.rev.as_ref().map(|r| Ref::Rev(r.clone()))
            };

            // Release selector: explicit --release wins over a URL release.
            let release_sel = match &args.release {
                Some(v) => Some(ReleaseSel::parse(v)?),
                None => url_release.clone(),
            };

            let is_github = host.eq_ignore_ascii_case("github.com");

            // Precedence: flag ref > release selector > URL ref > smart default.
            let (git_ref, release, release_note) = if let Some(r) = flag_ref {
                (r, None, None)
            } else if let Some(sel) = release_sel {
                let tag =
                    resolve_release_tag(owner, repo, &sel, token.as_deref(), is_github).await?;
                let note = format!("release {tag}");
                (Ref::Tag(tag), Some(sel), Some(note))
            } else if let Some(r) = url_ref {
                (r.clone(), None, None)
            } else if is_github {
                // Smart default: latest release if any, else default-branch HEAD.
                match crate::fetch::install_api::resolve_latest_release_tag(
                    owner,
                    repo,
                    token.as_deref(),
                )
                .await
                {
                    Ok(tag) => {
                        let note = format!("latest release {tag}");
                        (Ref::Tag(tag), Some(ReleaseSel::Latest), Some(note))
                    }
                    Err(_) => (Ref::Head, None, Some("no release found".to_string())),
                }
            } else {
                (Ref::Head, None, None)
            };

            // Resolve the chosen ref to an immutable commit sha (GitHub only).
            let sha = if is_github {
                let api_ref = git_ref.as_api_ref().unwrap_or("HEAD");
                crate::fetch::install_api::resolve_commit_sha(
                    owner,
                    repo,
                    api_ref,
                    token.as_deref(),
                )
                .await?
            } else {
                // Non-GitHub host (Phase 1): no API resolution, key the cache
                // on the ref token so the shallow-clone lane still caches.
                sanitize_ref_key(&git_ref)
            };

            Ok(ResolvedInstall {
                name,
                git_ref,
                sha,
                release,
                release_note,
                ..base
            })
        }
    }
}

async fn resolve_release_tag(
    owner: &str,
    repo: &str,
    sel: &ReleaseSel,
    token: Option<&str>,
    is_github: bool,
) -> Result<String, SoldrError> {
    if !is_github {
        return Err(SoldrError::Other(
            "install: --release is only supported for github.com targets in Phase 1".to_string(),
        ));
    }
    match sel {
        ReleaseSel::Latest => {
            crate::fetch::install_api::resolve_latest_release_tag(owner, repo, token).await
        }
        ReleaseSel::Tag(t) => Ok(t.clone()),
        ReleaseSel::Offset(n) => {
            crate::fetch::install_api::resolve_release_at_offset(owner, repo, *n, token).await
        }
    }
}

/// A filesystem-safe cache key for a non-resolved ref (non-GitHub hosts).
fn sanitize_ref_key(git_ref: &Ref) -> String {
    let raw = match git_ref {
        Ref::Head => "HEAD".to_string(),
        Ref::Branch(b) => format!("branch-{b}"),
        Ref::Tag(t) => format!("tag-{t}"),
        Ref::Rev(r) => r.clone(),
    };
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Read `[package] name` from a local crate's `Cargo.toml`, if present.
fn local_crate_name(path: &std::path::Path) -> Option<String> {
    let manifest = path.join("Cargo.toml");
    let text = std::fs::read_to_string(manifest).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests;
