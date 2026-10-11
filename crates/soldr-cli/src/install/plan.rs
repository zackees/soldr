//! Resolved-install data types, acquisition planning, and the
//! green/yellow resolution line (soldr#2310).

use std::path::PathBuf;

use super::refs::{Form, Ref, ReleaseSel};
use super::target::InstallTarget;
use crate::color_choice::{paint, GREEN, YELLOW};

/// Fully-resolved inputs (network already consulted for sha/release),
/// ready to acquire + build. Never mutated after construction.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedInstall {
    /// Tool name (from URL owner/repo, or the crate's bin name).
    pub name: String,
    pub target: InstallTarget,
    /// The ref actually chosen (after flags / URL / default logic).
    pub git_ref: Ref,
    /// Resolved immutable commit sha — the cache + pin key. Empty for a
    /// local-path install (no ref).
    pub sha: String,
    /// Release chosen, if this install came from a release selector.
    pub release: Option<ReleaseSel>,
    /// Human note about the release outcome for the resolution line
    /// (e.g. "latest release v0.3.1" or "no release found").
    pub release_note: Option<String>,
    pub form: Form,
    /// Resolved host triple (`--target`, else detected host).
    pub triple: String,
    pub debug: bool,
    pub bins: Vec<String>,
    pub features: Vec<String>,
    pub locked: bool,
    /// PATH bin dir: `--root` or `<paths.bin>/installed`.
    pub install_root: PathBuf,
}

/// How the source/binary will be obtained — the chosen lane (§4).
#[derive(Debug, Clone)]
pub(crate) enum AcquisitionPlan {
    /// Extract a local path directly (no network).
    LocalPath(PathBuf),
    /// `codeload.github.com/<o>/<r>/zip/<ref>` → stream + extract → build.
    CodeloadZip {
        url: String,
        approx_bytes: Option<u64>,
    },
    /// `git clone --depth 1` into the source cache → build.
    ShallowClone { clone_url: String },
    /// Prebuilt release asset (skips the compiler, soldr#3697).
    ReleaseAsset {
        url: String,
        asset_name: String,
        bytes: u64,
        /// GitHub's published asset digest, enforced as a hard pin.
        sha256: Option<String>,
    },
}

/// The green/yellow resolution block, as one string per line.
///
/// Pure so the soldr#3437 agreement test can assert it without capturing
/// stderr — the same split as `log_summary::summary_message`,
/// `cache_states::cache_stats_message` and `line_endings::crlf_warning_message`.
/// [`render_resolution_line`] prints exactly these.
pub(crate) fn resolution_lines(
    resolved: &ResolvedInstall,
    plan: &AcquisitionPlan,
    color: bool,
) -> Vec<String> {
    let mut lines = Vec::new();

    // Header: `install <name> ← <origin>`
    let origin = match &resolved.target {
        InstallTarget::GitHub {
            host, owner, repo, ..
        } => format!("{host}/{owner}/{repo}"),
        InstallTarget::Local(path) => path.display().to_string(),
    };
    lines.push(format!(
        "soldr: install {} \u{2190} {origin}",
        resolved.name
    ));

    // Ref line (skipped for local installs, which have no ref).
    if !matches!(resolved.target, InstallTarget::Local(_)) {
        let ref_desc = resolved
            .release_note
            .clone()
            .unwrap_or_else(|| resolved.git_ref.describe());
        if resolved.sha.is_empty() {
            lines.push(format!("soldr:   ref     {ref_desc}"));
        } else {
            let short = short_sha(&resolved.sha);
            lines.push(format!("soldr:   ref     {ref_desc}  (\u{2192} {short})"));
        }
    }

    // Source line: green when prebuilt (skips compile), yellow when it builds.
    let source_desc = match plan {
        AcquisitionPlan::ReleaseAsset {
            asset_name, bytes, ..
        } => paint(
            &format!(
                "prebuilt  {asset_name}  {}  \u{2190} skips compile",
                human_size(*bytes)
            ),
            GREEN,
            color,
        ),
        AcquisitionPlan::LocalPath(path) => paint(
            &format!(
                "local path {}  \u{2192} build \u{00b7} warm cache",
                path.display()
            ),
            YELLOW,
            color,
        ),
        AcquisitionPlan::CodeloadZip { approx_bytes, .. } => {
            let size = approx_bytes.map(human_size).unwrap_or_default();
            let body = if size.is_empty() {
                "codeload zip (raw source)  \u{2192} build \u{00b7} warm cache".to_string()
            } else {
                format!("codeload zip (raw source)  {size}  \u{2192} build \u{00b7} warm cache")
            };
            paint(&body, YELLOW, color)
        }
        AcquisitionPlan::ShallowClone { .. } => paint(
            "git clone --depth 1  \u{2192} build \u{00b7} warm cache",
            YELLOW,
            color,
        ),
    };
    lines.push(format!("soldr:   source  {source_desc}"));
    lines
}

/// Render the green/yellow resolution block to stderr. This is also the
/// entire output of `--dry-run`.
///
/// The color decision is the canonical soldr#3437 rule — this surface used to
/// own a seventh copy of it that read `GITHUB_ACTIONS` raw, so
/// `GITHUB_ACTIONS=false` still colored the block.
pub(crate) fn render_resolution_line(resolved: &ResolvedInstall, plan: &AcquisitionPlan) {
    for line in resolution_lines(resolved, plan, crate::color_choice::stderr_enabled()) {
        eprintln!("{line}");
    }
}

pub(crate) fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_sha_is_first_seven() {
        assert_eq!(short_sha("9f2c1ab3d4e5"), "9f2c1ab");
        assert_eq!(short_sha("abc"), "abc");
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(6_400_000), "6.1 MB");
    }
}
