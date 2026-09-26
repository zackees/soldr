//! Per-site rewording of `AliasError` messages must not come back (soldr#3390).
//!
//! `target_alias::AliasError`'s `Display` deliberately carries no command
//! verb — it is shared by every `--target`-accepting surface (`build`,
//! `prepare`, `env`, `cc`/`c++`, `wheel`, `lint`). A caller names the surface
//! it actually is by rendering the error through `AliasError::render` /
//! `AliasError::into_soldr_error` with a `target_alias::TargetSurface`.
//!
//! Before soldr#3390, five call sites instead patched the resolver's own
//! (then build-worded) message with a `.replace(...)` / `.replacen(...)` call
//! hard-coded to look for the literal `"soldr build --target"` prefix. Every
//! one of those shapes drifted independently: `wheel_cmd.rs` doubled the
//! verb, `lint_cmd.rs` doubled the input, `env_cmd.rs` and
//! `target_lifecycle.rs`'s `resolve_prepare_targets` never reworded at all,
//! and `AliasError::AllNotBuildable`'s body named `soldr build` regardless of
//! the prefix fix. A silent-no-op-on-miss string patch is exactly the drift
//! CLAUDE.md's code-smell rule describes: nothing failed to compile, and
//! nothing noticed.
//!
//! This guard fails the build if any file under `crates/` (other than this
//! one) still contains a `.replace(` / `.replacen(` call whose first argument
//! is the string literal `"soldr build --target"`. Whitespace (including
//! newlines, so the multi-line `replacen(\n    "soldr build --target",` form
//! is caught too) is normalized away before matching, since Rust doesn't care
//! how a call is wrapped.

use crate::common;

use std::path::{Path, PathBuf};

/// This guard's own module doc and comments name the banned literal for
/// explanatory purposes; it is exempted the same way `no_timed_test_guard.rs`
/// exempts itself.
const EXEMPT_PATHS: &[&str] = &["crates/soldr-cli/tests/guards/alias_error_surface_guard.rs"];

/// The banned first argument, with all whitespace removed. The haystack is
/// normalized the same way, so arbitrary line-wrapping between the method
/// name, the `(`, and the string literal cannot hide the pattern.
const BANNED_REPLACE: &str = "soldrbuild--target";

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` holds generated code; it is not ours to police.
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// True iff `body` contains `.replace(` or `.replacen(` whose first argument
/// is (whitespace-insensitively) the literal `"soldr build --target"`.
fn has_banned_reword(body: &str) -> bool {
    let normalized: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let replace_needle = format!(".replace(\"{BANNED_REPLACE}\"");
    let replacen_needle = format!(".replacen(\"{BANNED_REPLACE}\"");
    normalized.contains(&replace_needle) || normalized.contains(&replacen_needle)
}

#[test]
fn alias_error_reword_does_not_regrow() {
    let crates_dir = common::workspace_root().join("crates");
    let mut sources = Vec::new();
    rust_sources(&crates_dir, &mut sources);
    assert!(
        sources.len() > 100,
        "walker found only {} files; it is not reaching the workspace",
        sources.len()
    );

    let root = common::workspace_root();
    let mut offenders = Vec::new();
    for path in sources {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT_PATHS.contains(&relative.as_str()) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if has_banned_reword(&body) {
            offenders.push(relative);
        }
    }

    assert!(
        offenders.is_empty(),
        "found a per-site `.replace(\"soldr build --target\", ...)` / \
         `.replacen(\"soldr build --target\", ...)` rewording of an `AliasError` \
         message (soldr#3390) in:\n{}\n\n\
         Render the error through `AliasError::render(surface)` or \
         `AliasError::into_soldr_error(surface)` with the caller's own \
         `target_alias::TargetSurface` instead -- see `target_alias.rs`.",
        offenders.join("\n")
    );
}

#[test]
fn matcher_catches_both_call_shapes_and_ignores_prose() {
    assert!(has_banned_reword(
        "msg.replace(\"soldr build --target\", \"soldr wheel --target\")"
    ));
    assert!(has_banned_reword(
        "message.replacen(\n        \"soldr build --target\",\n        &verb,\n        1,\n    )"
    ));
    // Naming the blessed surface in prose or a message is fine.
    assert!(!has_banned_reword(
        "// `soldr build --target <T>` is the blessed surface"
    ));
    assert!(!has_banned_reword("format!(\"soldr build --target {t}\")"));
}
