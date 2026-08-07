//! Resolved-install data types, acquisition planning, and the
//! green/yellow resolution line (soldr#2310).

use std::path::PathBuf;

use super::refs::{Form, Ref, ReleaseSel};
use super::target::InstallTarget;

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
    /// Phase 2: prebuilt release asset (skips the compiler).
    #[allow(dead_code)]
    ReleaseAsset {
        url: String,
        asset_name: String,
        bytes: u64,
    },
}

const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Colorize when stderr can render ANSI and `NO_COLOR` is unset. Mirrors
/// the `cache_states` convention (on under GitHub Actions too).
pub(crate) fn use_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none()
        && (std::io::stderr().is_terminal() || std::env::var_os("GITHUB_ACTIONS").is_some())
}

fn paint(text: &str, color: &str, use_color: bool) -> String {
    if use_color {
        format!("{color}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Paint `text` yellow (used for warnings outside this module).
pub(crate) fn paint_yellow(text: &str, use_color: bool) -> String {
    paint(text, YELLOW, use_color)
}

/// Render the green/yellow resolution block to stderr. This is also the
/// entire output of `--dry-run`.
pub(crate) fn render_resolution_line(resolved: &ResolvedInstall, plan: &AcquisitionPlan) {
    let color = use_color();

    // Header: `install <name> ← <origin>`
    let origin = match &resolved.target {
        InstallTarget::GitHub {
            host, owner, repo, ..
        } => format!("{host}/{owner}/{repo}"),
        InstallTarget::Local(path) => path.display().to_string(),
    };
    eprintln!("soldr: install {} \u{2190} {origin}", resolved.name);

    // Ref line (skipped for local installs, which have no ref).
    if !matches!(resolved.target, InstallTarget::Local(_)) {
        let ref_desc = resolved
            .release_note
            .clone()
            .unwrap_or_else(|| resolved.git_ref.describe());
        if resolved.sha.is_empty() {
            eprintln!("soldr:   ref     {ref_desc}");
        } else {
            let short = short_sha(&resolved.sha);
            eprintln!("soldr:   ref     {ref_desc}  (\u{2192} {short})");
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
    eprintln!("soldr:   source  {source_desc}");
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

    #[test] // allow-bare-test: soldr#2310 install unit test (sync+fast); timed_test! migration is a follow-up
    fn short_sha_is_first_seven() {
        assert_eq!(short_sha("9f2c1ab3d4e5"), "9f2c1ab");
        assert_eq!(short_sha("abc"), "abc");
    }

    #[test] // allow-bare-test: soldr#2310 install unit test (sync+fast); timed_test! migration is a follow-up
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(6_400_000), "6.1 MB");
    }
}
