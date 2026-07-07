//! `soldr prepare --target <triple>` — uniform cross-compile toolchain bootstrap.
//!
//! Single CLI surface for every cross-compile target: same invocation
//! shape, only `--target` varies. Internally dispatches based on the
//! target triple:
//!
//! - `*-pc-windows-msvc` → ensure cargo-xwin + LLVM toolchain + extract
//!   the vendored xwin MSVC CRT cache from soldr-toolchain assets to
//!   `~/.cache/cargo-xwin/` so `cargo xwin build` skips the 15-min live
//!   Microsoft download.
//! - `x86_64-pc-windows-gnu` on Windows x64 -> ensure managed
//!   MinGW-w64 GCC, prepend it to PATH, and export target-scoped
//!   Cargo/cc-rs env. Other hosts keep using cargo-zigbuild + zig.
//! - `*-apple-darwin` → ensure the target-shaped Apple SDK and print
//!   `SDKROOT=<path>` so the caller can plumb it into `$GITHUB_ENV`.
//!   `soldr build --target` is the blessed Darwin cross-build path;
//!   prepare still materializes zig for legacy/external tooling.
//! - `*-unknown-linux-{gnu,musl}` (when triple ≠ host) → ensure
//!   cargo-zigbuild + zig.
//! - All targets: `rustup target add <triple>`.
//!
//! Designed to collapse the per-step ad-hoc downloads in
//! `cross-compile-all-targets.yml` into a single "Preparing Cross
//! Compile Toolchain" step.
//!
//! Output goes to stdout (human-readable). When `--github-env` is set,
//! also appends `KEY=VALUE` lines (e.g. `SDKROOT=/opt/...`) to that
//! file so a GitHub-Actions runner can pick them up in $GITHUB_ENV.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::{SoldrError, SoldrPaths};
use crate::fetch::github::http_client;
use crate::fetch::{ensure_apple_sdk, ensure_llvm_toolchain, ensure_zig};
use wait_timeout::ChildExt;

/// URL of the vendored cargo-xwin MSVC CRT + Windows SDK cache on
/// soldr-toolchain assets. PR #890 packaged 1.08 GiB of MSVC CRT +
/// Windows SDK headers/libs as a single 81 MiB zstd-19 tarball; this
/// extracts into `~/.cache/cargo-xwin/xwin/{crt,sdk}` so subsequent
/// `cargo xwin build` invocations skip the live Microsoft download.
pub const XWIN_CACHE_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-x86_64-msvc/xwin-cache.tar.zst";

/// Pinned sha256 of the xwin cache tar.zst. Mismatch is a hard error.
pub const XWIN_CACHE_SHA256: &str =
    "33c04d8026d99dab4d66f39ddbd93d75f64c68063d4ba58e5450626524bf348d";

const RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR: &str = "SOLDR_RUSTUP_TARGET_ADD_TIMEOUT_SECS";
const DEFAULT_RUSTUP_TARGET_ADD_TIMEOUT_SECS: u64 = 15 * 60;
const KILLED_RUSTUP_TARGET_ADD_REAP_TIMEOUT_SECS: u64 = 5;

/// Append `KEY=VALUE` to the file at `path` (creating it if needed).
/// No-op when `path` is `None`. Used so callers running under GitHub
/// Actions can pipe env vars (SDKROOT, etc.) into `$GITHUB_ENV`.
fn append_env(path: Option<&Path>, key: &str, value: &str) -> Result<(), SoldrError> {
    if let Some(p) = path {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| SoldrError::Other(format!("open {}: {e}", p.display())))?;
        writeln!(f, "{key}={value}")
            .map_err(|e| SoldrError::Other(format!("write {}: {e}", p.display())))?;
    }
    Ok(())
}

/// Resolve the xwin cache root cargo-xwin uses. cargo-xwin's default is
/// `dirs::cache_dir().join("cargo-xwin")`. On Linux that resolves to
/// `$XDG_CACHE_HOME/cargo-xwin` (when set) or `$HOME/.cache/cargo-xwin`.
/// We mirror that here so the extraction lands at the exact path
/// cargo-xwin will look in.
fn xwin_cache_root() -> Result<PathBuf, SoldrError> {
    let home = crate::core::home_dir()?;
    #[cfg(target_os = "linux")]
    let cache_base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".cache"),
    };
    #[cfg(target_os = "macos")]
    let cache_base = home.join("Library").join("Caches");
    #[cfg(target_os = "windows")]
    let cache_base = match std::env::var_os("LOCALAPPDATA") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join("AppData").join("Local"),
    };
    Ok(cache_base.join("cargo-xwin"))
}

/// Extract the vendored xwin cache into the location cargo-xwin reads
/// by default. After extraction the layout is
/// `<cache>/xwin/{crt,sdk,DONE,...}`. The DONE file in the tarball was
/// written by `cargo xwin cache xwin` itself when we pre-populated it
/// — its first line lists the arches (`x86_64 aarch64`) and subsequent
/// lines list every file downloaded. cargo-xwin reads that exact
/// format to decide whether to skip the live download.
///
/// IMPORTANT: do NOT overwrite `<cache>/xwin/DONE` after extraction.
/// The tarball already contains the correct cargo-xwin marker; writing
/// anything else there (e.g. a sha256 hex string as we used to) makes
/// cargo-xwin treat the first line as an unknown "arch", fail the
/// arch-match check, and re-download MSVC CRT live (15min wasted).
/// We use `.soldr-extracted` as a sibling marker for our own
/// idempotence instead.
async fn ensure_xwin_cache() -> Result<PathBuf, SoldrError> {
    let cache_root = xwin_cache_root()?;
    let xwin_dir = cache_root.join("xwin");
    // Sibling marker for our own already-extracted check — must NOT
    // be `DONE` (cargo-xwin owns that filename) and must NOT be inside
    // `xwin/` if any file there is being checksummed by cargo-xwin.
    let soldr_marker = cache_root.join(".soldr-xwin-extracted");

    if soldr_marker.is_file() && xwin_dir.join("crt").join("include").is_dir() {
        let aliases = ensure_xwin_case_aliases(&xwin_dir)?;
        if aliases > 0 {
            eprintln!("soldr prepare: added {aliases} lowercase xwin cache aliases");
        }
        eprintln!(
            "soldr prepare: xwin cache already present at {}",
            xwin_dir.display()
        );
        return Ok(xwin_dir);
    }

    eprintln!("soldr prepare: fetching xwin MSVC CRT + Windows SDK cache from {XWIN_CACHE_URL}...");
    let client = http_client()?;
    let resp = client
        .get(XWIN_CACHE_URL)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "xwin cache download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    let digest = crate::fetch::trust::sha256_of(&bytes);
    if digest != XWIN_CACHE_SHA256 {
        return Err(SoldrError::Other(format!(
            "xwin cache sha256 mismatch: expected {XWIN_CACHE_SHA256}, got {digest}"
        )));
    }
    eprintln!("soldr prepare: trust: verified xwin cache sha256={digest}");

    std::fs::create_dir_all(&cache_root)?;
    let reader = std::io::Cursor::new(&bytes[..]);
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    archive
        .unpack(&cache_root)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    let aliases = ensure_xwin_case_aliases(&xwin_dir)?;
    if aliases > 0 {
        eprintln!("soldr prepare: added {aliases} lowercase xwin cache aliases");
    }
    // Drop our own marker SIBLING to xwin/, not inside it — see the
    // doc comment above for why clobbering xwin/DONE was a 15-min CI
    // regression.
    std::fs::write(&soldr_marker, XWIN_CACHE_SHA256)?;
    eprintln!(
        "soldr prepare: extracted xwin cache to {} (size {} bytes)",
        xwin_dir.display(),
        bytes.len()
    );
    Ok(xwin_dir)
}

/// Case-alias materialization is shared with the blessed
/// `fetch::xwin_cache` path — same bundle shape, same case-sensitivity
/// problem, one implementation (cross-run 28574600982 fix). Covers both the lowercase
/// pass (`Kernel32.Lib` → `kernel32.lib`) and the include-referenced
/// pass (`driverspecs.h` → `DriverSpecs.h`, which `kernelspecs.h`
/// references as `#include "DriverSpecs.h"` from every `windows.h`
/// compile).
use crate::fetch::xwin_cache::ensure_xwin_case_aliases;

/// Parse the `--target` argument into a list of triples.
///
/// Accepts three shapes — see `cli_args::Commands::Prepare::target` for
/// the user-facing documentation. The literal `all` is a sentinel that
/// callers must expand via `cargo_metadata_soldr::resolve_all_targets`;
/// the dispatch returns `Err(SentinelAll)` so callers branch explicitly.
///
/// Comma-separated entries are trimmed and empty tokens dropped; an
/// empty effective list errors instead of silently producing a zero-
/// target run.
pub fn parse_target_arg(target: &str) -> Result<ParsedTargetArg, SoldrError> {
    if target == "all" {
        return Ok(ParsedTargetArg::All);
    }
    if target.contains(',') {
        let parsed: Vec<String> = target
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if parsed.is_empty() {
            return Err(SoldrError::Other(
                "soldr prepare --target: comma-separated list was empty".to_string(),
            ));
        }
        return Ok(ParsedTargetArg::Explicit(parsed));
    }
    Ok(ParsedTargetArg::Explicit(vec![target.to_string()]))
}

/// Result of parsing the `--target` CLI argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedTargetArg {
    /// `--target all` — caller must resolve from `Cargo.toml`.
    All,
    /// One or more explicit triples (single or comma-separated form).
    Explicit(Vec<String>),
}

/// Top-level entry point for `soldr prepare --target <triple>`.
pub async fn run(
    target: String,
    github_env: Option<PathBuf>,
    save: Option<PathBuf>,
    restore: Option<PathBuf>,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let github_env_path = github_env.as_deref();

    // Classify the triple up front so the dispatch below + the
    // post-restore audit + any future per-target cache namespacing all
    // share the same source of truth. Unknown triples ERROR here
    // instead of silently falling through to a no-op.
    let attrs = classify_target(&target)?;

    eprintln!("soldr prepare: target={target}");

    // `--restore`: extract a previously-saved archive of soldr-managed
    // prepare state (zig, LLVM, Apple SDK, xwin cache) BEFORE running
    // the normal prepare flow. Anything still missing afterwards gets
    // downloaded by the normal dispatch below. Restore failures are
    // logged but non-fatal — partial cache hits still help.
    //
    // After restore, walk the expected paths for `target` and emit a
    // present/missing summary so consumers can see whether the cache
    // covered everything or the dispatch will need to re-download
    // pieces. Missing paths are NOT an error — they just trigger
    // normal downloads via the dispatch below (#900 acceptance).
    if let Some(archive) = restore.as_deref() {
        match restore_prepare_state(archive, &paths) {
            Ok(()) => eprintln!("soldr prepare: restored state from {}", archive.display()),
            Err(e) => eprintln!(
                "soldr prepare: warning: restore from {} failed: {e}; will re-download as needed",
                archive.display()
            ),
        }
        // Emit the audit even if restore raised — partial restores are
        // useful and the dispatch fills any remaining gaps. The
        // present/missing summary lets consumers see exactly which
        // pieces survived.
        let report = expected_state_paths(&attrs, &paths)?;
        emit_restore_report(&report);
    }

    // Always add the rustup target (idempotent).
    if let Err(e) = rustup_add_target(&target) {
        eprintln!("soldr prepare: warning: rustup target add failed: {e}");
    }

    // soldr#940 — assets within a target's dispatch are fetched
    // concurrently via `tokio::try_join!`. Each ensure_* call already
    // implements its own integrity verification + caching, so racing
    // them only affects net-bandwidth ordering — not the on-disk
    // result.
    match attrs.os {
        TargetOs::Windows => {
            match attrs.abi {
                Some(TargetAbi::Msvc) => {
                    // Windows MSVC cross-compile path: needs cargo-xwin, LLVM
                    // toolchain (for clang/lld-link), and the MSVC CRT cache.
                    // Run LLVM and the xwin cache extract concurrently.
                    eprintln!("soldr prepare: dispatch=xwin (parallel)");
                    let llvm_fut = async {
                        match ensure_llvm_toolchain(&paths).await {
                            Ok(p) => Ok::<PathBuf, SoldrError>(p),
                            Err(SoldrError::UnsupportedPlatform(m)) => {
                                eprintln!(
                                    "soldr prepare: LLVM auto-bootstrap not supported on host: {m}"
                                );
                                Ok(PathBuf::new())
                            }
                            Err(e) => Err(e),
                        }
                    };
                    let xwin_fut = ensure_xwin_cache();
                    let (llvm_dir, _) = tokio::try_join!(llvm_fut, xwin_fut)?;
                    if !llvm_dir.as_os_str().is_empty() {
                        eprintln!("soldr prepare: LLVM toolchain at {}", llvm_dir.display());
                    }
                }
                Some(TargetAbi::Gnu) => {
                    if crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_gcc() {
                        eprintln!("soldr prepare: dispatch=mingw-w64-gcc");
                        let mingw_root =
                            crate::fetch::mingw_w64_gcc::ensure_mingw_w64_gcc(&paths, &target)
                                .await?;
                        let mingw_bin = crate::fetch::mingw_w64_gcc::bin_dir(&mingw_root);
                        eprintln!("soldr prepare: MinGW-w64 GCC at {}", mingw_root.display());
                        println!("MINGW_W64_GCC_ROOT={}", mingw_root.display());
                        println!("MINGW_W64_GCC_BIN={}", mingw_bin.display());
                        for (key, value) in
                            crate::fetch::mingw_w64_gcc::env_for_target(&mingw_root, &target)
                        {
                            append_env(github_env_path, &key, &value)?;
                        }
                        let path_value = crate::fetch::mingw_w64_gcc::path_with_mingw_bin(
                            &mingw_root,
                            std::env::var_os("PATH"),
                        )?;
                        std::env::set_var("PATH", &path_value);
                        append_env(github_env_path, "PATH", &path_value)?;
                    } else {
                        eprintln!(
                            "soldr prepare: dispatch=zigbuild (managed MinGW-w64 GCC is scoped to Windows x64 hosts)"
                        );
                        let zig_dir = ensure_zig(&paths).await?;
                        eprintln!("soldr prepare: zig at {}", zig_dir.display());
                    }
                }
                _ => unreachable!("classify_target rejects Windows without a supported ABI"),
            }
        }
        TargetOs::Darwin => {
            // Darwin prepare exports the target-shaped Apple SDK for
            // callers that need SDKROOT directly. Zig is still fetched
            // here for explicit legacy/external cargo-zigbuild use; the
            // blessed `soldr build` path injects clang/SDK env internally.
            eprintln!("soldr prepare: dispatch=apple-sdk+legacy-zig (parallel)");
            let (zig_dir, sdk) =
                tokio::try_join!(ensure_zig(&paths), ensure_apple_sdk(&paths, Some(&target)))?;
            eprintln!("soldr prepare: zig at {}", zig_dir.display());
            eprintln!("soldr prepare: Apple SDK at {}", sdk.display());
            let sdk_str = sdk.to_string_lossy();
            println!("SDKROOT={sdk_str}");
            append_env(github_env_path, "SDKROOT", &sdk_str)?;
        }
        TargetOs::Linux => {
            // Linux cross-compile via zigbuild (musl always, gnu when
            // host != target arch). Only one asset to fetch; no parallel
            // bench yet — leave as-is.
            eprintln!("soldr prepare: dispatch=zigbuild");
            let zig_dir = ensure_zig(&paths).await?;
            eprintln!("soldr prepare: zig at {}", zig_dir.display());
        }
    }

    // `--save`: capture the prepared state into a tar.zst that callers
    // can plug into `actions/cache@v4`'s save step. Subsequent CI runs
    // pass the same path to `--restore` and skip the live downloads.
    if let Some(archive) = save.as_deref() {
        save_prepare_state(archive, &paths)?;
        eprintln!("soldr prepare: saved state to {}", archive.display());
    }

    eprintln!("soldr prepare: done");
    Ok(())
}

/// One row in the per-target post-restore validation report.
/// `present` is true when the expected path exists on disk; the
/// `path` field is the location consumers can grep for in logs.
#[derive(Debug, Clone)]
struct RestoreEntry {
    label: String,
    path: PathBuf,
    present: bool,
}

/// CPU architecture tag in a Rust target triple. Restricted to the
/// arches soldr's cross-compile bootstrap supports; anything else
/// causes `classify_target` to ERROR rather than falling through
/// to a no-op dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X86_64,
    Aarch64,
}

/// OS family in a Rust target triple. Limited to the cross-compile
/// destinations soldr supports today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Windows,
    Darwin,
    Linux,
}

/// ABI suffix in a Rust target triple. `None` for darwin (no ABI
/// suffix in apple triples). `Msvc`/`Gnu`/`Musl` for the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAbi {
    Msvc,
    Gnu,
    Musl,
}

/// Result of classifying a Rust target triple into the soldr
/// bootstrap dispatch attributes. Replaces the ad-hoc
/// `target.ends_with("...")` checks with a single source-of-truth
/// classifier — every consumer (the dispatch in `run()`, the
/// restore-audit's `expected_state_paths`, future per-target cache
/// namespacing) reads off the same struct.
///
/// `classify_target` ERRORs on unknown triples rather than silently
/// returning an empty attribute set, so a typo in CI YAML surfaces
/// loudly instead of pretending the prepare succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAttrs {
    /// The canonical triple as supplied.
    pub triple: String,
    pub arch: TargetArch,
    pub os: TargetOs,
    pub abi: Option<TargetAbi>,
    /// Needs zig on PATH for prepare-time legacy/external flows. True
    /// for cross-compile to darwin or to a non-host linux flavor.
    pub needs_zig: bool,
    /// Needs the vendored MSVC CRT + Windows SDK cache extracted to
    /// `~/.cache/cargo-xwin/`. True for `*-pc-windows-msvc`.
    pub needs_xwin_cache: bool,
    /// Needs the soldr-managed LLVM toolchain (clang / lld-link /
    /// llvm-lib). True for `*-pc-windows-msvc` (cargo-xwin uses it).
    pub needs_llvm_toolchain: bool,
    /// Needs the soldr-managed MinGW-w64 GCC toolchain on supported
    /// hosts. True for `x86_64-pc-windows-gnu`.
    pub needs_mingw_w64_gcc: bool,
    /// Needs the vendored Apple SDK (IOKit/CoreFoundation/...).
    /// True for `*-apple-darwin`.
    pub needs_apple_sdk: bool,
}

/// Registry of known arch tokens. The classifier scores user input
/// against every entry and picks the best match — so adding a new
/// alias (e.g. `("x86_AMD", TargetArch::X86_64)`) automatically
/// works without changing the matching logic. Order doesn't matter;
/// scoring is symmetric.
pub const KNOWN_ARCHES: &[(&str, TargetArch)] = &[
    ("x86_64", TargetArch::X86_64),
    ("aarch64", TargetArch::Aarch64),
];

/// Registry of known OS tokens. See `KNOWN_ARCHES`.
pub const KNOWN_OSES: &[(&str, TargetOs)] = &[
    ("windows", TargetOs::Windows),
    ("darwin", TargetOs::Darwin),
    ("linux", TargetOs::Linux),
];

/// Registry of known ABI tokens. See `KNOWN_ARCHES`.
pub const KNOWN_ABIS: &[(&str, TargetAbi)] = &[
    ("msvc", TargetAbi::Msvc),
    ("gnu", TargetAbi::Gnu),
    ("musl", TargetAbi::Musl),
];

/// Minimum fuzzy score (0.0–1.0) for a token to be accepted as a
/// match. Exact matches score 1.0; case-insensitive exact 0.99; so
/// any threshold below 0.99 still admits casing variation. Below
/// 0.95 admits one-character extensions like `x86_64h` (Haswell+)
/// silently routing to `x86_64` — confirmed against the rustc
/// target list. The current value rejects every observed close-but-
/// wrong arch in the corpus while still accepting case variation
/// and any registered alias as an exact match.
///
/// Why not "exact match only"? The registry-of-aliases design (see
/// `best_match`) explicitly wants a future `("x86_AMD",
/// TargetArch::X86_64)` entry to work without re-coding the match
/// arm. Fuzzy scoring with a high threshold gives us that property
/// AND case-insensitivity in one knob.
pub const FUZZY_MATCH_THRESHOLD: f64 = 0.95;

/// Score `input` against `candidate` for fuzzy triple-component
/// matching. Returns `1.0` for an exact match (case-insensitive
/// 0.99), otherwise a blend of common-prefix ratio (50%) and
/// Jaro-Winkler (50%) — prefix weighting matters because arch/os/abi
/// tokens are short, so a shared "x86_" prefix carries more signal
/// than character-level edit distance alone.
///
/// Example: `fuzzy_score("x86_AMD", "x86")` and `fuzzy_score(
/// "x86_AMD", "x86_AMD")` both return non-zero, but the exact match
/// scores higher (1.0 vs ~0.65). The user-driven design constraint
/// is: when multiple registry entries match an input, the best one
/// wins; the brittle `match input { "x86_64" => ... }` it replaces
/// returned `Err` for every input that wasn't an exact key.
pub fn fuzzy_score(input: &str, candidate: &str) -> f64 {
    if input == candidate {
        return 1.0;
    }
    let i = input.to_ascii_lowercase();
    let c = candidate.to_ascii_lowercase();
    if i == c {
        return 0.99;
    }
    let common = i.bytes().zip(c.bytes()).take_while(|(a, b)| a == b).count();
    let max_len = i.len().max(c.len()).max(1);
    let prefix_score = common as f64 / max_len as f64;
    let jw = strsim::jaro_winkler(&i, &c);
    0.5 * prefix_score + 0.5 * jw
}

/// Find the best-matching registry entry for `input`. Returns the
/// matched value plus the runner-up name + score (useful for error
/// messages). Errors when the best score is below
/// `FUZZY_MATCH_THRESHOLD` — the registry doesn't have a candidate
/// close enough to commit to.
pub fn best_match<T: Copy>(
    input: &str,
    registry: &[(&str, T)],
    kind: &str,
) -> Result<T, SoldrError> {
    let mut best: Option<(&str, T, f64)> = None;
    for (name, val) in registry {
        let s = fuzzy_score(input, name);
        let take = match best {
            None => true,
            Some((_, _, prev)) => s > prev,
        };
        if take {
            best = Some((name, *val, s));
        }
    }
    let (name, val, score) = best
        .ok_or_else(|| SoldrError::Other(format!("soldr prepare: empty {kind} registry — bug")))?;
    if score < FUZZY_MATCH_THRESHOLD {
        let supported: Vec<&str> = registry.iter().map(|(n, _)| *n).collect();
        return Err(SoldrError::Other(format!(
            "soldr prepare: {kind} `{input}` did not match any known {kind} \
             (closest: `{name}` score={score:.3}; supported: {})",
            supported.join(", ")
        )));
    }
    Ok(val)
}

/// Classify a Rust target triple into a `TargetAttrs`. Pattern is
/// the standard `<arch>-<vendor>-<os>[-<abi>]` triple structure
/// LLVM uses; `vendor` is ignored (always `pc`, `apple`, or
/// `unknown` for the targets we care about). Each component is
/// fuzzy-matched against its registry via `best_match`, so adding
/// a new alias to e.g. `KNOWN_ARCHES` automatically extends the
/// classifier without touching this function. Triples that don't
/// score above `FUZZY_MATCH_THRESHOLD` against any registry entry
/// surface as a hard error naming the closest candidate.
pub fn classify_target(triple: &str) -> Result<TargetAttrs, SoldrError> {
    // Tokenize on `-`. Accept `<arch>-<vendor>-<os>` (3 parts, darwin)
    // and `<arch>-<vendor>-<os>-<abi>` (4 parts, windows/linux).
    let parts: Vec<&str> = triple.split('-').collect();
    if parts.len() < 3 || parts.len() > 4 {
        return Err(SoldrError::Other(format!(
            "soldr prepare: unrecognized target triple shape: `{triple}` \
             (expected `<arch>-<vendor>-<os>[-<abi>]`)"
        )));
    }
    let arch = best_match(parts[0], KNOWN_ARCHES, "arch")?;
    let vendor = parts[1];
    let os = best_match(parts[2], KNOWN_OSES, "os")?;
    // Vendor must match the canonical (os, vendor) pair soldr ships
    // against. Without this check `x86_64-uwp-windows-msvc`,
    // `x86_64-win7-windows-msvc`, `x86_64-unikraft-linux-musl`, and
    // `x86_64-pc-linux-gnu` all get wrongly accepted because the
    // classifier only looked at arch/os/abi. See the rustc target
    // list — UWP, Windows 7, Unikraft, etc. are real targets we
    // don't support.
    let expected_vendor = match os {
        TargetOs::Windows => "pc",
        TargetOs::Darwin => "apple",
        TargetOs::Linux => "unknown",
    };
    if !vendor.eq_ignore_ascii_case(expected_vendor) {
        return Err(SoldrError::Other(format!(
            "soldr prepare: triple `{triple}` has vendor `{vendor}`; \
             expected `{expected_vendor}` for the {os:?} family"
        )));
    }
    let abi = match parts.get(3) {
        Some(s) => Some(best_match(s, KNOWN_ABIS, "abi")?),
        None => None,
    };
    // Shape constraint: darwin has no ABI suffix, windows/linux require
    // one. Reject mismatches here — fuzzy matching can't catch
    // cross-component invariants.
    match (os, abi) {
        (TargetOs::Darwin, Some(_)) => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: `*-apple-darwin` triples take no ABI suffix; got `{triple}`"
            )));
        }
        (TargetOs::Windows, None) | (TargetOs::Linux, None) => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: `{}` triples require an ABI suffix; got `{triple}`",
                match os {
                    TargetOs::Windows => "windows",
                    TargetOs::Linux => "linux",
                    TargetOs::Darwin => unreachable!(),
                }
            )));
        }
        // Disallow windows-musl and linux-msvc. Windows GNU is
        // supported for x86_64 only today; other Windows GNU-family
        // triples (aarch64 gnullvm, i686 gnu, etc.) remain explicit
        // follow-ups rather than silently accepting a no-op.
        (TargetOs::Windows, Some(TargetAbi::Musl)) | (TargetOs::Linux, Some(TargetAbi::Msvc)) => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: unsupported os/abi combination in `{triple}` \
                 (supported: windows-msvc, x86_64 windows-gnu, darwin, linux-gnu, linux-musl)"
            )));
        }
        (TargetOs::Windows, Some(TargetAbi::Gnu)) if arch != TargetArch::X86_64 => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: unsupported Windows GNU target `{triple}` \
                 (first-class MinGW provisioning currently supports only x86_64-pc-windows-gnu)"
            )));
        }
        _ => {}
    }
    Ok(TargetAttrs {
        triple: triple.to_string(),
        arch,
        os,
        abi,
        needs_zig: matches!(os, TargetOs::Darwin | TargetOs::Linux),
        needs_xwin_cache: matches!((os, abi), (TargetOs::Windows, Some(TargetAbi::Msvc))),
        needs_llvm_toolchain: matches!((os, abi), (TargetOs::Windows, Some(TargetAbi::Msvc))),
        needs_mingw_w64_gcc: matches!(
            (os, abi, arch),
            (TargetOs::Windows, Some(TargetAbi::Gnu), TargetArch::X86_64)
        ),
        needs_apple_sdk: matches!(os, TargetOs::Darwin),
    })
}

/// List the on-disk paths that `prepare --target <triple>` is
/// expected to populate. Used after `--restore` to surface a
/// present/missing summary; the dispatch below downloads anything
/// missing so the report is purely informational.
///
/// Paths are version-pinned where possible (e.g. zig 0.13.0, LLVM
/// 21.1.5) so a stale archive that's missing the current pin is
/// reported as "missing" even if an older version exists on disk.
fn expected_state_paths(
    attrs: &TargetAttrs,
    paths: &SoldrPaths,
) -> Result<Vec<RestoreEntry>, SoldrError> {
    let mut entries = Vec::new();
    let windows_gnu_uses_zig = attrs.needs_mingw_w64_gcc
        && !crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_gcc();
    if attrs.needs_zig || windows_gnu_uses_zig {
        let zig_dir = paths
            .bin
            .join(format!("zig-{}", crate::fetch::MANAGED_ZIG_VERSION));
        let present = zig_dir.join(".complete").is_file() || zig_dir.is_dir();
        entries.push(RestoreEntry {
            label: format!("zig {}", crate::fetch::MANAGED_ZIG_VERSION),
            path: zig_dir,
            present,
        });
    }
    if attrs.needs_llvm_toolchain {
        let llvm_dir = paths
            .bin
            .join(format!("llvm-{}", crate::fetch::MANAGED_LLVM_VERSION));
        entries.push(RestoreEntry {
            label: format!("LLVM {}", crate::fetch::MANAGED_LLVM_VERSION),
            present: llvm_dir.is_dir(),
            path: llvm_dir,
        });
    }
    if attrs.needs_xwin_cache {
        let xwin = xwin_cache_root()?.join("xwin");
        entries.push(RestoreEntry {
            label: "xwin MSVC CRT + Windows SDK".to_string(),
            present: xwin.join("crt").join("include").is_dir()
                && xwin.join("sdk").join("include").is_dir(),
            path: xwin,
        });
    }
    if attrs.needs_mingw_w64_gcc
        && crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_gcc()
    {
        let mingw = paths
            .bin
            .join("syslib")
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TOOL)
            .join(crate::fetch::mingw_w64_gcc::MANAGED_MINGW_W64_GCC_VERSION)
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_SLUG);
        let package = mingw.join("package");
        entries.push(RestoreEntry {
            label: format!(
                "MinGW-w64 GCC {}",
                crate::fetch::mingw_w64_gcc::MANAGED_MINGW_W64_GCC_VERSION
            ),
            present: mingw.join(".complete").is_file()
                && package
                    .join("bin")
                    .join(crate::fetch::mingw_w64_gcc::exe_name("gcc"))
                    .is_file(),
            path: package,
        });
    }
    if attrs.needs_apple_sdk {
        let selection = crate::fetch::apple_sdk::resolve_apple_sdk_selection(Some(&attrs.triple));
        let sdk = crate::fetch::apple_sdk::install_dir_for_selection(paths, &selection);
        let sdk_dir = crate::fetch::apple_sdk::sdk_dir_for_selection(paths, &selection);
        entries.push(RestoreEntry {
            label: format!(
                "Apple SDK {}/{}",
                selection.version,
                selection.shape.catalogue_slug()
            ),
            present: sdk.join(".complete").is_file() && sdk_dir.is_dir(),
            path: sdk,
        });
    }
    Ok(entries)
}

fn emit_restore_report(entries: &[RestoreEntry]) {
    if entries.is_empty() {
        eprintln!("soldr prepare: restore audit: target has no expected paths to check");
        return;
    }
    let present = entries.iter().filter(|e| e.present).count();
    let total = entries.len();
    eprintln!("soldr prepare: restore audit: {present}/{total} expected entries present");
    for entry in entries {
        let mark = if entry.present { "✓" } else { "✗" };
        eprintln!(
            "  {mark} {label}  ({path})",
            mark = mark,
            label = entry.label,
            path = entry.path.display()
        );
    }
    if present < total {
        eprintln!(
            "soldr prepare: restore audit: {} missing entr{} will be downloaded by dispatch",
            total - present,
            if total - present == 1 { "y" } else { "ies" }
        );
    }
}

/// Worker count for the zstd encoder. `std::thread::available_parallelism`
/// is the most portable read of host parallelism; saturate to a sane
/// upper bound so we don't spawn a hundred zstd workers on big runners.
fn num_cpus_for_zstd() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8) as u32)
        .unwrap_or(4)
}

/// Glob-style listing of soldr-managed dirs that `prepare` populates.
/// Captured by `--save`, restored by `--restore`. Paths are relative
/// to the user's HOME dir so the archive is portable across runners
/// that share the same host triple.
///
/// We pack ENTIRE versioned subdirs (e.g. `~/.soldr/bin/zig-0.13.0/`)
/// rather than the parent `~/.soldr/bin/` so the archive doesn't
/// accidentally pull in zccache binaries or anything unrelated.
fn prepare_state_roots(paths: &SoldrPaths) -> Result<Vec<PathBuf>, SoldrError> {
    let mut roots = Vec::new();
    // ~/.soldr/bin/{zig-<ver>,llvm-<ver>,apple-sdk/<ver>,syslib/mingw-w64-gcc}
    if let Ok(entries) = std::fs::read_dir(&paths.bin) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let n = name.to_string_lossy();
            if n.starts_with("zig-") || n.starts_with("llvm-") || n == "apple-sdk" {
                roots.push(entry.path());
            }
        }
    }
    let mingw_root = paths
        .bin
        .join("syslib")
        .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TOOL);
    if mingw_root.is_dir() {
        roots.push(mingw_root);
    }
    // xwin cache (cargo-xwin's default location, mirrors `xwin_cache_root`).
    let xwin_root = xwin_cache_root()?;
    if xwin_root.is_dir() {
        roots.push(xwin_root);
    }
    Ok(roots)
}

/// Pack the prepare-managed dirs into a tar.zst at `archive`. Paths
/// inside the tar are RELATIVE to HOME so restore can extract them
/// onto any runner that uses the same home layout.
fn save_prepare_state(archive: &Path, paths: &SoldrPaths) -> Result<(), SoldrError> {
    let home = crate::core::home_dir()?;
    let roots = prepare_state_roots(paths)?;
    if roots.is_empty() {
        eprintln!("soldr prepare: nothing to save (no zig/llvm/apple-sdk/mingw/xwin dirs found)");
        return Ok(());
    }

    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(archive)?;
    // Level 12 + multi-thread: balances compression ratio and wall-clock.
    // -19 (the manifest-branch durable archive setting) is ~10x slower
    // single-threaded; -3 is fastest but bloats the archive. Multi-thread
    // (`-T0` equivalent) lets level 12 finish in a fraction of -19's
    // wall time while keeping the archive small enough to fit GHA's
    // 10 GiB per-repo cache budget comfortably.
    let mut encoder = zstd::stream::write::Encoder::new(file, 12)
        .map_err(|e| SoldrError::Archive(format!("zstd encoder init: {e}")))?;
    let _ = encoder.multithread(num_cpus_for_zstd());
    let encoder = encoder.auto_finish();
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for root in &roots {
        let rel = match root.strip_prefix(&home) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "soldr prepare: warning: {} is outside HOME ({}); skipping",
                    root.display(),
                    home.display()
                );
                continue;
            }
        };
        eprintln!("soldr prepare: saving {}", rel.display());
        // tar append can fail on Windows NTFS reparse points (junctions)
        // that cargo-xwin creates locally; on Linux CI runners these
        // are POSIX symlinks and tar handles them natively. Log + skip
        // failures rather than aborting the whole save — partial
        // archives still help the next restore.
        if let Err(e) = builder.append_dir_all(rel, root) {
            eprintln!(
                "soldr prepare: warning: tar append {} failed: {e}; skipping",
                rel.display()
            );
        }
    }
    builder
        .finish()
        .map_err(|e| SoldrError::Archive(format!("tar finish: {e}")))?;
    Ok(())
}

/// Extract a previously-saved tar.zst back onto disk. Entries are
/// resolved relative to HOME so the same archive replays across any
/// runner that shares the home layout. Existing files are overwritten
/// — the caller (`--restore`) treats partial / outdated archives as
/// best-effort: anything still missing after restore is re-downloaded
/// by the normal dispatch.
fn restore_prepare_state(archive: &Path, _paths: &SoldrPaths) -> Result<(), SoldrError> {
    let home = crate::core::home_dir()?;
    let file = std::fs::File::open(archive)
        .map_err(|e| SoldrError::Other(format!("open {}: {e}", archive.display())))?;
    let zst = zstd::stream::read::Decoder::new(file)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder: {e}")))?;
    let mut tarball = tar::Archive::new(zst);
    std::fs::create_dir_all(&home)?;
    tarball
        .unpack(&home)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    Ok(())
}

/// Run `rustup target add <triple>` for the active toolchain.
/// Idempotent — already-installed targets are a no-op.
fn rustup_add_target(triple: &str) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let rustup = crate::binaries::rustup_binary();
    let mut command = std::process::Command::new(rustup);
    command.args(["target", "add", triple]);
    if let Some(channel) = pinned_toolchain_channel()? {
        command.args(["--toolchain", &channel]);
    }
    command.env(
        crate::core::CARGO_HOME_ENV_VAR,
        crate::fetch::managed_cargo_home(&paths),
    );
    command.env(
        crate::core::RUSTUP_HOME_ENV_VAR,
        crate::fetch::managed_rustup_home(&paths),
    );
    crate::core::suppress_windows_console_window(&mut command);
    let status = run_rustup_target_add(&mut command, triple)?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "rustup target add {triple} exited with {}",
            status
        )));
    }
    Ok(())
}

fn rustup_target_add_timeout() -> Duration {
    std::env::var(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_RUSTUP_TARGET_ADD_TIMEOUT_SECS))
}

fn run_rustup_target_add(
    command: &mut std::process::Command,
    triple: &str,
) -> Result<std::process::ExitStatus, SoldrError> {
    let mut child = command
        .spawn()
        .map_err(|e| SoldrError::Other(format!("rustup target add {triple}: {e}")))?;
    let timeout = rustup_target_add_timeout();
    match child.wait_timeout(timeout).map_err(|e| {
        SoldrError::Other(format!(
            "failed to wait for rustup target add {triple}: {e}"
        ))
    })? {
        Some(status) => Ok(status),
        None => {
            let kill_result = child.kill();
            let reap_result = child.wait_timeout(Duration::from_secs(
                KILLED_RUSTUP_TARGET_ADD_REAP_TIMEOUT_SECS,
            ));
            let timeout_secs = timeout.as_secs();
            let mut message = format!(
                "rustup target add {triple} timed out after {timeout_secs} seconds \
                 (set {RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR} to override)"
            );
            match kill_result {
                Ok(()) => message.push_str("; killed child process"),
                Err(err) => message.push_str(&format!("; kill failed: {err}")),
            }
            match reap_result {
                Ok(Some(_)) => {}
                Ok(None) => message.push_str(&format!(
                    "; process did not exit within {KILLED_RUSTUP_TARGET_ADD_REAP_TIMEOUT_SECS} seconds after kill"
                )),
                Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
            }
            Err(SoldrError::Other(message))
        }
    }
}

fn pinned_toolchain_channel() -> Result<Option<String>, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    Ok(crate::core::read_rust_toolchain_manifest(&workspace_root)?.channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    struct CwdGuard {
        previous: PathBuf,
    }

    impl CwdGuard {
        fn enter(path: &Path) -> Self {
            let previous = std::env::current_dir().expect("cwd");
            std::env::set_current_dir(path).expect("chdir");
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    crate::timed_test!(constants_are_well_formed, {
        assert!(XWIN_CACHE_URL.starts_with("https://"));
        assert!(XWIN_CACHE_URL.ends_with(".tar.zst"));
        assert_eq!(XWIN_CACHE_SHA256.len(), 64);
        assert!(XWIN_CACHE_SHA256.chars().all(|c| c.is_ascii_hexdigit()));
    });

    crate::timed_test!(append_env_creates_file_and_appends, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = tmp.path().join("env");
        append_env(Some(&p), "FOO", "bar").expect("append");
        append_env(Some(&p), "BAZ", "/some/path").expect("append");
        let body = std::fs::read_to_string(&p).expect("read");
        assert!(body.contains("FOO=bar"));
        assert!(body.contains("BAZ=/some/path"));
    });

    crate::timed_test!(append_env_no_op_when_none, {
        append_env(None, "FOO", "bar").expect("no-op");
    });

    crate::timed_test!(xwin_cache_case_aliases_mixed_case_sdk_files, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let xwin = tmp.path().join("xwin");
        let lib = xwin.join("sdk").join("lib").join("um").join("x86_64");
        let include = xwin.join("sdk").join("include").join("um");
        std::fs::create_dir_all(&lib).expect("mkdir lib");
        std::fs::create_dir_all(&include).expect("mkdir include");
        std::fs::write(lib.join("Kernel32.Lib"), b"kernel32").expect("write kernel32");
        std::fs::write(lib.join("UserEnv.Lib"), b"userenv").expect("write userenv");
        std::fs::write(include.join("Windows.h"), b"windows").expect("write windows.h");

        // soldr#1229 — probe filesystem case-sensitivity. macOS APFS
        // is case-insensitive by default: `Kernel32.Lib` and
        // `kernel32.lib` resolve to the same inode, so
        // `ensure_lowercase_file_aliases`'s `alias.exists()` guard
        // trips and no aliases are created. That's CORRECT behavior
        // (aliases aren't needed on case-insensitive FS) — the test
        // just needs to adjust its expectation.
        let probe = tmp.path().join("CaseProbe");
        std::fs::write(&probe, b"").expect("write case probe");
        let case_insensitive = tmp.path().join("caseprobe").exists();
        std::fs::remove_file(&probe).ok();

        let created = ensure_xwin_case_aliases(&xwin).expect("aliases");
        let expected_created = if cfg!(windows) || case_insensitive {
            0
        } else {
            3
        };
        assert_eq!(created, expected_created);
        // These assertions pass on both case-sensitive (real aliases
        // created) and case-insensitive (same file resolvable under any
        // case) filesystems.
        assert!(lib.join("kernel32.lib").is_file());
        assert!(lib.join("userenv.lib").is_file());
        assert!(include.join("windows.h").is_file());

        let created_again = ensure_xwin_case_aliases(&xwin).expect("aliases are idempotent");
        assert_eq!(created_again, 0);
    });

    crate::timed_test!(rustup_add_target_uses_soldr_managed_rustup_state, {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tmpdir");
        let soldr_root = tmp.path().join("soldr-root");
        let log = tmp.path().join("rustup.log");
        let fake_rustup = tmp.path().join(if cfg!(windows) {
            "rustup.cmd"
        } else {
            "rustup"
        });

        #[cfg(windows)]
        std::fs::write(
            &fake_rustup,
            "@echo off\r\n\
             (\r\n\
             echo args=%*\r\n\
             echo CARGO_HOME=%CARGO_HOME%\r\n\
             echo RUSTUP_HOME=%RUSTUP_HOME%\r\n\
             ) > \"%SOLDR_RUSTUP_LOG%\"\r\n",
        )
        .expect("write fake rustup");

        #[cfg(not(windows))]
        {
            std::fs::write(
                &fake_rustup,
                "#!/bin/sh\n\
                 {\n\
                   printf 'args=%s\\n' \"$*\"\n\
                   printf 'CARGO_HOME=%s\\n' \"$CARGO_HOME\"\n\
                   printf 'RUSTUP_HOME=%s\\n' \"$RUSTUP_HOME\"\n\
                 } > \"$SOLDR_RUSTUP_LOG\"\n",
            )
            .expect("write fake rustup");
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_rustup)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_rustup, perms).expect("chmod");
        }

        let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
        let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &soldr_root);
        let _log = EnvVarGuard::set("SOLDR_RUSTUP_LOG", &log);
        let _cargo_home = EnvVarGuard::remove(crate::core::CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::remove(crate::core::RUSTUP_HOME_ENV_VAR);

        rustup_add_target("aarch64-apple-darwin").expect("rustup target add");

        let body = std::fs::read_to_string(&log).expect("read fake rustup log");
        assert!(
            body.contains("args=target add aarch64-apple-darwin"),
            "fake rustup should receive target add args, got: {body}"
        );
        assert!(
            body.contains(&format!(
                "CARGO_HOME={}",
                crate::fetch::managed_cargo_home(&SoldrPaths::with_root(soldr_root.clone()))
                    .display()
            )),
            "fake rustup should receive managed CARGO_HOME, got: {body}"
        );
        assert!(
            body.contains(&format!(
                "RUSTUP_HOME={}",
                crate::fetch::managed_rustup_home(&SoldrPaths::with_root(soldr_root)).display()
            )),
            "fake rustup should receive managed RUSTUP_HOME, got: {body}"
        );
    });

    crate::timed_test!(rustup_add_target_scopes_to_pinned_toolchain_channel, {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let tmp = tempfile::tempdir().expect("tmpdir");
        let soldr_root = tmp.path().join("soldr-root");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.94.1\"\n",
        )
        .expect("write toolchain");
        let log = tmp.path().join("rustup.log");
        let fake_rustup = tmp.path().join(if cfg!(windows) {
            "rustup.cmd"
        } else {
            "rustup"
        });

        #[cfg(windows)]
        std::fs::write(
            &fake_rustup,
            "@echo off\r\n\
             echo args=%* > \"%SOLDR_RUSTUP_LOG%\"\r\n",
        )
        .expect("write fake rustup");

        #[cfg(not(windows))]
        {
            std::fs::write(
                &fake_rustup,
                "#!/bin/sh\n\
                 printf 'args=%s\\n' \"$*\" > \"$SOLDR_RUSTUP_LOG\"\n",
            )
            .expect("write fake rustup");
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_rustup)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_rustup, perms).expect("chmod");
        }

        let _cwd_guard = CwdGuard::enter(&project);
        let _rustup = EnvVarGuard::set(crate::TEST_RUSTUP_BIN_ENV_VAR, &fake_rustup);
        let _root = EnvVarGuard::set(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &soldr_root);
        let _log = EnvVarGuard::set("SOLDR_RUSTUP_LOG", &log);

        rustup_add_target("aarch64-apple-darwin").expect("rustup target add");

        let body = std::fs::read_to_string(&log).expect("read fake rustup log");
        assert!(
            body.contains("args=target add aarch64-apple-darwin --toolchain 1.94.1"),
            "fake rustup should receive pinned toolchain args, got: {body}"
        );
    });

    crate::timed_test!(rustup_target_add_timeout_uses_positive_env_override_only, {
        let _lock = ENV_LOCK.lock().expect("env lock");
        {
            let _guard = EnvVarGuard::set(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR, "19");
            assert_eq!(rustup_target_add_timeout(), Duration::from_secs(19));
        }
        for value in ["", "0", "-1", "abc"] {
            let _guard = EnvVarGuard::set(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR, value);
            assert_eq!(
                rustup_target_add_timeout(),
                Duration::from_secs(DEFAULT_RUSTUP_TARGET_ADD_TIMEOUT_SECS),
                "invalid override {value:?} should use default"
            );
        }
        let _guard = EnvVarGuard::remove(RUSTUP_TARGET_ADD_TIMEOUT_ENV_VAR);
        assert_eq!(
            rustup_target_add_timeout(),
            Duration::from_secs(DEFAULT_RUSTUP_TARGET_ADD_TIMEOUT_SECS)
        );
    });

    crate::timed_test!(parse_target_arg_all_is_sentinel, {
        assert_eq!(parse_target_arg("all").unwrap(), ParsedTargetArg::All);
    });

    crate::timed_test!(parse_target_arg_single_triple, {
        let got = parse_target_arg("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(
            got,
            ParsedTargetArg::Explicit(vec!["x86_64-unknown-linux-gnu".into()])
        );
    });

    crate::timed_test!(parse_target_arg_comma_separated, {
        let got = parse_target_arg(
            "x86_64-pc-windows-msvc,aarch64-apple-darwin,x86_64-unknown-linux-musl",
        )
        .unwrap();
        assert_eq!(
            got,
            ParsedTargetArg::Explicit(vec![
                "x86_64-pc-windows-msvc".into(),
                "aarch64-apple-darwin".into(),
                "x86_64-unknown-linux-musl".into(),
            ])
        );
    });

    crate::timed_test!(parse_target_arg_trims_whitespace, {
        let got = parse_target_arg(" x86_64-pc-windows-msvc , aarch64-apple-darwin ").unwrap();
        assert_eq!(
            got,
            ParsedTargetArg::Explicit(vec![
                "x86_64-pc-windows-msvc".into(),
                "aarch64-apple-darwin".into(),
            ])
        );
    });

    crate::timed_test!(parse_target_arg_drops_empty_entries, {
        // Leading / trailing / consecutive commas are silently dropped
        // because they're a common copy-paste mistake. The error path
        // covers the "every entry was empty" case below.
        let got = parse_target_arg(",x86_64-pc-windows-msvc,,aarch64-apple-darwin,").unwrap();
        assert_eq!(
            got,
            ParsedTargetArg::Explicit(vec![
                "x86_64-pc-windows-msvc".into(),
                "aarch64-apple-darwin".into(),
            ])
        );
    });

    crate::timed_test!(parse_target_arg_all_empty_errors, {
        let err = parse_target_arg(", , ,").unwrap_err();
        assert!(
            err.to_string().contains("comma-separated list was empty"),
            "unexpected error: {err}"
        );
    });

    crate::timed_test!(classify_target_windows_msvc, {
        let attrs = classify_target("x86_64-pc-windows-msvc").expect("classify");
        assert_eq!(attrs.arch, TargetArch::X86_64);
        assert_eq!(attrs.os, TargetOs::Windows);
        assert_eq!(attrs.abi, Some(TargetAbi::Msvc));
        assert!(attrs.needs_xwin_cache);
        assert!(attrs.needs_llvm_toolchain);
        assert!(!attrs.needs_mingw_w64_gcc);
        assert!(!attrs.needs_zig);
        assert!(!attrs.needs_apple_sdk);

        let arm = classify_target("aarch64-pc-windows-msvc").expect("classify arm");
        assert_eq!(arm.arch, TargetArch::Aarch64);
        assert_eq!(arm.os, TargetOs::Windows);
    });

    crate::timed_test!(classify_target_apple_darwin, {
        let attrs = classify_target("aarch64-apple-darwin").expect("classify");
        assert_eq!(attrs.arch, TargetArch::Aarch64);
        assert_eq!(attrs.os, TargetOs::Darwin);
        assert_eq!(attrs.abi, None);
        assert!(attrs.needs_zig);
        assert!(attrs.needs_apple_sdk);
        assert!(!attrs.needs_xwin_cache);
        assert!(!attrs.needs_llvm_toolchain);

        let intel = classify_target("x86_64-apple-darwin").expect("classify intel");
        assert_eq!(intel.arch, TargetArch::X86_64);
    });

    crate::timed_test!(classify_target_linux_gnu_and_musl, {
        let gnu = classify_target("x86_64-unknown-linux-gnu").expect("classify gnu");
        assert_eq!(gnu.os, TargetOs::Linux);
        assert_eq!(gnu.abi, Some(TargetAbi::Gnu));
        assert!(gnu.needs_zig);
        assert!(!gnu.needs_xwin_cache);
        assert!(!gnu.needs_apple_sdk);

        let musl = classify_target("aarch64-unknown-linux-musl").expect("classify musl");
        assert_eq!(musl.os, TargetOs::Linux);
        assert_eq!(musl.abi, Some(TargetAbi::Musl));
    });

    crate::timed_test!(classify_target_rejects_unknown_arch, {
        let err = classify_target("riscv64-unknown-linux-gnu").expect_err("riscv unsupported");
        assert!(
            err.to_string().contains("did not match any known arch"),
            "msg: {err}"
        );
    });

    crate::timed_test!(classify_target_rejects_unknown_os, {
        // freebsd has no abi suffix so the triple is 3 parts; the os
        // slot ("freebsd") doesn't score above threshold against any
        // KNOWN_OSES entry.
        let err = classify_target("x86_64-unknown-freebsd").expect_err("freebsd unsupported");
        assert!(
            err.to_string().contains("did not match any known os"),
            "msg: {err}"
        );
    });

    crate::timed_test!(classify_target_rejects_malformed_triple, {
        let err = classify_target("x86_64").expect_err("too few parts");
        assert!(err.to_string().contains("unrecognized target triple shape"));
        let err = classify_target("a-b-c-d-e").expect_err("too many parts");
        assert!(err.to_string().contains("unrecognized target triple shape"));
    });

    crate::timed_test!(classify_target_windows_gnu_x64, {
        let attrs = classify_target("x86_64-pc-windows-gnu").expect("classify mingw");
        assert_eq!(attrs.arch, TargetArch::X86_64);
        assert_eq!(attrs.os, TargetOs::Windows);
        assert_eq!(attrs.abi, Some(TargetAbi::Gnu));
        assert!(attrs.needs_mingw_w64_gcc);
        assert!(!attrs.needs_xwin_cache);
        assert!(!attrs.needs_llvm_toolchain);
        assert!(!attrs.needs_zig);
        assert!(!attrs.needs_apple_sdk);
    });

    crate::timed_test!(classify_target_rejects_non_x64_windows_gnu_scope, {
        let err = classify_target("aarch64-pc-windows-gnu").expect_err("non-x64 gnu out of scope");
        assert!(
            err.to_string().contains("only x86_64-pc-windows-gnu"),
            "msg: {err}"
        );

        let err = classify_target("x86_64-pc-windows-gnullvm").expect_err("gnullvm out of scope");
        assert!(
            err.to_string().contains("did not match any known abi"),
            "msg: {err}"
        );
    });

    // ---- Fuzzy-matching behavior ----

    crate::timed_test!(fuzzy_exact_match_scores_one, {
        assert_eq!(fuzzy_score("x86_64", "x86_64"), 1.0);
        assert_eq!(fuzzy_score("linux", "linux"), 1.0);
        // Case-insensitive exact = 0.99 — still cleanly above threshold.
        let case = fuzzy_score("LINUX", "linux");
        assert!(
            case > FUZZY_MATCH_THRESHOLD,
            "case-insensitive score={case}"
        );
    });

    crate::timed_test!(fuzzy_best_match_prefers_exact_over_prefix, {
        // The user's example: input "x86_AMD"; registry has both "x86"
        // and "x86_AMD". Exact must beat prefix.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Tag {
            Short,
            AmdLong,
        }
        let registry: &[(&str, Tag)] = &[("x86", Tag::Short), ("x86_AMD", Tag::AmdLong)];
        let picked = best_match("x86_AMD", registry, "test").expect("matches");
        assert_eq!(picked, Tag::AmdLong);

        // And the inverse: input "x86" picks the short entry.
        let picked = best_match("x86", registry, "test").expect("matches");
        assert_eq!(picked, Tag::Short);
    });

    crate::timed_test!(fuzzy_rejects_below_threshold, {
        // "x86" against the real registry (only "x86_64", "aarch64")
        // scores ~0.65 against x86_64 — below 0.85, so rejected. This
        // is the safety property: typos and abbreviations don't
        // silently route to the wrong target.
        let err = best_match("x86", KNOWN_ARCHES, "arch").expect_err("rejected");
        let msg = err.to_string();
        assert!(msg.contains("did not match"), "msg: {msg}");
        assert!(
            msg.contains("x86_64"),
            "must name closest candidate; got: {msg}"
        );
    });

    crate::timed_test!(fuzzy_case_insensitive_classify, {
        // Uppercase triple components classify the same as lowercase.
        let attrs = classify_target("X86_64-PC-Windows-MSVC").expect("case-insensitive");
        assert_eq!(attrs.arch, TargetArch::X86_64);
        assert_eq!(attrs.os, TargetOs::Windows);
        assert_eq!(attrs.abi, Some(TargetAbi::Msvc));
    });

    crate::timed_test!(soldr_workspace_metadata_dogfood, {
        // Regression guard: soldr's own workspace `Cargo.toml`
        // declares `[workspace.metadata.soldr].targets` (RFC #914).
        // Every entry must classify cleanly via the fuzzy classifier
        // — typos in soldr's own manifest fail at test time, not
        // mid-CI when `soldr prepare --target all` blows up.
        //
        // soldr#1040 phase 2: source-tree-coupled — skip on cross-build
        // target runners that don't have the workspace checked out.
        if std::env::var_os("SOLDR_TEST_SKIP_SOURCE_TREE").is_some() {
            eprintln!(
                "skipping soldr_workspace_metadata_dogfood: \
                 SOLDR_TEST_SKIP_SOURCE_TREE is set (soldr#1040)"
            );
            return;
        }
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace parent of crate dir")
            .parent()
            .expect("workspace root")
            .join("Cargo.toml");
        assert!(manifest.is_file(), "workspace manifest at {manifest:?}");
        let meta = crate::cargo_metadata_soldr::read_soldr_metadata(&manifest)
            .expect("parse soldr Cargo.toml");
        assert!(
            !meta.targets.is_empty(),
            "soldr's own [workspace.metadata.soldr].targets is empty — regression"
        );
        for triple in &meta.targets {
            classify_target(triple)
                .unwrap_or_else(|e| panic!("triple `{triple}` in soldr Cargo.toml: {e}"));
        }
    });

    // ---- Corpus test ----
    //
    // `triple_corpus.txt` is the canonical `rustc --print target-list`
    // (snapshot taken on 2026-06-22, 308 entries) augmented with
    // extra real-world triples scraped from FastLED + zackees repos.
    // The rustc list is *the* answer to "what are the common target
    // triples across the Rust ecosystem" — every Rust toolchain
    // recognizes exactly this set.
    //
    // For each triple we assert:
    //   - If it's in soldr's supported subset ({x86_64, aarch64} ×
    //     {pc-windows-msvc, apple-darwin, unknown-linux-gnu,
    //     unknown-linux-musl}) → classify_target returns Ok with the
    //     expected attrs.
    //   - Otherwise → returns Err. This protects against the fuzzy
    //     matcher silently routing some `wasm32-...` or
    //     `riscv64gc-...` to one of the supported arms.

    fn is_soldr_supported(triple: &str) -> bool {
        matches!(
            triple,
            "x86_64-pc-windows-msvc"
                | "aarch64-pc-windows-msvc"
                | "x86_64-pc-windows-gnu"
                | "x86_64-apple-darwin"
                | "aarch64-apple-darwin"
                | "x86_64-unknown-linux-gnu"
                | "aarch64-unknown-linux-gnu"
                | "x86_64-unknown-linux-musl"
                | "aarch64-unknown-linux-musl"
        )
    }

    crate::timed_test!(classifier_against_rustc_target_list, {
        let corpus = include_str!("triple_corpus.txt");
        let triples: Vec<&str> = corpus
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(
            triples.len() >= 300,
            "corpus shrunk unexpectedly: {} entries",
            triples.len()
        );

        let mut supported_ok = 0;
        let mut unsupported_rejected = 0;
        let mut surprises: Vec<String> = Vec::new();

        for triple in &triples {
            let result = classify_target(triple);
            let expected_ok = is_soldr_supported(triple);
            match (expected_ok, &result) {
                (true, Ok(attrs)) => {
                    // Spot-check that the fuzzy matcher picked the
                    // right enum variants, not just *some* variant.
                    if triple.starts_with("x86_64-") {
                        assert_eq!(attrs.arch, TargetArch::X86_64, "{triple}");
                    } else {
                        assert_eq!(attrs.arch, TargetArch::Aarch64, "{triple}");
                    }
                    if triple.contains("-windows-") {
                        assert_eq!(attrs.os, TargetOs::Windows, "{triple}");
                        let expected_abi = if triple.ends_with("-gnu") {
                            TargetAbi::Gnu
                        } else {
                            TargetAbi::Msvc
                        };
                        assert_eq!(attrs.abi, Some(expected_abi), "{triple}");
                    } else if triple.contains("-darwin") {
                        assert_eq!(attrs.os, TargetOs::Darwin, "{triple}");
                        assert_eq!(attrs.abi, None, "{triple}");
                    } else if triple.ends_with("-gnu") {
                        assert_eq!(attrs.os, TargetOs::Linux, "{triple}");
                        assert_eq!(attrs.abi, Some(TargetAbi::Gnu), "{triple}");
                    } else if triple.ends_with("-musl") {
                        assert_eq!(attrs.os, TargetOs::Linux, "{triple}");
                        assert_eq!(attrs.abi, Some(TargetAbi::Musl), "{triple}");
                    }
                    supported_ok += 1;
                }
                (false, Err(_)) => {
                    unsupported_rejected += 1;
                }
                (true, Err(e)) => {
                    surprises.push(format!("FALSE NEGATIVE `{triple}` → Err: {e}"));
                }
                (false, Ok(attrs)) => {
                    surprises.push(format!(
                        "FALSE POSITIVE `{triple}` → Ok({:?}/{:?}/{:?})",
                        attrs.arch, attrs.os, attrs.abi
                    ));
                }
            }
        }

        eprintln!(
            "corpus: {} triples; {} soldr-supported classify Ok; {} unsupported correctly rejected",
            triples.len(),
            supported_ok,
            unsupported_rejected
        );
        if !surprises.is_empty() {
            for s in &surprises {
                eprintln!("  {s}");
            }
            panic!("{} corpus surprise(s) — see stderr above", surprises.len());
        }
        // Sanity: confirm we actually exercised the supported set.
        assert_eq!(
            supported_ok, 9,
            "expected all 9 soldr-supported triples to classify Ok"
        );
    });
}
