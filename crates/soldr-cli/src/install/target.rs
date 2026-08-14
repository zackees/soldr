//! Parse & classify the `soldr install` TARGET argument (soldr#2310).
//!
//! The scheme decides the type — there is deliberately no `owner/repo`
//! slug form (it is indistinguishable from a relative path):
//!
//! | Target starts with                       | Type          |
//! |------------------------------------------|---------------|
//! | `https://` / `http://` / `git@` / `ssh://` | remote git    |
//! | anything else (`.`, `/`, `./`, `~`, …)   | local path    |
//!
//! `#ref` fragments and native GitHub URL path forms (`/tree/<r>`,
//! `/commit/<sha>`, `/releases/tag/<t>`, `/actions/runs/<id>`) are lifted
//! from the TARGET string here so they need no clap flags.

use std::path::PathBuf;

use crate::core::SoldrError;

use super::refs::{auto_classify_ref, ReleaseSel};

/// What the user asked to install, after scheme classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallTarget {
    /// Anything that is not a URL: `.`, `./crates/foo`, `/abs`, `~/x`.
    Local(PathBuf),
    /// A remote GitHub repo. `host` is normally `github.com`.
    GitHub {
        host: String,
        owner: String,
        repo: String,
        /// A ref lifted from the URL itself (`#ref`, `/tree/<r>`,
        /// `/commit/<sha>`) or `None` when the URL named only the repo.
        url_ref: Option<super::refs::Ref>,
        /// A release lifted from a `/releases/tag/<t>` URL, if present.
        url_release: Option<ReleaseSel>,
        /// An `/actions/runs/<id>` artifact target (Phase 3).
        run_id: Option<u64>,
    },
}

impl InstallTarget {
    /// The tool name: the repo name for GitHub targets, or the final
    /// path component for local targets (the crate's bin name is
    /// re-derived at build time when available).
    pub(crate) fn inferred_name(&self) -> Option<String> {
        match self {
            InstallTarget::GitHub { repo, .. } => Some(repo.clone()),
            InstallTarget::Local(path) => path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| *n != "." && *n != "..")
                .map(str::to_string),
        }
    }
}

/// True when `target` uses a remote-git scheme.
fn is_remote_url(target: &str) -> bool {
    target.starts_with("https://")
        || target.starts_with("http://")
        || target.starts_with("git@")
        || target.starts_with("ssh://")
}

/// Classify a raw TARGET string into an [`InstallTarget`].
pub(crate) fn classify(target: &str) -> Result<InstallTarget, SoldrError> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err(SoldrError::Other(
            "install: empty TARGET; expected a GitHub URL or a local path".to_string(),
        ));
    }
    if is_remote_url(trimmed) {
        parse_remote(trimmed)
    } else {
        Ok(InstallTarget::Local(PathBuf::from(trimmed)))
    }
}

/// Parse a remote-git URL into a [`InstallTarget::GitHub`].
fn parse_remote(url: &str) -> Result<InstallTarget, SoldrError> {
    // Split off a `#ref` fragment first — it applies to any URL form.
    let (base, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };

    let (host, path) = split_host_and_path(base)?;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err(SoldrError::Other(format!(
            "install: could not parse owner/repo from URL: {url}"
        )));
    }
    let owner = segments[0].to_string();
    let repo = strip_git_suffix(segments[1]).to_string();

    let mut url_ref = None;
    let mut url_release = None;
    let mut run_id = None;

    // Native GitHub path forms come after owner/repo.
    match segments.get(2).copied() {
        Some("tree") | Some("blob") => {
            if let Some(r) = segments.get(3) {
                url_ref = Some(auto_classify_ref(r));
            }
        }
        Some("commit") => {
            if let Some(sha) = segments.get(3) {
                url_ref = Some(super::refs::Ref::Rev((*sha).to_string()));
            }
        }
        // `/releases/tag/<t>`
        Some("releases") if segments.get(3).copied() == Some("tag") => {
            if let Some(t) = segments.get(4) {
                url_release = Some(ReleaseSel::Tag((*t).to_string()));
            }
        }
        // `/actions/runs/<id>`
        Some("actions") if segments.get(3).copied() == Some("runs") => {
            if let Some(id) = segments.get(4) {
                run_id = id.parse::<u64>().ok();
            }
        }
        _ => {}
    }

    // A `#ref` fragment overrides any URL-path ref (but not a release URL).
    if let Some(f) = fragment {
        if !f.is_empty() {
            url_ref = Some(auto_classify_ref(f));
        }
    }

    Ok(InstallTarget::GitHub {
        host,
        owner,
        repo,
        url_ref,
        url_release,
        run_id,
    })
}

/// Split a remote URL into `(host, path)` regardless of scheme flavor.
fn split_host_and_path(base: &str) -> Result<(String, String), SoldrError> {
    // scp-like: `git@github.com:owner/repo(.git)`
    if let Some(rest) = base.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return Ok((host.to_string(), path.to_string()));
        }
        return Err(SoldrError::Other(format!(
            "install: malformed git@ URL: {base}"
        )));
    }
    // scheme://host/path forms (https, http, ssh)
    let after_scheme = base.split_once("://").map(|(_, rest)| rest).unwrap_or(base);
    // Drop any `user@` in an `ssh://git@host/...` authority.
    let authority_and_path = after_scheme;
    let (authority, path) = match authority_and_path.split_once('/') {
        Some((a, p)) => (a, p),
        None => (authority_and_path, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    // Strip a `:port` if present.
    let host = host.split(':').next().unwrap_or(host);
    Ok((host.to_string(), path.to_string()))
}

fn strip_git_suffix(repo: &str) -> &str {
    repo.strip_suffix(".git").unwrap_or(repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::refs::Ref;

    fn github(t: &str) -> InstallTarget {
        classify(t).unwrap()
    }

    #[test]
    fn install_target_url_is_remote_git() {
        match github("https://github.com/zackees/clud") {
            InstallTarget::GitHub {
                host, owner, repo, ..
            } => {
                assert_eq!(host, "github.com");
                assert_eq!(owner, "zackees");
                assert_eq!(repo, "clud");
            }
            other => panic!("expected GitHub, got {other:?}"),
        }
    }

    #[test]
    fn install_target_slug_is_local_path_not_github() {
        // RED rule: `zackees/clud` (no scheme) is a LOCAL PATH, not GitHub.
        assert_eq!(
            github("zackees/clud"),
            InstallTarget::Local(PathBuf::from("zackees/clud"))
        );
    }

    #[test]
    fn install_target_dot_and_relative_and_abs_are_local() {
        for t in [".", "./crates/foo", "/abs/path", "~/x"] {
            assert!(
                matches!(classify(t).unwrap(), InstallTarget::Local(_)),
                "{t} must be Local"
            );
        }
    }

    #[test]
    fn install_target_git_at_and_ssh_are_remote() {
        match github("git@github.com:zackees/clud.git") {
            InstallTarget::GitHub {
                host, owner, repo, ..
            } => {
                assert_eq!(host, "github.com");
                assert_eq!(owner, "zackees");
                assert_eq!(repo, "clud"); // .git stripped
            }
            other => panic!("expected GitHub, got {other:?}"),
        }
        match github("ssh://git@github.com/zackees/clud.git") {
            InstallTarget::GitHub {
                host, owner, repo, ..
            } => {
                assert_eq!(host, "github.com");
                assert_eq!(owner, "zackees");
                assert_eq!(repo, "clud");
            }
            other => panic!("expected GitHub, got {other:?}"),
        }
    }

    #[test]
    fn install_target_native_url_paths_parse() {
        match github("https://github.com/zackees/clud/tree/dev") {
            InstallTarget::GitHub { url_ref, .. } => {
                assert_eq!(url_ref, Some(Ref::Branch("dev".to_string())));
            }
            other => panic!("{other:?}"),
        }
        match github("https://github.com/zackees/clud/commit/9f2c1ab") {
            InstallTarget::GitHub { url_ref, .. } => {
                assert_eq!(url_ref, Some(Ref::Rev("9f2c1ab".to_string())));
            }
            other => panic!("{other:?}"),
        }
        match github("https://github.com/zackees/clud/releases/tag/v0.3.1") {
            InstallTarget::GitHub { url_release, .. } => {
                assert_eq!(url_release, Some(ReleaseSel::Tag("v0.3.1".to_string())));
            }
            other => panic!("{other:?}"),
        }
        match github("https://github.com/zackees/clud/actions/runs/123") {
            InstallTarget::GitHub { run_id, .. } => {
                assert_eq!(run_id, Some(123));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn install_fragment_ref_parses() {
        match github("https://github.com/zackees/clud#dev") {
            InstallTarget::GitHub { url_ref, .. } => {
                assert_eq!(url_ref, Some(Ref::Branch("dev".to_string())));
            }
            other => panic!("{other:?}"),
        }
        match github("https://github.com/zackees/clud#9f2c1ab") {
            InstallTarget::GitHub { url_ref, .. } => {
                assert_eq!(url_ref, Some(Ref::Rev("9f2c1ab".to_string())));
            }
            other => panic!("{other:?}"),
        }
    }
}
