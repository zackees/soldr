//! Git ref & release selection types + resolution (soldr#2310).

use crate::core::SoldrError;

/// A git ref to build source from. `#ref`/native-URL forms are
/// auto-classified; explicit `--branch/--tag/--rev` override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ref {
    /// Default-branch HEAD (`--head`, or bare repo URL with no release).
    Head,
    Branch(String),
    Tag(String),
    /// A commit sha (full or abbreviated hex).
    Rev(String),
}

impl Ref {
    /// The ref token to pass to the GitHub commits API / codeload.
    /// `Head` has no token — the caller substitutes the default branch.
    pub(crate) fn as_api_ref(&self) -> Option<&str> {
        match self {
            Ref::Head => None,
            Ref::Branch(b) => Some(b),
            Ref::Tag(t) => Some(t),
            Ref::Rev(r) => Some(r),
        }
    }

    /// Short human description for the resolution line.
    pub(crate) fn describe(&self) -> String {
        match self {
            Ref::Head => "default-branch HEAD".to_string(),
            Ref::Branch(b) => format!("branch {b}"),
            Ref::Tag(t) => format!("tag {t}"),
            Ref::Rev(r) => format!("commit {r}"),
        }
    }
}

/// Release selection (`--release`, `--release <tag>`, `--release ~N`,
/// or a `/releases/tag/<t>` URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseSel {
    /// Bare `--release` / smart-default: newest published release.
    Latest,
    /// `--release v1.2.3`.
    Tag(String),
    /// `--release ~N`: N releases back from latest (`~1` = previous).
    Offset(u32),
}

impl ReleaseSel {
    /// Parse a `--release` value: `""` → Latest, `~N` → Offset(N),
    /// otherwise a Tag.
    pub(crate) fn parse(value: &str) -> Result<Self, SoldrError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(ReleaseSel::Latest);
        }
        if let Some(n) = value.strip_prefix('~') {
            let n: u32 = n.parse().map_err(|_| {
                SoldrError::Other(format!(
                    "install: invalid --release offset '{value}'; expected ~<N> (e.g. ~1)"
                ))
            })?;
            return Ok(ReleaseSel::Offset(n));
        }
        Ok(ReleaseSel::Tag(value.to_string()))
    }
}

/// Prebuilt-vs-compile decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Form {
    /// Prebuilt-first, fall through to source build (default).
    #[default]
    Auto,
    /// `--prebuilt`: require a matching asset, else error.
    Prebuilt,
    /// `--build`: force a source compile.
    Build,
}

/// Auto-classify a bare ref token (`#ref`, `/tree/<r>`): a 7–40 char hex
/// string is a commit sha; anything else is a branch. (A tag would be
/// distinguished by a network lookup, which the caller does when it
/// resolves the ref; the offline heuristic never mislabels a hex sha.)
pub(crate) fn auto_classify_ref(token: &str) -> Ref {
    if looks_like_sha(token) {
        Ref::Rev(token.to_string())
    } else {
        Ref::Branch(token.to_string())
    }
}

fn looks_like_sha(token: &str) -> bool {
    let len = token.len();
    (7..=40).contains(&len) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// codeload zip URL for a resolved sha (immutable, content-addressed).
pub(crate) fn codeload_zip_url_for_sha(owner: &str, repo: &str, sha: &str) -> String {
    format!("https://codeload.github.com/{owner}/{repo}/zip/{sha}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_autoclass_hex_is_rev() {
        assert_eq!(
            auto_classify_ref("9f2c1ab"),
            Ref::Rev("9f2c1ab".to_string())
        );
        assert_eq!(
            auto_classify_ref("9f2c1ab3d4e5f60718293a4b5c6d7e8f90123456"),
            Ref::Rev("9f2c1ab3d4e5f60718293a4b5c6d7e8f90123456".to_string())
        );
    }

    #[test]
    fn ref_autoclass_else_is_branch() {
        // "dev" is too short to be a sha and non-hex-only words are branches.
        assert_eq!(auto_classify_ref("dev"), Ref::Branch("dev".to_string()));
        assert_eq!(auto_classify_ref("main"), Ref::Branch("main".to_string()));
        // A 7+ char word with non-hex chars is a branch, not a rev.
        assert_eq!(
            auto_classify_ref("feature"),
            Ref::Branch("feature".to_string())
        );
    }

    #[test]
    fn release_offset_parses_tilde_n() {
        assert_eq!(ReleaseSel::parse("~1").unwrap(), ReleaseSel::Offset(1));
        assert_eq!(ReleaseSel::parse("~3").unwrap(), ReleaseSel::Offset(3));
        assert_eq!(ReleaseSel::parse("").unwrap(), ReleaseSel::Latest);
        assert_eq!(
            ReleaseSel::parse("v1.2.3").unwrap(),
            ReleaseSel::Tag("v1.2.3".to_string())
        );
        assert!(ReleaseSel::parse("~notanum").is_err());
    }
}
