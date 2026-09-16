//! Direct nested-Cargo descendant classification (soldr#2924).
//!
//! Soldr's process re-entrancy guard ([`crate::reentrancy_guard`]) catches
//! `soldr -> tool -> ... -> soldr`, but a direct `$CARGO` child — a test body
//! running `Command::new($CARGO) build` against the real toolchain Cargo —
//! never re-enters soldr, so `IN_SOLDR_PID` cannot reject it. Cargo overwrites
//! `CARGO` with its own executable path even when `.cargo/config.toml` forces
//! the variable, so rewriting that env var is not an interception mechanism.
//!
//! This module classifies a descendant process by its executable basename and
//! command line, so the Cargo front door can detect (and then tear down) a
//! nested Cargo build that will deadlock on the outer Cargo's active target
//! lock. It is a pure function: no process walking, no I/O, so it is trivially
//! unit-tested on every platform.

/// How a descendant process's Cargo invocation should be treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoDescendant {
    /// Not a Cargo executable (`cargo` / `cargo.exe`).
    NotCargo,
    /// Cargo, but a command that does not acquire the active build target
    /// lock (`--version`, `--list`, `metadata --no-deps`, …).
    NonLocking,
    /// Cargo with a build-like verb but a provably distinct `--target-dir`,
    /// so it cannot deadlock on the outer build's target lock.
    DistinctTargetDir,
    /// Cargo with a build-like verb and no distinct `--target-dir`: it will
    /// wait on the outer Cargo's lock. Hazardous.
    Hazardous,
}

/// Cargo verbs that acquire the active build target lock. The issue's list
/// (`build`, `test`, `check`, `clippy`, `run`, `bench`, `doc`, `rustc`, …)
/// plus the other first-party verbs that compile or link. Everything not listed
/// here is treated as non-locking (`metadata`, `search`, `login`, …), which is
/// safe because the bare `--version` / `--list` / `--help` spellings never
/// reach a verb at all.
pub(crate) const BUILD_LIKE_VERBS: &[&str] = &[
    "build",
    "test",
    "check",
    "clippy",
    "run",
    "bench",
    "doc",
    "rustc",
    "fmt",
    "fix",
    "tree",
    "package",
    "install",
    "uninstall",
    "publish",
    "vendor",
    "update",
    "add",
    "remove",
    "generate-lockfile",
];

/// Long flags that consume a separate value argument, so verb discovery skips
/// their value too. Kept conservative: an unknown `--flag value` shape is
/// treated as two tokens, which errs toward finding a verb (and thus toward
/// classifying a build as hazardous) rather than missing one.
const VALUE_LONG_FLAGS: &[&str] = &[
    "manifest-path",
    "config",
    "target",
    "target-dir",
    "color",
    "message-format",
    "jobs",
    "profile",
    "features",
    "package",
    "exclude",
    "bin",
    "example",
    "test",
    "bench",
    "lib",
    "filter",
    "format",
    "out-dir",
    "timings",
    "future-incompat-report",
];

/// Classify a descendant process by its executable path and arguments.
pub(crate) fn classify_cargo_descendant(exe: &str, args: &[String]) -> CargoDescendant {
    // Split on both path separators so the classifier is host-independent: a
    // Windows `C:\…\cargo.exe` observed on a Linux host still resolves to
    // `cargo.exe`.
    let base = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
    // Windows executable spelling is `cargo.exe`; POSIX is `cargo`.
    let base = base.strip_suffix(".exe").unwrap_or(base);
    if !base.eq_ignore_ascii_case("cargo") {
        return CargoDescendant::NotCargo;
    }
    let Some(verb) = find_verb(args) else {
        // No positional verb: `--version`, `--list`, `--help`, or flags only.
        return CargoDescendant::NonLocking;
    };
    if !BUILD_LIKE_VERBS.contains(&verb.as_str()) {
        return CargoDescendant::NonLocking;
    }
    if has_distinct_target_dir(args) {
        return CargoDescendant::DistinctTargetDir;
    }
    CargoDescendant::Hazardous
}

/// The first non-flag positional argument — Cargo's verb. Returns `None` when
/// the invocation has no positional (pure flag spelling such as `--version`).
fn find_verb(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(rest) = arg.strip_prefix("--") {
            if rest.is_empty() {
                // A bare `--` ends flag parsing; the next token is the verb.
                return args.get(i + 1).cloned();
            }
            if rest.contains('=') {
                // `--flag=value`: self-contained, consume one token.
                i += 1;
                continue;
            }
            let name = rest;
            if VALUE_LONG_FLAGS.contains(&name) && i + 1 < args.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if arg.starts_with('-') {
            // Short flag. `-Z` and `-j` take a separate value; everything else
            // is a bare switch or a `-jN`-style combined flag.
            if matches!(arg, "-Z" | "-j") && i + 1 < args.len() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        return Some(arg.to_string());
    }
    None
}

/// Whether the args carry `--target-dir` (either split or `=` form), which a
/// nested build can use to prove it targets a directory distinct from the
/// outer build's.
fn has_distinct_target_dir(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--target-dir" || arg.starts_with("--target-dir="))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cargo_spellings_on_windows_and_posix_are_recognized() {
        for exe in [
            "cargo",
            "cargo.exe",
            "/usr/bin/cargo",
            r"C:\rust\bin\cargo.exe",
        ] {
            assert_eq!(
                classify_cargo_descendant(exe, &argv(&["build"])),
                CargoDescendant::Hazardous,
                "{exe}"
            );
        }
    }

    #[test]
    fn a_non_cargo_executable_is_not_cargo() {
        assert_eq!(
            classify_cargo_descendant("rustc", &argv(&["build"])),
            CargoDescendant::NotCargo
        );
        assert_eq!(
            classify_cargo_descendant("cargo-foo", &argv(&["build"])),
            CargoDescendant::NotCargo
        );
    }

    #[test]
    fn build_like_verbs_are_hazardous_without_a_target_dir() {
        for verb in BUILD_LIKE_VERBS {
            assert_eq!(
                classify_cargo_descendant("cargo", &argv(&[verb])),
                CargoDescendant::Hazardous,
                "{verb}"
            );
        }
    }

    #[test]
    fn non_locking_verbs_are_allowed() {
        for verb in ["metadata", "search", "login", "logout", "owner", "yank"] {
            assert_eq!(
                classify_cargo_descendant("cargo", &argv(&[verb])),
                CargoDescendant::NonLocking,
                "{verb}"
            );
        }
        // metadata --no-deps is the canonical non-locking spell.
        assert_eq!(
            classify_cargo_descendant("cargo", &argv(&["metadata", "--no-deps"])),
            CargoDescendant::NonLocking
        );
    }

    #[test]
    fn flag_only_spellings_are_non_locking() {
        for args in [&["--version"][..], &["--list"][..], &["--help"][..]] {
            assert_eq!(
                classify_cargo_descendant("cargo", &argv(args)),
                CargoDescendant::NonLocking
            );
        }
    }

    #[test]
    fn flags_before_the_verb_are_skipped() {
        assert_eq!(
            classify_cargo_descendant("cargo", &argv(&["--verbose", "build"])),
            CargoDescendant::Hazardous
        );
        assert_eq!(
            classify_cargo_descendant("cargo", &argv(&["--manifest-path", "Cargo.toml", "build"]),),
            CargoDescendant::Hazardous
        );
        assert_eq!(
            classify_cargo_descendant("cargo", &argv(&["--color", "never", "check"])),
            CargoDescendant::Hazardous
        );
    }

    #[test]
    fn a_distinct_target_dir_makes_a_build_allowed() {
        assert_eq!(
            classify_cargo_descendant("cargo", &argv(&["build", "--target-dir", "/tmp/other"])),
            CargoDescendant::DistinctTargetDir
        );
        assert_eq!(
            classify_cargo_descendant("cargo", &argv(&["build", "--target-dir=/tmp/other"])),
            CargoDescendant::DistinctTargetDir
        );
    }
}
