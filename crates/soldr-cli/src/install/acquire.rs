//! Acquisition ladder + source build for `soldr install` (soldr#2310).
//!
//! Given a fully-[`ResolvedInstall`], choose the fastest viable lane
//! (§4 of the issue) and materialize an on-disk source tree, then build
//! it with `cargo install --path` through soldr's compile-cache wrapper.

use std::path::{Path, PathBuf};

use crate::binaries::resolve_toolchain_binary;
use crate::build_from_source_cmd::apply_source_build_cache_wrapper;
use crate::core::{
    suppress_windows_console_window, InstallerWatchdogConfig, SoldrError, SoldrPaths,
};

use super::cache;
use super::place::binary_ext_for_triple;
use super::plan::{AcquisitionPlan, ResolvedInstall};
use super::refs::codeload_zip_url_for_sha;
use super::target::InstallTarget;

pub(crate) const INSTALL_TIMEOUT_ENV_VAR: &str = "SOLDR_INSTALL_BUILD_TIMEOUT_SECS";

/// Choose the acquisition lane. Pure — the sha/release resolution has
/// already happened in [`super::resolve`].
pub(crate) fn plan_acquisition(resolved: &ResolvedInstall) -> AcquisitionPlan {
    match &resolved.target {
        InstallTarget::Local(path) => AcquisitionPlan::LocalPath(path.clone()),
        InstallTarget::GitHub {
            host, owner, repo, ..
        } => {
            // Phase 1: GitHub → codeload zip (by resolved sha). Non-GitHub
            // hosts fall back to a shallow clone.
            if host.eq_ignore_ascii_case("github.com") && !resolved.sha.is_empty() {
                AcquisitionPlan::CodeloadZip {
                    url: codeload_zip_url_for_sha(owner, repo, &resolved.sha),
                    approx_bytes: None,
                }
            } else {
                AcquisitionPlan::ShallowClone {
                    clone_url: format!("https://{host}/{owner}/{repo}.git"),
                }
            }
        }
    }
}

/// Acquire the source tree for `plan`, returning the directory that holds
/// the crate's `Cargo.toml` (ready for `cargo install --path`).
pub(crate) async fn acquire_source(
    paths: &SoldrPaths,
    resolved: &ResolvedInstall,
    plan: &AcquisitionPlan,
) -> Result<PathBuf, SoldrError> {
    match plan {
        AcquisitionPlan::LocalPath(path) => {
            let canonical = path.canonicalize().map_err(|e| {
                SoldrError::Other(format!(
                    "install: local path {} is not accessible: {e}",
                    path.display()
                ))
            })?;
            Ok(canonical)
        }
        AcquisitionPlan::CodeloadZip { url, .. } => {
            acquire_codeload_zip(paths, resolved, url).await
        }
        AcquisitionPlan::ShallowClone { clone_url } => {
            acquire_shallow_clone(paths, resolved, clone_url)
        }
        AcquisitionPlan::ReleaseAsset { .. } => Err(SoldrError::Other(
            "install: prebuilt release-asset acquisition is Phase 2 (not yet implemented)"
                .to_string(),
        )),
    }
}

fn cache_dir_for(paths: &SoldrPaths, resolved: &ResolvedInstall) -> Option<PathBuf> {
    match &resolved.target {
        InstallTarget::GitHub {
            host, owner, repo, ..
        } => Some(cache::source_cache_dir(
            paths,
            host,
            owner,
            repo,
            &resolved.sha,
        )),
        InstallTarget::Local(_) => None,
    }
}

async fn acquire_codeload_zip(
    paths: &SoldrPaths,
    resolved: &ResolvedInstall,
    url: &str,
) -> Result<PathBuf, SoldrError> {
    let cache_dir = cache_dir_for(paths, resolved)
        .ok_or_else(|| SoldrError::Other("install: codeload requires a GitHub target".into()))?;

    // Cache hit: a completed, content-addressed extraction is immutable.
    if cache::is_complete(&cache_dir) {
        cache::touch_last_use(&cache_dir);
        return single_crate_root(&cache_dir);
    }

    // Fresh acquisition: mark in-flight, extract, then publish.
    // Clear any stale partial dir first.
    if cache_dir.exists() {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
    cache::mark_partial(&cache_dir)?;

    let token = crate::fetch::source_zip::github_token_from_env();
    let extracted =
        crate::fetch::source_zip::stream_and_extract_source_zip(url, &cache_dir, token.as_deref())
            .await?;

    cache::clear_partial(&cache_dir)?;
    cache::touch_last_use(&cache_dir);
    Ok(extracted.root)
}

fn acquire_shallow_clone(
    paths: &SoldrPaths,
    resolved: &ResolvedInstall,
    clone_url: &str,
) -> Result<PathBuf, SoldrError> {
    let cache_dir = cache_dir_for(paths, resolved)
        .ok_or_else(|| SoldrError::Other("install: clone requires a remote target".into()))?;

    if cache::is_complete(&cache_dir) {
        cache::touch_last_use(&cache_dir);
        return single_crate_root(&cache_dir);
    }
    if cache_dir.exists() {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
    cache::mark_partial(&cache_dir)?;
    std::fs::create_dir_all(&cache_dir)?;

    let checkout = cache_dir.join("checkout");
    let mut command = std::process::Command::new("git");
    command.arg("clone").arg("--depth").arg("1");
    if let Some(git_ref) = resolved.git_ref.as_api_ref() {
        command.arg("--branch").arg(git_ref);
    }
    command.arg(clone_url).arg(&checkout);
    suppress_windows_console_window(&mut command);

    let status = command
        .status()
        .map_err(|e| SoldrError::Other(format!("install: failed to spawn git clone: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Err(SoldrError::Other(format!(
            "install: git clone {clone_url} failed with status {status}"
        )));
    }

    cache::clear_partial(&cache_dir)?;
    cache::touch_last_use(&cache_dir);
    Ok(checkout)
}

/// A codeload extraction yields a single `repo-<sha>/` dir; a resumed
/// cache dir needs that dir re-discovered.
fn single_crate_root(cache_dir: &Path) -> Result<PathBuf, SoldrError> {
    // Prefer a `checkout` subdir (shallow clone), else the single wrapped
    // codeload dir, else the cache dir itself if it holds a Cargo.toml.
    let checkout = cache_dir.join("checkout");
    if checkout.join("Cargo.toml").is_file() {
        return Ok(checkout);
    }
    if cache_dir.join("Cargo.toml").is_file() {
        return Ok(cache_dir.to_path_buf());
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.file_name() != ".last-use" {
            dirs.push(entry.path());
        }
    }
    dirs.retain(|d| d.file_name().map(|n| n != "checkout").unwrap_or(true));
    match dirs.len() {
        1 => Ok(dirs.remove(0)),
        _ => Err(SoldrError::Other(format!(
            "install: could not locate crate root in cached source at {}",
            cache_dir.display()
        ))),
    }
}

/// Build `source_dir` with `cargo install --path` into a staging root and
/// return the produced binary path. Routes rustc through soldr's
/// compile-cache wrapper (like `build-from-source`).
pub(crate) fn cargo_install_from_path(
    source_dir: &Path,
    resolved: &ResolvedInstall,
    staging_root: &Path,
) -> Result<PathBuf, SoldrError> {
    let cargo = resolve_toolchain_binary("cargo")?;
    std::fs::create_dir_all(staging_root)?;
    let staging_bin = staging_root.join("bin");

    let mut command = std::process::Command::new(&cargo);
    command
        .arg("install")
        .arg("--path")
        .arg(source_dir)
        .arg("--root")
        .arg(staging_root)
        .arg("--force");
    if resolved.locked {
        command.arg("--locked");
    }
    if resolved.debug {
        command.arg("--debug");
    }
    for bin in &resolved.bins {
        command.arg("--bin").arg(bin);
    }
    if !resolved.features.is_empty() {
        command.arg("--features").arg(resolved.features.join(","));
    }
    command
        .arg("--target")
        .arg(&resolved.triple)
        // Neutral working dir + scrub inherited wrappers, exactly like
        // build-from-source, before opting back into the cache wrapper.
        .current_dir(source_dir)
        .env_remove("MAKEFLAGS")
        .env_remove("CARGO_MAKEFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    apply_source_build_cache_wrapper(&mut command);
    suppress_windows_console_window(&mut command);

    let status = crate::exit_guard::run_child_command(
        &mut command,
        &format!("install: cargo install --path {}", source_dir.display()),
        "install",
        InstallerWatchdogConfig::from_env(INSTALL_TIMEOUT_ENV_VAR),
    )?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "install: cargo install --path {} failed with status {status}",
            source_dir.display()
        )));
    }

    // Find the produced binary. When `--bin` narrowed it, prefer that name;
    // otherwise take the tool's inferred name, else the first binary present.
    let ext = binary_ext_for_triple(&resolved.triple);
    let mut candidates: Vec<String> = Vec::new();
    if let Some(first) = resolved.bins.first() {
        candidates.push(format!("{first}{ext}"));
    }
    candidates.push(format!("{}{ext}", resolved.name));
    for name in &candidates {
        let p = staging_bin.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    // Fall back to the single binary cargo produced.
    let produced: Vec<PathBuf> = std::fs::read_dir(&staging_bin)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    match produced.len() {
        1 => Ok(produced.into_iter().next().unwrap()),
        0 => Err(SoldrError::Other(format!(
            "install: cargo install produced no binary in {}",
            staging_bin.display()
        ))),
        _ => Err(SoldrError::Other(format!(
            "install: cargo install produced multiple binaries in {}; pass --bin <name>",
            staging_bin.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::refs::{Form, Ref};

    fn resolved_local(path: &str) -> ResolvedInstall {
        ResolvedInstall {
            name: "foo".into(),
            target: InstallTarget::Local(PathBuf::from(path)),
            git_ref: Ref::Head,
            sha: String::new(),
            release: None,
            release_note: None,
            form: Form::Auto,
            triple: "x86_64-unknown-linux-gnu".into(),
            debug: false,
            bins: vec![],
            features: vec![],
            locked: false,
            install_root: PathBuf::from("/tmp/installed"),
        }
    }

    #[test]
    fn plan_local_path_is_local_lane() {
        let r = resolved_local(".");
        assert!(matches!(
            plan_acquisition(&r),
            AcquisitionPlan::LocalPath(_)
        ));
    }

    #[test]
    fn plan_github_with_sha_is_codeload() {
        let mut r = resolved_local(".");
        r.target = InstallTarget::GitHub {
            host: "github.com".into(),
            owner: "zackees".into(),
            repo: "clud".into(),
            url_ref: None,
            url_release: None,
            run_id: None,
        };
        r.sha = "9f2c1ab3".into();
        match plan_acquisition(&r) {
            AcquisitionPlan::CodeloadZip { url, .. } => {
                assert!(
                    url.contains("codeload.github.com/zackees/clud/zip/9f2c1ab3"),
                    "{url}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_non_github_is_shallow_clone() {
        let mut r = resolved_local(".");
        r.target = InstallTarget::GitHub {
            host: "gitlab.com".into(),
            owner: "o".into(),
            repo: "r".into(),
            url_ref: None,
            url_release: None,
            run_id: None,
        };
        r.sha = "abc".into();
        assert!(matches!(
            plan_acquisition(&r),
            AcquisitionPlan::ShallowClone { .. }
        ));
    }
}
