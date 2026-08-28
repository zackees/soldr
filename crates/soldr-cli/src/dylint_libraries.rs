//! The one reader of "which nightly do this workspace's Dylint libraries
//! declare" (soldr#2945).
//!
//! # Why this module exists
//!
//! Dylint builds **one driver per lint-library toolchain**. The authority for
//! the nightly a lint run needs is therefore each library's own
//! `rust-toolchain.toml` — not the workspace's stable channel, and not
//! anything derived from it. Soldr had three answers to that question, two of
//! which were wrong:
//!
//! * `dylint_toolchain.rs` *derived* a nightly from the workspace root's
//!   **stable** channel through `rust-nightly-versions.v1.json`. In this repo
//!   that produced `nightly-2026-02-28`, for which no Dylint driver has ever
//!   been published — so `soldr dylint` failed at the driver gate while the
//!   six lint manifests all pinned `nightly-2026-05-28`, whose driver was
//!   already on disk.
//! * `ci_test/plan.rs` read the libraries directly, off a hard-coded list of
//!   six directory names, and got the right answer.
//! * `dylint_cook.rs` read `workspace.metadata.dylint.libraries` but joined
//!   each entry's `path` **literally**. This workspace declares
//!   `{ path = "dylints/*" }`, so it looked for
//!   `<root>/dylints/*/rust-toolchain.toml`, found no such file, took the
//!   "missing manifest is not an error" default, and silently fell back to the
//!   root *stable* channel. That is why its conflict detection had never once
//!   been reachable.
//!
//! Expanding the glob is the piece all three were missing, which is why this
//! is one module rather than three copies of a loop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::core::{read_rust_toolchain_manifest, SoldrError};

/// The single nightly every Dylint library in a workspace agrees on, plus the
/// directories that declared it (for diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LibraryToolchain {
    /// The channel exactly as the libraries spell it — the string is handed
    /// straight to rustup and to the driver-asset lookup, so it is never
    /// normalized here.
    pub(crate) channel: String,
    /// Workspace-relative library directories, `/`-separated and sorted.
    pub(crate) libraries: Vec<String>,
}

/// Reduce a channel to the identity the Dylint driver is keyed on: a dated
/// nightly without its host-triple suffix. `nightly-2026-05-28-x86_64-pc-
/// windows-msvc` and `nightly-2026-05-28` name the same driver; anything that
/// is not a nightly (a stable `1.95.0`, say) is returned unchanged.
pub(crate) fn canonical_channel(channel: &str) -> &str {
    channel
        .get(..18)
        .filter(|prefix| prefix.starts_with("nightly-"))
        .unwrap_or(channel)
}

/// The nightly the workspace's Dylint libraries declare, or `None` when they
/// declare none.
///
/// `None` covers two states that are the same as far as a caller is concerned,
/// and neither is an error:
///
/// * the workspace lists no Dylint libraries (a consumer repo with no lints);
/// * every listed library omits its own `rust-toolchain.toml`, so each one
///   *inherits* the workspace root's — the root is then already the right
///   answer and the caller's next tier reads it.
///
/// Errors are reserved for the states a caller cannot paper over: a declared
/// library that is not on disk, libraries pinned to different nightlies (only
/// one of which could have a driver), and a workspace where some libraries pin
/// a nightly while others inherit a different root channel.
pub(crate) fn pinned_channel(
    workspace_root: &Path,
) -> Result<Option<LibraryToolchain>, SoldrError> {
    let mut pinned: Vec<(String, String)> = Vec::new();
    let mut inheriting: Vec<String> = Vec::new();
    for directory in library_directories(workspace_root)? {
        let relative = relative_display(workspace_root, &directory);
        // Only a literal (unglobbed) entry can reach here missing; glob
        // expansion only ever yields directories that exist. Saying so is
        // worth an error of its own — cargo-dylint's own answer to a library
        // that is not there is "No libraries were found" and exit 0.
        if !directory.is_dir() {
            return Err(SoldrError::Other(format!(
                "declared Dylint library `{relative}` does not exist: {} is not a directory. \
                 Fix or remove its entry under workspace.metadata.dylint.libraries \
                 (soldr#2945).",
                directory.display()
            )));
        }
        match read_rust_toolchain_manifest(&directory)?.channel {
            Some(channel) => pinned.push((relative, channel)),
            None => inheriting.push(relative),
        }
    }
    if pinned.is_empty() {
        return Ok(None);
    }
    if !inheriting.is_empty() {
        return Err(SoldrError::Other(format!(
            "Dylint libraries disagree about their toolchain: {} pin a nightly while {} declare \
             no rust-toolchain.toml and inherit the workspace root's channel. Dylint builds one \
             driver per library toolchain (soldr#2945) and Soldr fetches exactly one driver, so \
             either pin every library or none of them.",
            describe(&pinned),
            inheriting.join(", ")
        )));
    }
    let distinct: BTreeSet<&str> = pinned
        .iter()
        .map(|(_, channel)| canonical_channel(channel))
        .collect();
    if distinct.len() > 1 {
        return Err(SoldrError::Other(format!(
            "conflicting Dylint library toolchain pins: {}. Soldr fetches exactly one prebuilt \
             driver per run and Dylint builds one driver per library toolchain (soldr#2945), so \
             at most one of these nightlies could have a usable driver. Pin every library under \
             workspace.metadata.dylint.libraries to the same nightly.",
            describe(&pinned)
        )));
    }
    let channel = pinned[0].1.clone();
    Ok(Some(LibraryToolchain {
        channel,
        libraries: pinned.into_iter().map(|(directory, _)| directory).collect(),
    }))
}

/// Refuse the one inheritance that can never resolve (soldr#2973).
///
/// [`pinned_channel`] returns `None` both when a workspace has no lint
/// libraries and when it has some that all inherit the root. The caller's next
/// tiers *derive* a nightly, which is the right answer for the first case and
/// only sometimes right for the second:
///
/// * root pins a dated nightly — inheriting is sound, and a driver exists for
///   it. `ci/fixtures/dylint-cache` is exactly this and must keep working.
/// * root pins a stable release (this workspace's own `1.95.0`) — no Dylint
///   driver has ever existed for a stable channel, so the version map turns it
///   into some *other* nightly and the run dies at the driver gate naming a
///   channel nobody chose. That is soldr#2945 reached by a second route.
///
/// `inherited_is_nightly` is passed in rather than recomputed because the
/// predicate lives with the resolver in `dylint_toolchain`, and two spellings
/// of "is this a dated nightly" is how the original bug got its second route.
pub(crate) fn reject_underivable_inheritance(
    workspace_root: &Path,
    inherited: Option<&str>,
    inherited_is_nightly: bool,
) -> Result<(), SoldrError> {
    if inherited_is_nightly {
        return Ok(());
    }
    let libraries = library_directories(workspace_root)?;
    if libraries.is_empty() {
        return Ok(());
    }
    let root = match inherited {
        Some(channel) => format!("the workspace root pins `{channel}`"),
        None => "the workspace root pins no channel".to_string(),
    };
    Err(SoldrError::Other(format!(
        "this workspace declares {} Dylint library director{} but none of them pins a nightly, \
         and {root} — not a dated nightly. Dylint builds one driver per library toolchain and \
         drivers exist only for dated nightlies, so inheriting this channel cannot resolve: \
         soldr would derive some other nightly from it and then fail at the driver gate naming a \
         channel nothing asked for (soldr#2945/soldr#2973). Give each library under \
         workspace.metadata.dylint.libraries its own rust-toolchain.toml pinning the nightly its \
         lints are built against, or pin the workspace root to that nightly so they inherit it.",
        libraries.len(),
        if libraries.len() == 1 { "y" } else { "ies" },
    )))
}

/// `dir-a (nightly-…), dir-b (nightly-…)` — every library named with the
/// channel it declares, because "they disagree" without saying which is not
/// an actionable diagnostic.
fn describe(pinned: &[(String, String)]) -> String {
    pinned
        .iter()
        .map(|(directory, channel)| format!("{directory} ({channel})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every directory named by `workspace.metadata.dylint.libraries`, with `*`
/// expanded. Sorted and de-duplicated so two entries whose globs overlap
/// (`dylints/*` plus `dylints/ban_raw_env_flag`) do not report the same
/// library twice.
///
/// A missing or unparseable-as-metadata `Cargo.toml` yields an empty list
/// rather than an error: "this workspace declares no Dylint libraries" is a
/// legitimate state, and the caller's fallback chain handles it.
pub(crate) fn library_directories(workspace_root: &Path) -> Result<Vec<PathBuf>, SoldrError> {
    let mut directories = Vec::new();
    for pattern in declared_library_paths(workspace_root)? {
        directories.extend(expand(workspace_root, &pattern));
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

fn declared_library_paths(workspace_root: &Path) -> Result<Vec<String>, SoldrError> {
    let manifest_path = workspace_root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Ok(Vec::new());
    };
    let manifest: toml::Value = toml::from_str(&text).map_err(|error| {
        SoldrError::Other(format!(
            "failed to parse {}: {error}",
            manifest_path.display()
        ))
    })?;
    let Some(libraries) = manifest
        .get("workspace")
        .and_then(|value| value.get("metadata"))
        .and_then(|value| value.get("dylint"))
        .and_then(|value| value.get("libraries"))
        .and_then(toml::Value::as_array)
    else {
        return Ok(Vec::new());
    };
    Ok(libraries
        .iter()
        .filter_map(|library| library.get("path").and_then(toml::Value::as_str))
        .map(str::to_string)
        .collect())
}

/// Expand one declared `path` against `root`.
///
/// Soldr has no glob crate in its dependency graph and this is not worth
/// adding one for: Dylint library paths are short, relative, and the only
/// wildcard anyone uses is `*` on a single path component. Components without
/// a `*` are joined literally — a declared-but-absent literal path is kept, so
/// the caller can report it against the path the user actually wrote rather
/// than dropping it — while components with a `*` are matched against
/// `read_dir`.
///
/// A glob component matches **directories only**, and — following the usual
/// glob convention — `*` does not match a name beginning with `.`, which keeps
/// `.git` and editor scratch directories out of a `lints/*` expansion.
fn expand(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut current = vec![root.to_path_buf()];
    for component in pattern
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != ".")
    {
        if !component.contains('*') {
            current = current
                .into_iter()
                .map(|path| path.join(component))
                .collect();
            continue;
        }
        let mut expanded = Vec::new();
        for directory in &current {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            let mut matches: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    matches_wildcard(component, entry.file_name().to_string_lossy().as_ref())
                        && entry.path().is_dir()
                })
                .map(|entry| entry.path())
                .collect();
            matches.sort();
            expanded.extend(matches);
        }
        current = expanded;
    }
    current
}

/// `*`-only wildcard match over a single path component.
fn matches_wildcard(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let mut segments = pattern.split('*');
    let prefix = segments.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let segments: Vec<&str> = segments.collect();
    let Some((suffix, middle)) = segments.split_last() else {
        // No `*` at all: the pattern is a literal.
        return rest.is_empty();
    };
    for segment in middle {
        if segment.is_empty() {
            continue;
        }
        let Some(index) = rest.find(segment) else {
            return false;
        };
        rest = &rest[index + segment.len()..];
    }
    rest.len() >= suffix.len() && rest.ends_with(suffix)
}

/// `directory` relative to `workspace_root`, `/`-separated so a diagnostic
/// reads the same on every host. Falls back to the full path when the
/// directory is somehow not under the root.
fn relative_display(workspace_root: &Path, directory: &Path) -> String {
    directory
        .strip_prefix(workspace_root)
        .unwrap_or(directory)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds `<root>/Cargo.toml` with the given `libraries` array body plus a
    /// `rust-toolchain.toml` in each named directory.
    fn workspace(libraries: &str, lints: &[(&str, &str)]) -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        let manifest = format!(
            "[workspace]\nmembers=[]\n[workspace.metadata.dylint]\nlibraries=[{libraries}]\n"
        );
        std::fs::write(temp.path().join("Cargo.toml"), manifest).expect("write workspace manifest");
        for (relative, channel) in lints {
            let directory = temp.path().join(relative);
            std::fs::create_dir_all(&directory).expect("create lint dir");
            if !channel.is_empty() {
                std::fs::write(
                    directory.join("rust-toolchain.toml"),
                    format!("[toolchain]\nchannel='{channel}'\n"),
                )
                .expect("write lint manifest");
            }
        }
        temp
    }

    const SIX: &[(&str, &str)] = &[
        ("dylints/ban_raw_process_creation", "nightly-2026-05-28"),
        ("dylints/ban_raw_network_access", "nightly-2026-05-28"),
        ("dylints/ban_raw_local_socket_name", "nightly-2026-05-28"),
        ("dylints/ban_raw_ipc_transport", "nightly-2026-05-28"),
        (
            "dylints/ban_platform_cfg_outside_boundary",
            "nightly-2026-05-28",
        ),
        ("dylints/ban_raw_env_flag", "nightly-2026-05-28"),
    ];

    /// The defect soldr#2945 was filed for: `dylints/*` must enumerate every
    /// lint directory. Before the fix this joined the literal `dylints/*`,
    /// read no manifest, and reported no libraries at all.
    #[test]
    fn glob_expansion_finds_every_lint_directory() {
        let temp = workspace("{path='dylints/*'}", SIX);
        let directories = library_directories(temp.path()).expect("expand libraries");
        assert_eq!(
            directories.len(),
            6,
            "expected the six lint directories, got {directories:?}"
        );
        let pinned = pinned_channel(temp.path())
            .expect("read library pins")
            .expect("libraries are declared");
        assert_eq!(pinned.channel, "nightly-2026-05-28");
        assert_eq!(
            pinned.libraries,
            vec![
                "dylints/ban_platform_cfg_outside_boundary",
                "dylints/ban_raw_env_flag",
                "dylints/ban_raw_ipc_transport",
                "dylints/ban_raw_local_socket_name",
                "dylints/ban_raw_network_access",
                "dylints/ban_raw_process_creation",
            ]
        );
    }

    #[test]
    fn glob_expansion_skips_files_and_dot_directories() {
        let temp = workspace(
            "{path='dylints/*'}",
            &[("dylints/one", "nightly-2026-05-28")],
        );
        std::fs::create_dir_all(temp.path().join("dylints/.scratch")).expect("dot dir");
        std::fs::write(temp.path().join("dylints/README.md"), "x").expect("stray file");
        let directories = library_directories(temp.path()).expect("expand libraries");
        assert_eq!(directories, vec![temp.path().join("dylints").join("one")]);
    }

    #[test]
    fn overlapping_declarations_report_each_library_once() {
        let temp = workspace(
            "{path='dylints/*'},{path='dylints/one'}",
            &[("dylints/one", "nightly-2026-05-28")],
        );
        let directories = library_directories(temp.path()).expect("expand libraries");
        assert_eq!(directories.len(), 1, "{directories:?}");
    }

    #[test]
    fn disagreeing_library_channels_name_the_directories_and_channels() {
        let temp = workspace(
            "{path='dylints/*'}",
            &[
                ("dylints/lint-a", "nightly-2026-04-16"),
                ("dylints/lint-b", "nightly-2026-04-17"),
            ],
        );
        let error = pinned_channel(temp.path())
            .expect_err("disagreeing pins must not resolve")
            .to_string();
        assert!(error.contains("conflicting"), "{error}");
        assert!(
            error.contains("dylints/lint-a (nightly-2026-04-16)"),
            "{error}"
        );
        assert!(
            error.contains("dylints/lint-b (nightly-2026-04-17)"),
            "{error}"
        );
    }

    /// A host-qualified pin and a bare pin name the same driver, so they are
    /// not a conflict.
    #[test]
    fn host_qualified_and_bare_pins_agree() {
        let temp = workspace(
            "{path='dylints/*'}",
            &[
                ("dylints/lint-a", "nightly-2026-05-28"),
                (
                    "dylints/lint-b",
                    "nightly-2026-05-28-x86_64-unknown-linux-gnu",
                ),
            ],
        );
        let pinned = pinned_channel(temp.path())
            .expect("host-qualified pins must agree")
            .expect("libraries are declared");
        assert_eq!(pinned.channel, "nightly-2026-05-28");
    }

    /// `ci/fixtures/dylint-cache` is exactly this shape: one library with no
    /// `rust-toolchain.toml` of its own, pinned at the workspace root it
    /// inherits from. The libraries have nothing to add, so the root stays the
    /// authority and this must not become an error.
    #[test]
    fn libraries_that_all_inherit_the_root_manifest_report_none() {
        let temp = workspace("{path='lint'}", &[("lint", "")]);
        assert_eq!(pinned_channel(temp.path()).expect("read pins"), None);
    }

    /// soldr#2973. `ci/fixtures/dylint-cache` is this exact shape — one
    /// unpinned library under a root that pins a *nightly* — and it resolves,
    /// so the guard must stay out of the way. This is the case a first attempt
    /// at the fix broke.
    #[test]
    fn an_inherited_nightly_root_is_left_alone() {
        let temp = workspace("{path='lint'}", &[("lint", "")]);
        reject_underivable_inheritance(temp.path(), Some("nightly-2026-05-28"), true)
            .expect("inheriting a dated nightly is sound and must not be rejected");
    }

    /// The half that cannot work: no Dylint driver has ever existed for a
    /// stable channel, so deriving from one produces a nightly nobody chose
    /// and the failure surfaces at the driver gate blaming that channel.
    #[test]
    fn an_inherited_stable_root_is_refused_and_says_why() {
        let temp = workspace(
            "{path='dylints/*'}",
            &[("dylints/a", ""), ("dylints/b", "")],
        );
        let error = reject_underivable_inheritance(temp.path(), Some("1.95.0"), false)
            .expect_err("inheriting a stable root can never resolve a driver");
        let message = error.to_string();
        assert!(
            message.contains("2 Dylint library directories"),
            "{message}"
        );
        assert!(message.contains("`1.95.0`"), "{message}");
        assert!(message.contains("soldr#2973"), "{message}");
    }

    /// A root with no channel at all lands on the version map, which is the
    /// same derivation by another name.
    #[test]
    fn an_unpinned_root_beneath_libraries_is_refused_too() {
        let temp = workspace("{path='lint'}", &[("lint", "")]);
        let error = reject_underivable_inheritance(temp.path(), None, false)
            .expect_err("no root channel plus libraries still derives");
        assert!(
            error.to_string().contains("pins no channel"),
            "{}",
            error.to_string()
        );
    }

    /// A workspace with no lint libraries is the one case where deriving is
    /// genuinely the best available answer, and must keep working.
    #[test]
    fn a_workspace_without_libraries_may_still_derive() {
        let temp = workspace("", &[]);
        reject_underivable_inheritance(temp.path(), Some("1.95.0"), false)
            .expect("no libraries means nothing to inherit; derivation is allowed");
    }

    /// Half-pinned is the state nobody can serve: one driver gets fetched, and
    /// these libraries were not all built against the same compiler.
    #[test]
    fn a_library_that_inherits_beside_one_that_pins_is_an_error_naming_both() {
        let temp = workspace(
            "{path='dylints/*'}",
            &[
                ("dylints/lint-a", "nightly-2026-05-28"),
                ("dylints/lint-b", ""),
            ],
        );
        let error = pinned_channel(temp.path())
            .expect_err("a half-pinned workspace must not resolve")
            .to_string();
        assert!(
            error.contains("dylints/lint-a (nightly-2026-05-28)"),
            "{error}"
        );
        assert!(error.contains("dylints/lint-b"), "{error}");
        assert!(error.contains("rust-toolchain.toml"), "{error}");
    }

    /// A literal path is the only way to declare a library that is not there;
    /// silently ignoring it would hand the workspace back to the root-manifest
    /// fallback, which is the failure soldr#2945 exists to remove.
    #[test]
    fn a_declared_library_that_does_not_exist_is_reported() {
        let temp = workspace("{path='dylints/missing'}", &[]);
        let error = pinned_channel(temp.path())
            .expect_err("an absent library must not be silently skipped")
            .to_string();
        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains("dylints/missing"), "{error}");
    }

    #[test]
    fn a_workspace_without_dylint_libraries_reports_none() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n")
            .expect("write workspace manifest");
        assert_eq!(pinned_channel(temp.path()).expect("read pins"), None);

        // An empty declaration, and a glob that matches nothing, are the same
        // state: nothing to be the authority.
        let empty = workspace("", &[]);
        assert_eq!(pinned_channel(empty.path()).expect("read pins"), None);
        let unmatched = workspace("{path='dylints/*'}", &[]);
        assert_eq!(pinned_channel(unmatched.path()).expect("read pins"), None);
    }

    #[test]
    fn a_workspace_without_a_manifest_reports_none() {
        let temp = tempfile::tempdir().expect("workspace tempdir");
        assert_eq!(pinned_channel(temp.path()).expect("read pins"), None);
    }

    #[test]
    fn wildcard_matching_covers_prefix_suffix_and_infix() {
        assert!(matches_wildcard("*", "anything"));
        assert!(matches_wildcard("ban_*", "ban_raw_env_flag"));
        assert!(matches_wildcard("*_flag", "ban_raw_env_flag"));
        assert!(matches_wildcard("ban_*_flag", "ban_raw_env_flag"));
        assert!(matches_wildcard("exact", "exact"));

        assert!(!matches_wildcard("exact", "exactly"));
        assert!(!matches_wildcard("ban_*", "lint_raw"));
        assert!(!matches_wildcard("*_flag", "ban_raw_env"));
        assert!(
            !matches_wildcard("*", ".git"),
            "`*` must not match a dot directory"
        );
        assert!(
            matches_wildcard(".*", ".git"),
            "an explicit dot pattern may"
        );
    }

    #[test]
    fn canonical_channel_drops_the_host_suffix_only_for_nightlies() {
        assert_eq!(
            canonical_channel("nightly-2026-05-28-x86_64-pc-windows-msvc"),
            "nightly-2026-05-28"
        );
        assert_eq!(
            canonical_channel("nightly-2026-05-28"),
            "nightly-2026-05-28"
        );
        assert_eq!(canonical_channel("1.95.0"), "1.95.0");
    }
}
