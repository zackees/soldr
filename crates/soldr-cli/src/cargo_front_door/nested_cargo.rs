//! Direct nested-Cargo descendant classification (soldr#2924).
//!
//! Soldr's process re-entrancy guard ([`crate::reentrancy_guard`]) catches
//! `soldr -> tool -> ... -> soldr`, but a direct `$CARGO` child — a build
//! script running `Command::new($CARGO) build` against the real toolchain
//! Cargo — never re-enters soldr, so `IN_SOLDR_PID` cannot reject it. Cargo
//! overwrites `CARGO` with its own executable path even when
//! `.cargo/config.toml` forces the variable, so rewriting that env var is not
//! an interception mechanism.
//!
//! This module classifies one observed process by its argument vector, so the
//! front door's [`super::nested_cargo_guard`] can decide whether a nested
//! Cargo will deadlock on the outer Cargo's active target lock. It is a pure
//! function: no process walking, no I/O, so it is unit-tested on every
//! platform. Verb discovery reuses the front door's own
//! [`super::subcommand::first_cargo_subcommand_index`] rather than a second
//! Cargo argument parser, so the two cannot drift on `+toolchain`, `-C`, or
//! value-taking global flags.

use super::subcommand::first_cargo_subcommand_index;

/// How a descendant process's Cargo invocation should be treated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CargoDescendant {
    /// Not a Cargo executable (`cargo` / `cargo.exe`).
    NotCargo,
    /// Cargo, but a command that does not acquire the active build target
    /// lock (`--version`, `--list`, `metadata --no-deps`, …).
    NonLocking,
    /// Cargo with a verb that may acquire the build target lock. Whether it
    /// can collide with the outer build depends on its target directory,
    /// which the guard checks against the outer lock holder.
    Locking(LockingCargo),
}

/// A lock-acquiring nested Cargo invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockingCargo {
    /// The verb (or alias / external subcommand name) that was found.
    pub(crate) verb: String,
    /// What the command line says about the target directory.
    pub(crate) target_dir: TargetDirClaim,
}

/// The target-directory evidence carried by a nested Cargo command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetDirClaim {
    /// No `--target-dir`: the target comes from the environment, config, or
    /// the workspace default — none of which the observer can read portably.
    Absent,
    /// An explicit `--target-dir`, split or `=` form. `change_dir` is the
    /// global `-C <dir>` Cargo applies before resolving a relative path.
    Explicit {
        path: String,
        change_dir: Option<String>,
    },
    /// The target directory is set through `--config`, whose TOML value this
    /// classifier does not evaluate. Never treated as proof of isolation.
    ConfigOverride,
}

/// Cargo verbs that acquire the active build target lock. The issue's list
/// (`build`, `test`, `check`, `clippy`, `run`, `bench`, `doc`, `rustc`, …)
/// plus the other first-party verbs that compile, link, or rewrite the
/// workspace, and Cargo's built-in single-letter aliases.
pub(crate) const BUILD_LIKE_VERBS: &[&str] = &[
    "build",
    "b",
    "test",
    "t",
    "check",
    "c",
    "clippy",
    "run",
    "r",
    "bench",
    "doc",
    "d",
    "rustc",
    "rustdoc",
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
    "rm",
    "generate-lockfile",
];

/// Cargo verbs proven not to take the build target lock. Anything that is
/// neither here nor in [`BUILD_LIKE_VERBS`] — a user alias from
/// `[alias]`, an external `cargo-<name>` subcommand — is treated as locking:
/// an alias such as `xb = "build"` would lock, and the classifier cannot read
/// the alias table, so it fails closed.
pub(crate) const NON_LOCKING_VERBS: &[&str] = &[
    "metadata",
    "search",
    "login",
    "logout",
    "owner",
    "yank",
    "help",
    "version",
    "locate-project",
    "pkgid",
    "verify-project",
    "read-manifest",
    "config",
    "info",
    "report",
    "init",
    "new",
];

/// Whether `exe` names a Cargo executable (`cargo` / `cargo.exe`, any
/// directory, either path separator — host-independent).
pub(crate) fn is_cargo_executable(exe: &str) -> bool {
    executable_stem(exe).eq_ignore_ascii_case("cargo")
}

/// The file name of `exe` with a trailing `.exe` removed, splitting on both
/// path separators so a Windows path observed on any host still resolves.
pub(crate) fn executable_stem(exe: &str) -> &str {
    let base = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
    base.strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(base)
}

/// Classify a descendant process by its executable path and arguments.
pub(crate) fn classify_cargo_descendant(exe: &str, args: &[String]) -> CargoDescendant {
    if !is_cargo_executable(exe) {
        return CargoDescendant::NotCargo;
    }
    let Some(verb_index) = first_cargo_subcommand_index(args) else {
        // No positional verb: `--version`, `--list`, `--help`, or flags only.
        return CargoDescendant::NonLocking;
    };
    let verb = args[verb_index].as_str();
    if NON_LOCKING_VERBS.contains(&verb) {
        return CargoDescendant::NonLocking;
    }
    CargoDescendant::Locking(LockingCargo {
        verb: verb.to_string(),
        target_dir: target_dir_claim(args),
    })
}

/// What the Cargo-owned part of `args` (everything before a bare `--`, which
/// hands the rest to the program being run) says about the target directory.
/// A program argument after `--` can therefore never forge isolation.
fn target_dir_claim(args: &[String]) -> TargetDirClaim {
    let cargo_owned = args.split(|arg| arg == "--").next().unwrap_or(&[]);
    let mut path = None;
    let mut change_dir = None;
    let mut config_override = false;
    let mut iter = cargo_owned.iter();
    while let Some(arg) = iter.next() {
        if arg == "--target-dir" {
            path = iter.next().cloned();
        } else if let Some(value) = arg.strip_prefix("--target-dir=") {
            path = Some(value.to_string());
        } else if arg == "-C" {
            change_dir = iter.next().cloned();
        } else if let Some(value) = arg.strip_prefix("-C=") {
            change_dir = Some(value.to_string());
        } else if arg == "--config" {
            config_override |= iter.next().is_some_and(|value| config_sets_target(value));
        } else if let Some(value) = arg.strip_prefix("--config=") {
            config_override |= config_sets_target(value);
        }
    }
    if config_override {
        return TargetDirClaim::ConfigOverride;
    }
    match path {
        Some(path) if !path.is_empty() => TargetDirClaim::Explicit { path, change_dir },
        _ => TargetDirClaim::Absent,
    }
}

/// A `--config` value (inline TOML or a config-file path) that could move the
/// target or build directory. A path to a config file is opaque, so any
/// non-inline value counts too.
fn config_sets_target(value: &str) -> bool {
    !value.contains('=') || value.contains("target-dir") || value.contains("build-dir")
}

/// The byte cap on a diagnostic command-line head.
pub(crate) const HEAD_LIMIT: usize = 200;

/// A bounded, best-effort command-line head for a diagnostic. Never the full
/// argv — diagnostics must not echo an unbounded command line.
pub(crate) fn bounded_head(args: &[String]) -> String {
    let joined = args.join(" ");
    if joined.len() <= HEAD_LIMIT {
        return joined;
    }
    let mut head: String = joined.chars().take(HEAD_LIMIT).collect();
    head.push('…');
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn locking(exe: &str, args: &[&str]) -> LockingCargo {
        match classify_cargo_descendant(exe, &argv(args)) {
            CargoDescendant::Locking(locking) => locking,
            other => panic!("{exe} {args:?} should lock, got {other:?}"),
        }
    }

    #[test]
    fn cargo_spellings_on_windows_and_posix_are_recognized() {
        for exe in [
            "cargo",
            "cargo.exe",
            "/usr/bin/cargo",
            r"C:\rust\bin\cargo.exe",
            r"C:\rust\bin\CARGO.EXE",
        ] {
            assert_eq!(locking(exe, &["build"]).verb, "build", "{exe}");
        }
    }

    #[test]
    fn a_non_cargo_executable_is_not_cargo() {
        for exe in ["rustc", "cargo-foo", "/usr/bin/cargo-clippy", "soldr"] {
            assert_eq!(
                classify_cargo_descendant(exe, &argv(&["build"])),
                CargoDescendant::NotCargo,
                "{exe}"
            );
        }
    }

    #[test]
    fn build_like_verbs_lock_without_a_target_dir() {
        for verb in BUILD_LIKE_VERBS {
            let got = locking("cargo", &[verb]);
            assert_eq!(got.verb, *verb);
            assert_eq!(got.target_dir, TargetDirClaim::Absent, "{verb}");
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
    fn unknown_verbs_and_aliases_fail_closed() {
        // A user `[alias]` (`xb = "build"`) or an external subcommand cannot
        // be proven non-locking from the command line.
        for verb in ["xb", "xtask", "nextest"] {
            assert_eq!(locking("cargo", &[verb]).verb, verb);
        }
    }

    #[test]
    fn flag_only_spellings_are_non_locking() {
        for args in [
            &["--version"][..],
            &["--list"][..],
            &["--help"][..],
            &["-V"][..],
        ] {
            assert_eq!(
                classify_cargo_descendant("cargo", &argv(args)),
                CargoDescendant::NonLocking
            );
        }
    }

    #[test]
    fn flags_before_the_verb_are_skipped() {
        assert_eq!(locking("cargo", &["--verbose", "build"]).verb, "build");
        let manifest = locking("cargo", &["--manifest-path", "Cargo.toml", "build"]);
        assert_eq!(manifest.verb, "build");
        assert_eq!(manifest.target_dir, TargetDirClaim::Absent);
        assert_eq!(
            locking("cargo", &["--manifest-path=Cargo.toml", "b"]).verb,
            "b"
        );
        assert_eq!(
            locking("cargo", &["--color", "never", "check"]).verb,
            "check"
        );
        // Toolchain overrides and `-C <dir>` are the shared parser's job.
        assert_eq!(locking("cargo", &["+nightly", "build"]).verb, "build");
        assert_eq!(locking("cargo", &["-C", "sub", "build"]).verb, "build");
        assert_eq!(
            locking("cargo", &["-Z", "unstable-options", "test"]).verb,
            "test"
        );
    }

    #[test]
    fn target_dir_is_read_in_split_and_equals_forms() {
        let explicit = |path: &str| TargetDirClaim::Explicit {
            path: path.to_string(),
            change_dir: None,
        };
        assert_eq!(
            locking("cargo", &["build", "--target-dir", "/tmp/other"]).target_dir,
            explicit("/tmp/other")
        );
        assert_eq!(
            locking("cargo", &["build", "--target-dir=/tmp/other"]).target_dir,
            explicit("/tmp/other")
        );
        assert_eq!(
            locking("cargo", &["-C", "sub", "build", "--target-dir", "t"]).target_dir,
            TargetDirClaim::Explicit {
                path: "t".to_string(),
                change_dir: Some("sub".to_string()),
            }
        );
    }

    #[test]
    fn program_arguments_after_double_dash_cannot_forge_a_target_dir() {
        assert_eq!(
            locking("cargo", &["run", "--", "--target-dir", "/tmp/other"]).target_dir,
            TargetDirClaim::Absent
        );
    }

    #[test]
    fn a_config_target_override_is_never_proof() {
        for config in [
            "build.target-dir=\"/tmp/x\"",
            "build.build-dir=\"/tmp/x\"",
            "extra-config.toml",
        ] {
            assert_eq!(
                locking(
                    "cargo",
                    &["build", "--config", config, "--target-dir", "/tmp/y"]
                )
                .target_dir,
                TargetDirClaim::ConfigOverride,
                "{config}"
            );
        }
        // An unrelated inline config value does not poison the claim.
        assert_eq!(
            locking("cargo", &["build", "--config=net.offline=true"]).target_dir,
            TargetDirClaim::Absent
        );
    }

    #[test]
    fn the_diagnostic_head_is_bounded() {
        let long = format!("build {}", "x".repeat(1000));
        let head = bounded_head(&[long]);
        assert!(head.chars().count() <= HEAD_LIMIT + 1, "{head}");
        assert!(head.ends_with('…'));
        // A short head is passed through unchanged.
        assert_eq!(
            bounded_head(&argv(&["build", "--release"])),
            "build --release"
        );
    }
}
