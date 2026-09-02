//! Which Dylint target tree `soldr dylint cook` is preparing (soldr#3042
//! step 3).
//!
//! There are two Dylint target trees, and they must never share a cook
//! digest. `target/dylint/target` is the tree `cargo dylint` analyses the
//! workspace in (the historical, and until now only, `soldr dylint cook`
//! surface). `target/dylint/tests` is the dependency layer the Dylint
//! UI-test stages (`ci_test/plan.rs`'s `dylint-test-*` stages) compile
//! their `trybuild`/UI harnesses against. The two trees are populated by
//! differently-shaped cargo invocations (check vs. build — see
//! [`CookTree::operation`]) and are keyed by differently-derived channel
//! segments (see [`CookTree::channel_segment`]), so collapsing them into one
//! digest would let one tree's cook satisfy the other's marker while leaving
//! the wrong-shaped artifacts on disk.

use crate::core::SoldrError;

/// Which of the two Dylint target trees a `soldr dylint cook` invocation is
/// preparing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CookTree {
    /// `target/dylint/target` — the tree `cargo dylint` analyses the
    /// workspace in. Historical default; every existing `soldr dylint cook`
    /// behaviour is this variant.
    #[default]
    Analysis,
    /// `target/dylint/tests` — the dependency layer the Dylint UI-test
    /// stages compile their `trybuild`/UI harnesses against.
    Tests,
}

impl CookTree {
    /// Parse `--tree <value>`. Unknown values name both accepted spellings
    /// in the error so a typo is actionable without a docs round-trip.
    pub(crate) fn parse(value: &str) -> Result<Self, SoldrError> {
        match value {
            "analysis" => Ok(Self::Analysis),
            "tests" => Ok(Self::Tests),
            other => Err(SoldrError::Other(format!(
                "soldr dylint cook: unknown --tree `{other}`; expected `analysis` or `tests`"
            ))),
        }
    }

    /// The spelling that enters the cook digest (see `dylint_cook.rs`'s
    /// `build_output`) and the JSON `build_shape.tree` field. Kept distinct
    /// from [`Self::directory`] so a future third tree does not have to
    /// reuse a directory name as its digest identity.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Analysis => "analysis",
            Self::Tests => "tests",
        }
    }

    /// The `target/dylint/<directory>` path segment. `"target"` for
    /// [`Self::Analysis`] is the historical spelling and is preserved
    /// byte-for-byte — every existing analysis-tree path must not move.
    pub(crate) fn directory(self) -> &'static str {
        match self {
            Self::Analysis => "target",
            Self::Tests => "tests",
        }
    }

    /// The cargo operation this tree's cook is shaped as.
    ///
    /// FACT 2 (soldr#3042 step 3): `cargo check` emits `.rmeta` under a
    /// different fingerprint mode than the `cargo test` the UI-test stage
    /// runs (`ci_test/plan.rs:160-177`), so a check-shaped cook of the tests
    /// tree would be recompiled wholesale the first time the UI-test stage
    /// touched it. The tests tree must be build-shaped; the analysis tree
    /// keeps its historical check shape.
    pub(crate) fn operation(self) -> &'static str {
        match self {
            Self::Analysis => "check",
            Self::Tests => "build",
        }
    }

    /// The nightly-channel path segment cargo's `--target-dir` must resolve
    /// to, so the cook lands where its consumer looks.
    ///
    /// FACT 1 (soldr#3042 step 3): `ci_test/plan.rs:56` builds the UI-test
    /// target dir as `.../dylint/tests/<dylint_key>` where `dylint_key =
    /// canonical_channel(&nightly.channel, &host)` (`ci_test/plan.rs:631`),
    /// which APPENDS the host triple —
    /// `nightly-2026-05-28-x86_64-unknown-linux-gnu`. The analysis tree's
    /// consumer, `dylint_libraries::canonical_channel`
    /// (`dylint_libraries.rs:61`), instead TRUNCATES to 18 chars and yields
    /// the bare `nightly-2026-05-28`. These two rules disagree today, and
    /// this function deliberately reuses each consumer's own rule rather
    /// than inventing a third one that would agree with neither.
    pub(crate) fn channel_segment(self, channel: &str, host: &str) -> String {
        match self {
            Self::Analysis => crate::dylint_libraries::canonical_channel(channel).to_string(),
            Self::Tests => crate::ci_test::plan::canonical_channel(channel, host),
        }
    }
}

/// Apply the same environment `ci_test/execute.rs:848-861` gives every
/// `dylint-*` stage to the *current process*, so a `--tree tests` cook
/// compiles under the identical environment the UI-test stage will later
/// reuse the cooked artifacts under (soldr#3042, FACT 3).
///
/// `ci_test/execute.rs` builds a fresh `std::process::Command` per stage and
/// calls `Command::env` on it directly. `soldr dylint cook` instead calls
/// [`crate::cargo_front_door::run_cargo_front_door`], which spawns its
/// child cargo from the *current process's* environment, so the equivalent
/// here is `std::env::set_var` rather than `Command::env` — mirroring the
/// style of the other production `set_var("PATH", ...)` call sites,
/// `multicall.rs`'s `strip_self_from_path` and `prepare_github_env.rs`'s
/// `exported_env_pairs`.
///
/// Every piece mirrored is named at `ci_test/execute.rs:848-861`:
///   - `bootstrap.bin_dirs` prepended to `PATH`: this is where `dylint-link`
///     comes from — `dylints/*/.cargo/config.toml` sets
///     `rustflags = ["-C", "linker=dylint-link"]`, so the cook's
///     build-script and dummy-cdylib links need it on PATH.
///   - `bootstrap.env`: the resolved `dylint`/`dylint-link` bootstrap
///     environment (mirrors `self.dylint_env` there).
///   - `SOLDR_LINKER=default`: not cosmetic. soldr's linker injection
///     changes RUSTFLAGS, and RUSTFLAGS are in every unit's fingerprint, so
///     a cook without it produces artifacts cargo rejects as stale.
///   - `SOLDR_NO_GC_TARGET=1`: matches the UI-test stage so the cooked tree
///     is not swept mid-cook.
///
/// These assignments deliberately **override** an inherited value rather
/// than deferring to the caller, exactly as the UI-test stage's
/// `Command::env` does. A caller-wins rule would fail in the one case the
/// mirror exists for: someone with `SOLDR_LINKER=mold` exported would get a
/// cook whose RUSTFLAGS differ from the stage that has to reuse it, so every
/// cooked unit would be recompiled and the cook would be pure cost. The
/// scope is one process that is about to exec cargo and exit.
pub(crate) fn apply_dylint_ui_test_environment(
    bootstrap: &crate::cargo_front_door::SubcommandToolBootstrap,
) -> Result<(), SoldrError> {
    if !bootstrap.bin_dirs.is_empty() {
        let mut dirs = bootstrap.bin_dirs.clone();
        if let Some(existing) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&existing));
        }
        let joined = std::env::join_paths(dirs).map_err(|error| {
            SoldrError::Other(format!(
                "soldr dylint cook: failed to build PATH for the tests-tree cook: {error}"
            ))
        })?;
        std::env::set_var("PATH", joined);
    }
    for (key, value) in &bootstrap.env {
        std::env::set_var(key, value);
    }
    std::env::set_var("SOLDR_LINKER", "default");
    std::env::set_var("SOLDR_NO_GC_TARGET", "1");
    Ok(())
}

#[cfg(test)]
#[path = "dylint_cook_tree_tests.rs"]
mod tests;
