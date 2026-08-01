//! soldr#997 — friendly target aliases + Rust-triple passthrough.
//!
//! The native-chain front door of `soldr build --target …` accepts
//! either a soldr alias (`win-x64`, `mac-arm64`, `linux-x64-musl`, …)
//! or a real Rust target triple (`x86_64-pc-windows-msvc`, …).
//! Both routes converge on the resolved Rust triple plus the target OS
//! catalogue assets and linker environment needed before spawning cargo.
//! Workspace-dependent policies, including PyO3 target Python compatibility,
//! are resolved separately after target alias resolution.
//!
//! See [`resolve_soldr_target`] for the routing entry point.
//!
//! ## Grammar
//!
//! `<os>-<arch>[-<libc-or-abi>]`. All-lowercase, hyphen-separated.
//! `arch` is always the short form: `x64` (= `x86_64` / `amd64`) and
//! `arm64` (= `aarch64`). 32-bit triples don't ship in soldr.
//!
//! Canonical aliases — the documented form — are listed in the
//! [`CANONICAL_ALIASES`] table below. Synonyms (`darwin-arm64`,
//! `apple-silicon`, `musl-x64`, …) all resolve silently through
//! [`SYNONYMS`].
//!
//! ## CLAUDE.md policy
//!
//! Bare cargo built-in verbs (`build`, `test`, `check`, `run`,
//! `bench`, `doc`, `fmt`, `clippy`) become soldr-native **only when
//! `--target` is present**. Without it they remain shorthand for
//! `cargo <verb>`. The explicit escape hatch is `soldr cargo <verb>`
//! which is always pure passthrough regardless of `--target`. See
//! the documentation in `crate::main` for where the routing decision
//! is made.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Canonical alias → Rust target triple table. 8 entries; matches
/// `crate::core::canonical_targets::CANONICAL_TARGETS` 1:1.
pub const CANONICAL_ALIASES: &[(&str, &str)] = &[
    ("win-x64", "x86_64-pc-windows-msvc"),
    ("win-arm64", "aarch64-pc-windows-msvc"),
    ("mac-x64", "x86_64-apple-darwin"),
    ("mac-arm64", "aarch64-apple-darwin"),
    ("linux-x64", "x86_64-unknown-linux-gnu"),
    ("linux-arm64", "aarch64-unknown-linux-gnu"),
    ("linux-x64-musl", "x86_64-unknown-linux-musl"),
    ("linux-arm64-musl", "aarch64-unknown-linux-musl"),
];

/// Replace soldr target aliases in cargo-style arguments with Rust triples.
///
/// The blessed build path prepares its toolchain from the resolved target and
/// then forwards the same argument vector to cargo. Normalizing that vector
/// keeps documented aliases from leaking through to cargo as unknown targets.
/// Unknown values are deliberately left alone so cargo can still accept custom
/// target specifications.
pub fn normalize_target_aliases_in_args(args: &mut [String]) {
    let mut index = 0;
    while index < args.len() {
        if let Some(input) = args[index].strip_prefix("--target=") {
            if let Some(triple) = target_for_known_alias(input) {
                args[index] = format!("--target={triple}");
            }
        } else if args[index] == "--target" && index + 1 < args.len() {
            if let Some(triple) = target_for_known_alias(&args[index + 1]) {
                args[index + 1] = triple.to_string();
            }
            index += 1;
        }
        index += 1;
    }
}

/// `args` with any `--target <triple>.<glibc>` reduced to the bare triple.
///
/// soldr#2139: the `.<glibc>` suffix is a soldr-level spelling meaning "ask
/// zig for this floor". rustc has never heard of it, so it must not reach a
/// cargo child. This is applied at each place a cargo process is actually
/// spawned rather than once in the caller, because the blessed path spawns
/// two -- the build itself and `cargo fetch` -- and a rule enforced at the
/// boundary cannot be forgotten by a future third.
///
/// Borrows when there is nothing to strip, which is the overwhelmingly common
/// case, so the ordinary build path allocates nothing.
pub fn args_without_glibc_floor(args: &[String]) -> std::borrow::Cow<'_, [String]> {
    let needs_strip = args.iter().enumerate().any(|(index, arg)| {
        arg.strip_prefix("--target=")
            .map(|value| split_glibc_floor(value).is_some())
            .unwrap_or_else(|| {
                arg == "--target"
                    && args
                        .get(index + 1)
                        .is_some_and(|next| split_glibc_floor(next).is_some())
            })
    });
    if !needs_strip {
        return std::borrow::Cow::Borrowed(args);
    }
    let mut stripped = args.to_vec();
    strip_glibc_floor_in_args(&mut stripped);
    std::borrow::Cow::Owned(stripped)
}

/// In-place form of [`args_without_glibc_floor`].
pub fn strip_glibc_floor_in_args(args: &mut [String]) {
    let mut index = 0;
    while index < args.len() {
        if let Some(input) = args[index].strip_prefix("--target=") {
            if let Some((base, _)) = split_glibc_floor(input) {
                args[index] = format!("--target={base}");
            }
        } else if args[index] == "--target" && index + 1 < args.len() {
            if let Some((base, _)) = split_glibc_floor(&args[index + 1]) {
                args[index + 1] = base.to_string();
            }
            index += 1;
        }
        index += 1;
    }
}

fn target_for_known_alias(input: &str) -> Option<&'static str> {
    let lower = input.trim().to_ascii_lowercase();
    canonical_lookup(&lower).or_else(|| synonym_lookup(&lower).and_then(canonical_lookup))
}

/// Synonym → canonical alias table. Every entry's value MUST appear
/// in [`CANONICAL_ALIASES`]'s key column. Tests enforce this.
const SYNONYMS: &[(&str, &str)] = &[
    // Windows
    ("win-x86_64", "win-x64"),
    ("win-amd64", "win-x64"),
    ("windows-x64", "win-x64"),
    ("windows-x86_64", "win-x64"),
    ("windows-amd64", "win-x64"),
    ("win-aarch64", "win-arm64"),
    ("windows-arm64", "win-arm64"),
    ("windows-aarch64", "win-arm64"),
    // macOS
    ("mac-x86_64", "mac-x64"),
    ("mac-amd64", "mac-x64"),
    ("macos-x64", "mac-x64"),
    ("macos-x86_64", "mac-x64"),
    ("darwin-x64", "mac-x64"),
    ("darwin-x86_64", "mac-x64"),
    ("mac-aarch64", "mac-arm64"),
    ("macos-arm64", "mac-arm64"),
    ("macos-aarch64", "mac-arm64"),
    ("darwin-arm64", "mac-arm64"),
    ("darwin-aarch64", "mac-arm64"),
    ("apple-silicon", "mac-arm64"),
    // Linux glibc
    ("linux-x86_64", "linux-x64"),
    ("linux-amd64", "linux-x64"),
    ("linux-aarch64", "linux-arm64"),
    // Linux musl
    ("linux-x86_64-musl", "linux-x64-musl"),
    ("linux-amd64-musl", "linux-x64-musl"),
    ("musl-x64", "linux-x64-musl"),
    ("musl-x86_64", "linux-x64-musl"),
    ("linux-aarch64-musl", "linux-arm64-musl"),
    ("musl-arm64", "linux-arm64-musl"),
    ("musl-aarch64", "linux-arm64-musl"),
];

/// Special aliases that don't map directly to a fixed triple.
const SPECIAL_ALIASES: &[&str] = &["native", "host", "all"];

/// Result of resolving a target string. Carries both the resolved
/// Rust triple and the input form, so error messages can echo what
/// the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// What the user typed verbatim.
    pub input: String,
    /// Resolved Rust target triple, e.g. `x86_64-pc-windows-msvc`.
    pub rust_triple: String,
    /// True iff `input` was a soldr alias (canonical or synonym).
    /// False iff `input` was already a Rust triple.
    pub via_alias: bool,
    /// Requested glibc floor from a `<triple>.<major>.<minor>` input
    /// (soldr#2139). `rust_triple` is always the bare, rustc-legal triple, so
    /// this is the only place the floor survives resolution.
    pub glibc_floor: Option<String>,
}

/// Errors raised when resolution fails.
#[derive(Debug, thiserror::Error)]
pub enum AliasError {
    #[error(
        "soldr build --target `{input}`: not a known alias or Rust triple. \
         Did you mean `{suggestion}`?"
    )]
    Unknown { input: String, suggestion: String },
    #[error(
        "soldr build --target `{input}`: ambiguous — could mean ARM32 \
         (not supported) or ARM64. Use `{disambiguated}` explicitly."
    )]
    Ambiguous {
        input: String,
        disambiguated: String,
    },
    #[error(
        "soldr build --target `{input}`: 32-bit targets are not in soldr's \
         supported set. Did you mean `{suggestion}`?"
    )]
    Thirty2Bit { input: String, suggestion: String },
    #[error(
        "soldr build --target `{input}`: a glibc floor cannot be honoured for \
         `{base}`. soldr enforces a floor by linking through managed zig, which it \
         does only for x86_64-unknown-linux-gnu and aarch64-unknown-linux-gnu; for \
         any other target soldr would have to drop the `.{version}` and ship a \
         binary whose floor is not the one you asked for. Use one of those two \
         triples, or cargo-zigbuild directly. Tracked in soldr#1060 / soldr#2139."
    )]
    GlibcVersioned {
        input: String,
        base: String,
        version: String,
    },
    #[error(
        "soldr build --target `all` is only valid for `soldr prepare --target all`; \
         expand explicitly for `soldr build`."
    )]
    AllNotBuildable,
}

/// One-shot resolver — primary entry point. Pass whatever the user
/// typed; gets back a [`ResolvedTarget`] or an [`AliasError`] with
/// a suggested correction.
///
/// Resolution order:
/// 1. **Lowercase + trim** the input.
/// 2. **Canonical alias** — exact match against [`CANONICAL_ALIASES`].
/// 3. **Synonym** — lookup in [`SYNONYMS`] → canonical → triple.
/// 4. **Special: `native` / `host`** — current host's triple.
/// 5. **Special: `all`** — explicit error (caller must expand).
/// 6. **Rust triple passthrough** — accepted if `looks_like_rust_triple`.
/// 7. **Reject** with [`AliasError::Unknown`] including a
///    Jaro-Winkler best-match suggestion (reuses the `strsim` dep
///    already on the workspace).
pub fn resolve_soldr_target(input: &str) -> Result<ResolvedTarget, AliasError> {
    let raw = input.trim();
    let lower = raw.to_ascii_lowercase();

    // Ambiguity + 32-bit gates fire BEFORE alias lookup so users get
    // the targeted error rather than "unknown" with a fuzzy match.
    if let Some(disambiguated) = check_ambiguous(&lower) {
        return Err(AliasError::Ambiguous {
            input: raw.to_string(),
            disambiguated: disambiguated.to_string(),
        });
    }
    if let Some(sug) = check_thirty2_bit(&lower) {
        return Err(AliasError::Thirty2Bit {
            input: raw.to_string(),
            suggestion: sug.to_string(),
        });
    }
    // soldr#2139. cargo-zigbuild spells an old-glibc target as
    // `<triple>.<major>.<minor>`. Without this gate the suffixed form reaches the
    // sysroot table and fails as "no sqlite sysroot recipe for target
    // x86_64-unknown-linux-gnu.2.17", which reads like a hole in that table
    // rather than an unsupported request -- and invites "just strip the suffix",
    // which would ship a binary whose glibc floor is not the one that was asked
    // for. Say what soldr cannot do, and name the tool that can.
    reject_glibc_versioned(raw)?;

    // soldr#2139: a supported floor survived the gate above. Resolve the base
    // triple through the ordinary path and carry the floor beside it --
    // `rust_triple` stays rustc-legal, so no caller can pass the suffixed
    // spelling to a tool that has never heard of it.
    if let Some((base, floor)) = split_glibc_floor(raw) {
        let mut resolved = resolve_soldr_target(base)?;
        resolved.input = raw.to_string();
        resolved.glibc_floor = Some(floor.to_string());
        return Ok(resolved);
    }

    // Special aliases
    if lower == "native" || lower == "host" {
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: host_triple().to_string(),
            via_alias: true,
            glibc_floor: None,
        });
    }
    if lower == "all" {
        return Err(AliasError::AllNotBuildable);
    }

    // Canonical alias
    if let Some(triple) = canonical_lookup(&lower) {
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: triple.to_string(),
            via_alias: true,
            glibc_floor: None,
        });
    }

    // Synonym → canonical → triple
    if let Some(canonical) = synonym_lookup(&lower) {
        let triple = canonical_lookup(canonical)
            .expect("synonyms table value MUST appear in CANONICAL_ALIASES — test enforced");
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: triple.to_string(),
            via_alias: true,
            glibc_floor: None,
        });
    }

    // Rust triple passthrough — let it through if it looks like one
    if looks_like_rust_triple(&lower) {
        return Ok(ResolvedTarget {
            input: raw.to_string(),
            rust_triple: raw.to_string(),
            via_alias: false,
            glibc_floor: None,
        });
    }

    // Reject with a suggestion
    Err(AliasError::Unknown {
        input: raw.to_string(),
        suggestion: best_match_suggestion(&lower).to_string(),
    })
}

// ---- internal helpers -----------------------------------------

fn canonical_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| CANONICAL_ALIASES.iter().copied().collect())
}

fn synonym_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| SYNONYMS.iter().copied().collect())
}

fn canonical_lookup(input: &str) -> Option<&'static str> {
    canonical_map().get(input).copied()
}

fn synonym_lookup(input: &str) -> Option<&'static str> {
    synonym_map().get(input).copied()
}

/// True iff `input` superficially looks like a Rust target triple.
/// Loose check — 2+ hyphens, only ASCII alnum + `_` + `-` characters.
/// Rust's actual triple grammar is more relaxed than this, but
/// anything that passes the check is forwarded to cargo, which will
/// produce the real validation error if the triple is genuinely
/// unknown.
fn looks_like_rust_triple(input: &str) -> bool {
    if input.matches('-').count() < 2 {
        return false;
    }
    input
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Detect inputs that are ambiguous between 32-bit and 64-bit ARM.
/// Returns the suggested unambiguous alias.
fn check_ambiguous(input: &str) -> Option<&'static str> {
    match input {
        "mac-arm" | "macos-arm" | "darwin-arm" => Some("mac-arm64"),
        "win-arm" | "windows-arm" => Some("win-arm64"),
        "linux-arm" => Some("linux-arm64"),
        _ => None,
    }
}

/// Base triples whose glibc floor soldr can actually honour.
///
/// soldr#2139: these are exactly the triples [`crate::linux_cross`] links
/// through managed zig, which is the only mechanism that can enforce a floor.
/// Accepting a suffix anywhere else would mean stripping it and shipping a
/// binary whose floor is not the one that was asked for -- silently wrong, and
/// worse than refusing.
pub const GLIBC_FLOOR_SUPPORTED_BASES: [&str; 2] =
    ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"];

/// Split `x86_64-unknown-linux-gnu.2.17` into its base triple and the
/// requested glibc floor.
///
/// Anchored on `-linux-gnu` rather than matching any trailing dotted number:
/// musl has no glibc notion, and the other platforms version their SDK
/// differently, so a dot after those is a typo and belongs in the
/// "did you mean" path instead.
///
/// Returns slices of `raw`, so the caller keeps the original casing.
pub fn split_glibc_floor(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    let (base, version) = trimmed.split_once('.')?;
    if !base.to_ascii_lowercase().ends_with("-linux-gnu") {
        return None;
    }
    // Digits and dots only, starting with a digit -- a version, not any suffix.
    let looks_like_version = version.starts_with(|c: char| c.is_ascii_digit())
        && version.chars().all(|c| c.is_ascii_digit() || c == '.');
    looks_like_version.then_some((base, version))
}

/// Whether a floor request against `base` can be honoured rather than ignored.
pub fn glibc_floor_is_supported(base: &str) -> bool {
    let lower = base.trim().to_ascii_lowercase();
    GLIBC_FLOOR_SUPPORTED_BASES.contains(&lower.as_str())
}

/// Detect 32-bit-named inputs and suggest the 64-bit replacement.
/// Soldr deliberately doesn't ship 32-bit triples.
///
/// Reject `<triple>.<major>.<minor>` when the floor cannot be honoured.
///
/// Callable independently of full alias resolution, because the two surfaces
/// resolve targets differently: `soldr prepare` goes through
/// [`resolve_soldr_target`], while `soldr build` only runs
/// [`normalize_target_aliases_in_args`], which rewrites *known* aliases and
/// passes anything else through untouched. Routing `build` through the full
/// resolver instead would make it reject every target not in the alias table,
/// including legitimate custom triples -- so the narrow guard is the one that
/// can be shared safely.
///
/// soldr#2139: without this, `soldr build --target x86_64-unknown-linux-gnu.2.17`
/// reported "no sqlite sysroot recipe for target …" for each library in turn
/// and then *continued*, which reads as a hole in the sysroot table rather
/// than an unsupported request.
pub fn reject_glibc_versioned(raw: &str) -> Result<(), AliasError> {
    match split_glibc_floor(raw) {
        // soldr#2139: honoured on the managed-zig -gnu targets, so the
        // request is real rather than silently dropped. Everywhere else the
        // original refusal stands.
        Some((base, _)) if glibc_floor_is_supported(base) => Ok(()),
        Some((base, version)) => Err(AliasError::GlibcVersioned {
            input: raw.to_string(),
            base: base.to_string(),
            version: version.to_string(),
        }),
        None => Ok(()),
    }
}

fn check_thirty2_bit(input: &str) -> Option<&'static str> {
    match input {
        "win-x86" | "windows-x86" | "win-i686" | "win-i386" => Some("win-x64"),
        "mac-x86" | "macos-x86" | "darwin-x86" | "mac-i686" => Some("mac-x64"),
        "linux-x86" | "linux-i686" | "linux-i386" => Some("linux-x64"),
        _ => None,
    }
}

/// Best Jaro-Winkler match among all canonical aliases + Rust triples
/// we recognize. Used for the "did you mean" suggestion in
/// [`AliasError::Unknown`].
fn best_match_suggestion(input: &str) -> &'static str {
    let mut best: (f64, &'static str) = (-1.0, "win-x64");
    for (alias, _) in CANONICAL_ALIASES {
        let score = strsim::jaro_winkler(input, alias);
        if score > best.0 {
            best = (score, alias);
        }
    }
    for &alias in SPECIAL_ALIASES {
        let score = strsim::jaro_winkler(input, alias);
        if score > best.0 {
            best = (score, alias);
        }
    }
    best.1
}

/// Host triple at runtime — used by `native`/`host` aliases.
fn host_triple() -> &'static str {
    // soldr ships a host-triple constant baked at build time via
    // env! in `core::target_triple` for the actual targeting logic.
    // For the alias resolver we just want the canonical form.
    // CFG-based resolution covers the supported host set; anything
    // else falls back to a sensible default + lets cargo error.
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(canonical_alias_resolves_to_rust_triple, {
        let r = resolve_soldr_target("win-x64").unwrap();
        assert_eq!(r.rust_triple, "x86_64-pc-windows-msvc");
        assert!(r.via_alias);
        let r = resolve_soldr_target("mac-arm64").unwrap();
        assert_eq!(r.rust_triple, "aarch64-apple-darwin");
        assert!(r.via_alias);
    });

    crate::timed_test!(synonym_resolves_to_canonical_triple, {
        let r = resolve_soldr_target("apple-silicon").unwrap();
        assert_eq!(r.rust_triple, "aarch64-apple-darwin");
        let r = resolve_soldr_target("musl-x64").unwrap();
        assert_eq!(r.rust_triple, "x86_64-unknown-linux-musl");
        let r = resolve_soldr_target("linux-amd64-musl").unwrap();
        assert_eq!(r.rust_triple, "x86_64-unknown-linux-musl");
    });

    crate::timed_test!(rust_triple_passthrough, {
        let r = resolve_soldr_target("x86_64-pc-windows-msvc").unwrap();
        assert_eq!(r.rust_triple, "x86_64-pc-windows-msvc");
        assert!(!r.via_alias);
        let r = resolve_soldr_target("wasm32-unknown-unknown").unwrap();
        assert_eq!(r.rust_triple, "wasm32-unknown-unknown");
        assert!(!r.via_alias);
    });

    crate::timed_test!(case_insensitivity, {
        let r = resolve_soldr_target("WIN-X64").unwrap();
        assert_eq!(r.rust_triple, "x86_64-pc-windows-msvc");
        let r = resolve_soldr_target("Apple-Silicon").unwrap();
        assert_eq!(r.rust_triple, "aarch64-apple-darwin");
    });

    crate::timed_test!(ambiguous_arm_rejected_with_suggestion, {
        let err = resolve_soldr_target("mac-arm").unwrap_err();
        match err {
            AliasError::Ambiguous { disambiguated, .. } => {
                assert_eq!(disambiguated, "mac-arm64");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    });

    crate::timed_test!(thirty_two_bit_rejected, {
        let err = resolve_soldr_target("win-x86").unwrap_err();
        match err {
            AliasError::Thirty2Bit { suggestion, .. } => {
                assert_eq!(suggestion, "win-x64");
            }
            other => panic!("expected Thirty2Bit, got {other:?}"),
        }
    });

    crate::timed_test!(the_floor_suffix_is_stripped_from_both_argv_spellings, {
        // The suffix must never reach a cargo child. Both spellings are
        // covered because argv carries whichever the user typed.
        let mut split = vec![
            "build".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu.2.17".to_string(),
            "--release".to_string(),
        ];
        strip_glibc_floor_in_args(&mut split);
        // The borrowing form must agree, since it is what the spawn uses.
        assert_eq!(args_without_glibc_floor(&split).as_ref(), split.as_slice());
        assert_eq!(split[2], "x86_64-unknown-linux-gnu");
        assert_eq!(split[3], "--release", "unrelated args must be untouched");

        let mut joined = vec![
            "build".to_string(),
            "--target=aarch64-unknown-linux-gnu.2.28".to_string(),
        ];
        strip_glibc_floor_in_args(&mut joined);
        assert_eq!(joined[1], "--target=aarch64-unknown-linux-gnu");
    });

    crate::timed_test!(stripping_leaves_ordinary_targets_alone, {
        // Including musl, which has no glibc to floor, and a bare `--target`
        // at the end of argv, which must not index past the end.
        let mut args = vec![
            "build".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
        ];
        strip_glibc_floor_in_args(&mut args);
        assert_eq!(args[2], "x86_64-unknown-linux-musl");

        let mut trailing = vec!["build".to_string(), "--target".to_string()];
        strip_glibc_floor_in_args(&mut trailing);
        assert_eq!(trailing, vec!["build".to_string(), "--target".to_string()]);
    });

    crate::timed_test!(a_supported_glibc_floor_resolves_to_the_bare_triple, {
        // soldr#2139: the zigbuild spelling for an old-glibc target. soldr
        // honours it on the two triples it links through managed zig, which is
        // the only mechanism that can enforce a floor.
        //
        // `rust_triple` must come back *bare*: rustc has never heard of the
        // suffixed spelling, so a caller that forwards it to cargo would fail
        // with a confusing "unknown target" a long way from here.
        for (input, base, floor) in [
            (
                "x86_64-unknown-linux-gnu.2.17",
                "x86_64-unknown-linux-gnu",
                "2.17",
            ),
            (
                "aarch64-unknown-linux-gnu.2.28",
                "aarch64-unknown-linux-gnu",
                "2.28",
            ),
        ] {
            let resolved = resolve_soldr_target(input).unwrap();
            assert_eq!(resolved.rust_triple, base, "{input}");
            assert_eq!(resolved.glibc_floor.as_deref(), Some(floor), "{input}");
            // The input is echoed verbatim so errors can quote what was typed.
            assert_eq!(resolved.input, input);
        }
    });

    crate::timed_test!(a_floor_on_an_unenforceable_target_is_still_rejected, {
        // The floor is only meaningful where soldr links through managed zig.
        // Anywhere else soldr would have to drop the suffix and ship a binary
        // whose floor is not the one that was asked for -- silently wrong, and
        // strictly worse than refusing.
        let err = resolve_soldr_target("i686-unknown-linux-gnu.2.17").unwrap_err();
        match &err {
            AliasError::GlibcVersioned { base, version, .. } => {
                assert_eq!(base, "i686-unknown-linux-gnu");
                assert_eq!(version, "2.17");
            }
            other => panic!("expected GlibcVersioned, got {other:?}"),
        }

        // The message has to name the way forward, or the user is stuck.
        let rendered = err.to_string();
        assert!(
            rendered.contains("zigbuild"),
            "the error must point at a tool that can do it: {rendered}"
        );
        assert!(
            rendered.contains("x86_64-unknown-linux-gnu")
                && rendered.contains("aarch64-unknown-linux-gnu"),
            "the error must name the triples where a floor *is* honoured: {rendered}"
        );
    });

    crate::timed_test!(only_glibc_triples_take_the_versioned_path, {
        // The gate keys on `-linux-gnu`, so nothing else is diverted into an
        // error about a libc it does not have.
        for input in [
            "x86_64-unknown-linux-musl.2.17",
            "x86_64-apple-darwin.11.0",
            "x86_64-unknown-linux-gnu.foo",
        ] {
            let err = resolve_soldr_target(input).unwrap_err();
            assert!(
                !matches!(err, AliasError::GlibcVersioned { .. }),
                "{input} must not be treated as glibc-versioned, got {err:?}"
            );
        }

        // And the plain triple still resolves, i.e. the gate did not widen.
        let ok = resolve_soldr_target("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(ok.rust_triple, "x86_64-unknown-linux-gnu");
    });

    // soldr#2139: `soldr build` never calls `resolve_soldr_target` -- it only
    // runs `normalize_target_aliases_in_args`, which passes an unrecognised
    // target straight through. So the guard is also exposed on its own and
    // called from every blessed-prep entry. These pin that the standalone
    // form agrees with the resolver in both directions.
    crate::timed_test!(the_standalone_guard_rejects_what_the_resolver_rejects, {
        // soldr#2139: the guard now splits on whether the floor can be
        // *enforced*, so it must agree with the resolver in both directions.
        let err = reject_glibc_versioned("i686-unknown-linux-gnu.2.17").unwrap_err();
        match err {
            AliasError::GlibcVersioned {
                input,
                base,
                version,
            } => {
                assert_eq!(input, "i686-unknown-linux-gnu.2.17");
                assert_eq!(base, "i686-unknown-linux-gnu");
                assert_eq!(version, "2.17");
            }
            other => panic!("expected GlibcVersioned, got {other:?}"),
        }
        // Case and surrounding whitespace must not smuggle an unenforceable
        // floor past the guard.
        assert!(reject_glibc_versioned("  I686-Unknown-Linux-GNU.2.17 ").is_err());
        assert!(resolve_soldr_target("i686-unknown-linux-gnu.2.17").is_err());

        // ...and the enforceable ones pass both, however they are spelled.
        assert!(reject_glibc_versioned("  X86_64-Unknown-Linux-GNU.2.17 ").is_ok());
        assert!(reject_glibc_versioned("aarch64-unknown-linux-gnu.2.28").is_ok());
        assert!(resolve_soldr_target("aarch64-unknown-linux-gnu.2.28").is_ok());
    });

    crate::timed_test!(the_standalone_guard_does_not_over_reject, {
        // This one runs on *every* prepared target, so a false positive would
        // break builds that work today -- including custom triples that the
        // alias table has never heard of and must not be judged on.
        for input in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl.2.17",
            "x86_64-apple-darwin.11.0",
            "x86_64-unknown-linux-gnu.foo",
            "thumbv7em-none-eabihf",
            "some-vendor-custom-linux-gnueabi",
        ] {
            assert!(
                reject_glibc_versioned(input).is_ok(),
                "{input} must pass the guard untouched",
            );
        }
    });

    crate::timed_test!(all_alias_rejected_for_build, {
        let err = resolve_soldr_target("all").unwrap_err();
        assert!(matches!(err, AliasError::AllNotBuildable));
    });

    crate::timed_test!(unknown_input_carries_jaro_winkler_suggestion, {
        let err = resolve_soldr_target("win-arm6").unwrap_err();
        match err {
            AliasError::Unknown { suggestion, .. } => {
                assert_eq!(suggestion, "win-arm64");
            }
            other => panic!("expected Unknown with suggestion, got {other:?}"),
        }
    });

    crate::timed_test!(native_resolves_to_a_real_triple, {
        let r = resolve_soldr_target("native").unwrap();
        assert!(r.via_alias);
        // Resolved triple varies by host; just confirm it's non-empty
        // and looks like a triple.
        assert!(looks_like_rust_triple(&r.rust_triple));
    });

    crate::timed_test!(synonyms_table_targets_are_all_canonical, {
        // Static invariant: every synonym value must appear as a key
        // in CANONICAL_ALIASES. If this fails, the resolver's
        // synonym → canonical → triple chain panics in production.
        let canonical_keys: std::collections::HashSet<&str> =
            CANONICAL_ALIASES.iter().map(|(k, _)| *k).collect();
        for (syn, canonical_target) in SYNONYMS {
            assert!(
                canonical_keys.contains(canonical_target),
                "synonym `{syn}` → `{canonical_target}` is not a canonical alias key"
            );
        }
    });

    crate::timed_test!(canonical_aliases_match_canonical_targets_const, {
        // Soldr ships TWO related canonical-list tables:
        //   - crate::core::CANONICAL_TARGETS (Rust triples)
        //   - target_alias::CANONICAL_ALIASES (alias → triple)
        // Their VALUES (Rust triples) must agree as a set.
        let triples_const: std::collections::HashSet<&str> =
            crate::core::CANONICAL_TARGETS.iter().copied().collect();
        let triples_alias: std::collections::HashSet<&str> =
            CANONICAL_ALIASES.iter().map(|(_, t)| *t).collect();
        assert_eq!(triples_const, triples_alias);
    });

    crate::timed_test!(canonical_aliases_normalize_in_cargo_arguments, {
        for (alias, triple) in CANONICAL_ALIASES {
            let mut split = vec![
                "build".to_string(),
                "--target".to_string(),
                alias.to_string(),
            ];
            normalize_target_aliases_in_args(&mut split);
            assert_eq!(split[2], *triple, "split --target form drifted for {alias}");

            let mut equals = vec!["build".to_string(), format!("--target={alias}")];
            normalize_target_aliases_in_args(&mut equals);
            assert_eq!(
                equals[1],
                format!("--target={triple}"),
                "equals --target form drifted for {alias}"
            );
        }
    });
}
