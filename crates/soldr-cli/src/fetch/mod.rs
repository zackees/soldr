//! Binary resolution and download for soldr.
//!
//! Resolution chain (Phase 1 MVP):
//! 1. Local cache (`~/.soldr/bin/<tool>-<version>/`)
//! 2. GitHub Releases (repository URL from crates.io, or override from `known_tools`)

pub mod known_tools;

pub use known_tools::{
    known_cargo_subcommands, lookup_by_cargo_subcommand, lookup_by_crate, wraps_inner_cargo_build,
    ToolSpec, CARGO_CHEF_PINNED_VERSION, KNOWN_TOOLS,
};

pub mod trust;

pub use trust::{
    sha256_of, verify_download, PinnedChecksumStore, TrustMode, VerifyOutcome,
    CHECKSUMS_FILE_ENV_VAR, TRUST_MODE_ENV_VAR,
};

pub mod install_zccache;

pub use install_zccache::{
    install_zccache_from_source, pinned_version_drift_from_managed, pinned_zccache_dir,
    read_pinned_sidecar, remove_pinned_zccache, resolve_pinned_zccache, InstallReport,
    InstallSource, PinnedBinaryRecord, PinnedResolution, PinnedSidecar, PINNED_ZCCACHE_DIRNAME,
    PINNED_ZCCACHE_SIDECAR_FILENAME, PINNED_ZCCACHE_SIDECAR_SCHEMA_VERSION,
    ZCCACHE_PINNED_BINARY_NAMES,
};

pub mod rustup_init;

pub use rustup_init::{
    auto_bootstrap_if_missing, auto_bootstrap_if_missing_blocking, bootstrap_rustup,
    bootstrap_rustup_blocking, discover_rustup, managed_cargo_home, managed_rustup_home,
    managed_rustup_path, rustup_init_download_url, rustup_init_host_triple, AutoBootstrapOutcome,
    BootstrapReport, NO_BOOTSTRAP_ENV_VAR, RUSTUP_INIT_TRIPLE_ENV_VAR, RUSTUP_INIT_URL_ENV_VAR,
};

pub mod apple_sdk;
pub mod archive;
/// soldr#1012 PR 5 — xwin-cache catalogue materialization for the
/// blessed `*-pc-windows-msvc` cross-compile path.
pub mod xwin_cache;
/// soldr#988 Phase 4 — on-demand builds via `zackees/forge` when the
/// toolchain catalogue lookup misses. See module doc for the env-var
/// contract and failure surface.
pub mod forge_dispatch;
pub mod github;
pub mod llvm;
/// soldr#997 Phase A — LLVM-tools bundle from soldr-toolchain
/// `recipes/llvm-tools-linux-x64/`. See module doc for why this is
/// distinct from [`llvm`].
pub mod llvm_tools_bundle;
pub mod manifest_lookup;
pub mod manifest_v6;
/// soldr#997 Phase A — Node.js node.lib bundle for Windows MSVC
/// cross-compile (closes #944).
pub mod nodelib;
/// soldr#997 Phase A — OpenSSL libs/headers bundle for Windows MSVC
/// cross-compile (closes #943).
pub mod openssl_sysroot;
/// soldr#997 Phase A — Python sysroot bundle for PyO3 cross-compile
/// (closes parts of #931, #932, #933).
pub mod python_sysroot;
// soldr#1064 Phase B — *-sys C library catalogue distribution.
// Each module is a stub-until-ingested consumer modeled on
// openssl_sysroot.rs. Once the soldr-toolchain forge dispatches
// land the assets, blessed_build::prepare wires the corresponding
// _SYSTEM / _OVERRIDE env vars onto the child cargo invocation so
// the *-sys crate's build.rs skips its vendored compile.
pub mod bzip2_sysroot;
pub mod jemalloc_sysroot;
pub mod lzma_sysroot;
pub mod mimalloc_sysroot;
pub mod sqlite_sysroot;
pub mod zlib_ng_sysroot;
pub mod zstd_sysroot;
pub mod zccache;
pub mod zccache_install;
pub mod zccache_runtime;
pub mod zig;

pub use apple_sdk::{ensure_apple_sdk, MANAGED_APPLE_SDK_VERSION};
pub use llvm::{ensure_llvm_toolchain, MANAGED_LLVM_VERSION};
pub use manifest_lookup::{
    ManifestEntry, ManifestIndex, CATALOGUE_DOC_NAME, DEFAULT_TOOLCHAIN_ORIGIN,
    MANIFEST_DISABLE_ENV_VAR, MANIFEST_FETCH_TIMEOUT, TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
    TOOLCHAIN_ORIGIN_ENV_VAR,
};
pub use manifest_v6::{
    embedded_manifest, embedded_manifest_bytes, is_stable_version_tag, ManifestV6, V6Asset, V6Hit,
    V6Leaf,
};
pub use zig::{ensure_zig, MANAGED_ZIG_VERSION};

#[cfg(test)]
mod zccache_contract_tests;

pub use zccache::{
    classify_zccache_source, managed_only_zccache_summary, pinned_zccache_summary,
    resolve_local_zccache, zccache_binary_summary, ZccacheBinarySummary, ZccacheSource,
};
pub use zccache_install::{
    cached_zccache_binary, fetch_zccache_with_paths, resolve_system_zccache,
};
pub use zccache_runtime::{ZccacheResolver, ZccacheRuntime, ZccacheRuntimeSource};

pub(crate) use github::http_client;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum VersionSpec {
    Latest,
    Exact(String),
}

impl VersionSpec {
    pub fn parse(s: &str) -> Self {
        if s.is_empty() || s == "latest" {
            Self::Latest
        } else {
            Self::Exact(s.to_string())
        }
    }
}

#[derive(Debug)]
pub struct FetchResult {
    pub binary_path: PathBuf,
    pub version: String,
    pub cached: bool,
}

pub const MANAGED_ZCCACHE_VERSION: &str = "1.12.11";

/// The soldr version segment used by per-version `~/.soldr/v<X.Y.Z>/**`
/// state. Source of truth for `SoldrPaths::versioned_root` and
/// `SoldrPaths::versioned_shims_dir`. See zackees/soldr#743 for the
/// layout RFC and zackees/soldr#742 for the first consumer.
pub const MANAGED_SHIM_VERSION: &str = env!("CARGO_PKG_VERSION");
// After the Wave 7 monocrate rename in zccache (`zccache-monocrate` -> `zccache`),
// all three native binaries (`zccache`, `zccache-daemon`, `zccache-fp`) are
// `[[bin]]` targets inside the umbrella `zccache` crate on crates.io. The
// sibling crates `zccache-cli`, `zccache-watcher`, and `zccache-fingerprint`
// are pyo3 cdylibs only; `cargo install` rejects them with
// "no bin target named X in <pkg>" when soldr asks for an executable.
// Each tuple is `(crates.io package, --bin name)`.
pub(crate) const MANAGED_ZCCACHE_PACKAGES: [(&str, &str); 3] = [
    ("zccache", "zccache"),
    ("zccache", "zccache-daemon"),
    ("zccache", "zccache-fp"),
];

/// Override the managed-zccache resolution entirely: instead of
/// fetching from GitHub Releases (or installing from crates.io), use
/// the locally-built binaries in this directory. See
/// `resolve_local_zccache` for the resolution contract.
pub const ZCCACHE_LOCAL_DIR_ENV_VAR: &str = "SOLDR_ZCCACHE_LOCAL_DIR";

/// Pinned crgx version that soldr's release pipeline source-builds and
/// bundles into the combined `.tar.zst` archive (see PR follow-up to
/// #434 — combined archive now ships zccache + crgx). Also surfaced
/// via `lookup_by_crate("crgx").pinned_version` so the
/// `fetch_repo_binary` fallback resolves the same version when the
/// bundled binary is not available.
pub const MANAGED_CRGX_VERSION: &str = "0.1.0";

/// Pinned `maturin` release used by both the `soldr maturin <args>`
/// CLI dispatch and the PEP 517 build backend in `src/soldr/__init__.py`.
/// Surfaced via `lookup_by_crate("maturin").pinned_version` so every
/// resolution path lands on the same upstream version — keeping the
/// Python+Rust packaging behavior reproducible across machines.
///
/// Lockstep bookkeeping (per CLAUDE.md "Bumping managed_zccache_version"):
/// when bumping, also update `contracts/zccache-runtime.v1.json`'s
/// `maturin.managed_version`; the
/// `zccache_runtime_contract_matches_rust_constants` test asserts they
/// match and panics with a directive message if they drift.
pub const MANAGED_MATURIN_VERSION: &str = "1.14.1";

/// Override the runtime crgx resolution: when set, soldr uses the
/// `crgx` (or `crgx.exe`) binary in this directory ahead of any
/// GitHub-Releases / crates.io fetch. The npm shim (`bin/soldr.js`)
/// and the setup-soldr action point this at the directory containing
/// the bundled binary so the first `soldr crgx ...` call needs no
/// network round trip. Mirrors `SOLDR_ZCCACHE_LOCAL_DIR`.
pub const CRGX_LOCAL_DIR_ENV_VAR: &str = "SOLDR_CRGX_LOCAL_DIR";

/// Override the runtime cargo-chef resolution: when set, soldr uses the
/// `cargo-chef` (or `cargo-chef.exe`) binary in this directory ahead of
/// any GitHub-Releases fetch. Soldr release archives and setup-soldr
/// point this at the bundled cargo-chef binary so `soldr cook` is
/// deterministic on targets upstream does not publish, notably
/// `aarch64-apple-darwin`.
pub const CARGO_CHEF_LOCAL_DIR_ENV_VAR: &str = "SOLDR_CARGO_CHEF_LOCAL_DIR";

/// Retry budget for the GitHub-Releases + crates.io fetch chain inside
/// `fetch_repo_binary_with_paths`. Transient errors
/// (`SoldrError::Network`, `SoldrError::ToolNotFound`) retry up to
/// `REPO_FETCH_ATTEMPTS` times with exponential backoff starting at
/// `REPO_FETCH_INITIAL_BACKOFF`. The same values previously lived as
/// `MANAGED_ZCCACHE_FETCH_*` constants used at a single call site; they
/// now apply to every fetch in soldr (zccache, crgx, cargo-chef, every
/// ecosystem tool) so users and CI don't have to babysit transient
/// GitHub API hiccups.
const REPO_FETCH_ATTEMPTS: u32 = 4;
const REPO_FETCH_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const MANAGED_ZCCACHE_INSTALL_ATTEMPTS: u32 = 3;
pub(crate) const MANAGED_ZCCACHE_INSTALL_INITIAL_BACKOFF: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Fetch a tool binary for the current platform.
pub async fn fetch_tool(
    crate_name: &str,
    version: &VersionSpec,
) -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    fetch_tool_with_paths(crate_name, version, &paths).await
}

/// Fetch with explicit paths (useful for testing).
pub async fn fetch_tool_with_paths(
    crate_name: &str,
    version: &VersionSpec,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;

    // Bundled single-binary escape hatches: when the npm shim or
    // setup-soldr action ships a bundled tool, they export a local-dir
    // env var pointing at the directory holding the binary. Honor it
    // ahead of every other resolution path so first-use carries no
    // network round trip. Mirrors the SOLDR_ZCCACHE_LOCAL_DIR contract
    // used by the bundled zccache trio (#434).
    if crate_name == "cargo-chef" {
        if let Some(local_dir) = non_empty_env_path(CARGO_CHEF_LOCAL_DIR_ENV_VAR) {
            return resolve_local_single_binary(
                &local_dir,
                CARGO_CHEF_LOCAL_DIR_ENV_VAR,
                "cargo-chef",
                CARGO_CHEF_PINNED_VERSION,
            );
        }
    }
    if crate_name == "crgx" {
        if let Some(local_dir) = non_empty_env_path(CRGX_LOCAL_DIR_ENV_VAR) {
            return resolve_local_single_binary(
                &local_dir,
                CRGX_LOCAL_DIR_ENV_VAR,
                "crgx",
                MANAGED_CRGX_VERSION,
            );
        }
    }

    if let Some(spec) = lookup_by_crate(crate_name) {
        let repo = match spec.repo {
            Some((owner, name)) => github::RepoInfo {
                owner: owner.to_string(),
                repo: name.to_string(),
            },
            None => github::resolve_repo(crate_name).await?,
        };
        // Honor `spec.pinned_version` when the caller didn't pass an
        // explicit `@version` — mirrors the cargo-front-door's
        // existing pin substitution so `soldr crgx` and
        // `soldr cargo chef` resolve the same way from both entry
        // points. Lets the `External` dispatch in main.rs go through
        // the registry pin without each call site re-implementing it.
        let effective_version = match (version, spec.pinned_version) {
            (VersionSpec::Latest, Some(pin)) => VersionSpec::Exact(pin.to_string()),
            _ => version.clone(),
        };
        return fetch_repo_binary_with_paths(
            spec.crate_name,
            &[spec.binary_name],
            &repo,
            &effective_version,
            spec.tag_prefix,
            paths,
        )
        .await;
    }

    let repo = github::resolve_repo(crate_name).await?;
    fetch_repo_binary_with_paths(crate_name, &[crate_name], &repo, version, None, paths).await
}

/// Resolve a bundled single-binary tool from a local directory env var.
/// Returns the path verbatim — no copy, no content hashing — because
/// tools such as crgx and cargo-chef have no daemon/sidecar siblings
/// (contrast with `resolve_local_zccache`, which normalizes a trio plus
/// debug-info sidecars).
fn resolve_local_single_binary(
    local_dir: &Path,
    env_var: &str,
    binary_stem: &str,
    version: &str,
) -> Result<FetchResult, SoldrError> {
    if !local_dir.is_dir() {
        return Err(SoldrError::Other(format!(
            "{env_var}={} is not a directory",
            local_dir.display(),
        )));
    }
    let binary_name = if cfg!(windows) {
        format!("{binary_stem}.exe")
    } else {
        binary_stem.to_string()
    };
    let binary = local_dir.join(binary_name);
    if !binary.is_file() {
        return Err(SoldrError::Other(format!(
            "{env_var}={} but {} is missing",
            local_dir.display(),
            binary.display(),
        )));
    }
    Ok(FetchResult {
        binary_path: binary,
        version: format!("local-{version}"),
        cached: true,
    })
}

pub async fn fetch_zccache() -> Result<FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    fetch_zccache_with_paths(&paths).await
}

pub(crate) fn non_empty_env_path(env_var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

pub(super) async fn fetch_repo_binary_with_paths(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    let mut backoff = REPO_FETCH_INITIAL_BACKOFF;
    let mut attempt: u32 = 1;
    loop {
        match fetch_repo_binary_once(cache_name, binary_names, repo, version, tag_prefix, paths)
            .await
        {
            Ok(result) => return Ok(result),
            Err(err) if attempt < REPO_FETCH_ATTEMPTS && is_transient_fetch_error(&err) => {
                eprintln!(
                    "soldr: transient error fetching {cache_name} from {}/{} (attempt {attempt}/{}): {err}; retrying in {:?}",
                    repo.owner, repo.repo, REPO_FETCH_ATTEMPTS, backoff,
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Classify whether an error from the repo-fetch chain is worth retrying.
/// `ToolNotFound` covers GitHub-Releases 404s during release propagation
/// windows + crates.io index-not-ready windows; `Network` covers DNS
/// blips, connection resets, and TLS handshake failures. Anything else
/// (malformed JSON, archive extraction errors, IO errors) is treated as
/// terminal — retrying wouldn't help.
fn is_transient_fetch_error(err: &SoldrError) -> bool {
    matches!(err, SoldrError::ToolNotFound(_) | SoldrError::Network(_))
}

/// One attempt at the full repo-binary fetch pipeline. Cache check →
/// release lookup → asset match → download + extract. Wrapped in
/// `fetch_repo_binary_with_paths`'s retry loop so transient failures
/// don't bubble up to the user.
async fn fetch_repo_binary_once(
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    version: &VersionSpec,
    tag_prefix: Option<&str>,
    paths: &SoldrPaths,
) -> Result<FetchResult, SoldrError> {
    paths.ensure_dirs()?;
    let target = TargetTriple::detect()?;
    if binary_names.is_empty() {
        return Err(SoldrError::Other(format!(
            "no binary names configured for {cache_name}"
        )));
    }

    if let VersionSpec::Exact(ref v) = version {
        if let Some(r) = check_cache(paths, cache_name, v, binary_names, &target)? {
            return Ok(r);
        }
    }

    // Resolver order (issue #873): embed → live → api. The first two
    // hops are gated by `SOLDR_RESOLVER_ORDER` for testing + debugging.
    // The embedded blob is a zstd-compressed snapshot of the live
    // asset-index baked in at build time (see build.rs + manifest_v6),
    // so a hit short-circuits all network traffic. The live fetch
    // remains as a safety net for assets published after the embed was
    // baked. Both feed the same `archive::download_and_extract_with_pin`
    // sha-pin verification path.
    let order = ResolverOrder::from_env();
    if order.try_embed {
        if let VersionSpec::Exact(ref tag) = version {
            if let Some(result) = try_embedded_manifest_v6(
                paths,
                cache_name,
                binary_names,
                repo,
                tag,
                tag_prefix,
                &target,
            )
            .await?
            {
                return Ok(result);
            }
        }
    }
    if order.try_live {
        // Manifest-first (live): when we have an exact version pinned,
        // consult soldr's published asset index on the `manifest`
        // branch. The manifest index carries pinned sha256s, so a hit
        // short-circuits both the release lookup AND the
        // permissive-mode "unverified" warning — the URL we install
        // from is the one the manifest publishes and the bytes are
        // verified against the manifest's pin. Misses + every failure
        // mode (404, parse error, network, SOLDR_MANIFEST_DISABLE=1)
        // fall through to the live GitHub Releases API path below.
        if let VersionSpec::Exact(ref tag) = version {
            if let Some(result) = try_manifest_first(
                paths,
                cache_name,
                binary_names,
                repo,
                tag,
                tag_prefix,
                &target,
            )
            .await?
            {
                return Ok(result);
            }
        }
    }

    let release = github::fetch_release(repo, version, tag_prefix)
        .await
        .map_err(|err| annotate_release_fetch_error(err, repo, version, &target))?;

    if let Some(r) = check_cache(paths, cache_name, &release.version, binary_names, &target)? {
        return Ok(r);
    }

    let asset = github::match_asset(&release.assets, &target).map_err(|err| match err {
        SoldrError::UnsupportedPlatform(message) => SoldrError::UnsupportedPlatform(format!(
            "asset matching failed for {}/{} version {} target {}: {message}",
            repo.owner,
            repo.repo,
            release.version,
            target.triple()
        )),
        other => other,
    })?;

    let binary_path = archive::download_and_extract(
        paths,
        cache_name,
        &release.version,
        &asset.download_url,
        &target,
        binary_names,
    )
    .await?;

    // soldr#936: post-extract smoke test. Confirms the binary is
    // actually executable + not a corrupted-shell-script masquerade
    // (the `4: Syntax error: ")" unexpected` failure mode that
    // motivated this issue). Failure deletes the extracted artifact
    // and bubbles up a clear error so the caller can retry or fail
    // loudly instead of silently caching a broken binary.
    smoke_test_or_evict(&binary_path, cache_name)?;

    Ok(FetchResult {
        binary_path,
        version: release.version,
        cached: false,
    })
}

/// soldr#936: smoke-test a freshly-extracted binary by invoking it
/// with `--version` (with a short timeout). On failure, evict the
/// extracted file so the next fetch attempt does a clean re-download
/// rather than reading the corrupt artifact from cache.
fn smoke_test_or_evict(
    binary_path: &std::path::Path,
    cache_name: &str,
) -> Result<(), SoldrError> {
    use std::process::Command;

    if !binary_path.is_file() {
        return Err(SoldrError::Other(format!(
            "smoke test: {cache_name} binary at {} is not a file after extract",
            binary_path.display()
        )));
    }

    let output = Command::new(binary_path)
        .arg("--version")
        .output();

    let evict = |reason: &str| {
        eprintln!(
            "soldr: smoke test FAILED for {cache_name} at {}: {reason} — evicting from cache",
            binary_path.display()
        );
        let _ = std::fs::remove_file(binary_path);
    };

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            // Some tools (e.g. unusual subcommand stubs) don't support
            // --version cleanly. Fall through to --help as the
            // second-chance probe before evicting.
            let help_ok = Command::new(binary_path)
                .arg("--help")
                .output()
                .map(|h| h.status.success())
                .unwrap_or(false);
            if help_ok {
                Ok(())
            } else {
                evict(&format!(
                    "exit status {} on --version (stderr: {})",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
                Err(SoldrError::Other(format!(
                    "smoke test failed: {cache_name} binary at {} \
                     did not respond to --version / --help — likely \
                     a corrupted download (see soldr#936)",
                    binary_path.display()
                )))
            }
        }
        Err(err) => {
            evict(&format!("exec failed: {err}"));
            Err(SoldrError::Other(format!(
                "smoke test failed: could not exec {cache_name} at {}: {err}",
                binary_path.display()
            )))
        }
    }
}

/// Env var (issue #873) controlling which resolver hops fire. Comma-
/// separated list of `embed`, `live`, `api` (e.g.
/// `SOLDR_RESOLVER_ORDER=live,api` to skip the embedded blob,
/// `SOLDR_RESOLVER_ORDER=api` to skip both manifest hops). Unset or
/// empty → all three hops fire in the default order. Tokens not in the
/// known-set are ignored (forward-compat).
///
/// The `api` hop is always last when listed — there is no way to
/// re-order the hops, only to disable them. This keeps the trust
/// posture intact: a hit from the embed/live manifest is sha-pinned;
/// a hit from the api path is not. Promoting the api path above either
/// manifest path would silently downgrade integrity.
pub const RESOLVER_ORDER_ENV_VAR: &str = "SOLDR_RESOLVER_ORDER";

/// Decoded form of [`RESOLVER_ORDER_ENV_VAR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverOrder {
    pub try_embed: bool,
    pub try_live: bool,
    /// Reserved — the api hop is unconditional today, but the field is
    /// parsed so a future change can disable it for tests.
    pub try_api: bool,
}

impl ResolverOrder {
    /// All three hops, in the canonical embed → live → api order.
    pub const fn all() -> Self {
        Self {
            try_embed: true,
            try_live: true,
            try_api: true,
        }
    }

    /// Parse `SOLDR_RESOLVER_ORDER` from the process environment.
    pub fn from_env() -> Self {
        match std::env::var(RESOLVER_ORDER_ENV_VAR) {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Self::all(),
        }
    }

    /// Parse a comma-separated token list. Unknown tokens are ignored.
    /// Empty / whitespace-only input falls back to `Self::all()`.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::all();
        }
        let mut order = Self {
            try_embed: false,
            try_live: false,
            try_api: false,
        };
        for token in trimmed.split(',') {
            match token.trim().to_ascii_lowercase().as_str() {
                "embed" => order.try_embed = true,
                "live" => order.try_live = true,
                "api" => order.try_api = true,
                _ => {} // unknown token — forward-compat
            }
        }
        order
    }
}

/// Attempt to resolve `(repo, tag)` for the current target via the
/// embedded v6 manifest blob baked into the binary at build time.
///
/// Same trust posture as [`try_manifest_first`]: a hit is sha-pinned
/// against the embed's entry; a sha mismatch is a hard error regardless
/// of `SOLDR_TRUST_MODE`. A miss (no matching tool / triple / version)
/// returns `Ok(None)` so the caller falls through to the live fetch.
async fn try_embedded_manifest_v6(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    tag: &str,
    tag_prefix: Option<&str>,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let Some(manifest) = manifest_v6::embedded_manifest() else {
        return Ok(None);
    };
    if manifest.is_empty() {
        return Ok(None);
    }
    // Normalize the version pin: callers may pass `v1.12.10` (from
    // `known_tools`) but v6 keys are bare semver (`1.12.10`).
    let bare = tag.trim_start_matches('v').to_string();
    let triple_owned = target.triple();
    let triple: &str = &triple_owned;
    let hit = match manifest.lookup(&repo.owner, &repo.repo, triple, Some(&bare)) {
        Some(h) => h,
        None => return Ok(None),
    };
    let version = hit.version.clone();

    if let Some(r) = check_cache(paths, cache_name, &version, binary_names, target)? {
        return Ok(Some(r));
    }

    // Synthesize an asset name from the URL's final path component so
    // the trust-pin log line matches the existing live-fetch shape.
    let asset_name = hit
        .asset
        .href
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(hit.asset.href.as_str());

    eprintln!(
        "soldr: embed-first hit for {}/{} v{} → {}",
        repo.owner, repo.repo, version, asset_name
    );

    let _ = tag_prefix; // accepted for signature symmetry; unused by the v6 lookup.

    let binary_path = archive::download_and_extract_with_pin(
        paths,
        cache_name,
        &version,
        &hit.asset.href,
        target,
        binary_names,
        Some((asset_name, hit.asset.sha256.as_str())),
    )
    .await?;

    Ok(Some(FetchResult {
        binary_path,
        version,
        cached: false,
    }))
}

/// Attempt to resolve `(repo, tag)` for the current target via soldr's
/// published asset index on the `manifest` branch.
///
/// Returns:
///   * `Ok(Some(FetchResult))` — manifest hit, sha256 verified,
///     download + extract succeeded. The caller short-circuits.
///   * `Ok(None)` — manifest miss (no matching entry, manifest empty,
///     manifest disabled, parse failure, network failure). The caller
///     falls through to the live GitHub Releases API.
///   * `Err(_)` — manifest hit but sha256 verification or download
///     failed. This is a hard error — falling back to the live API
///     would mask a tampered manifest entry. Same trust posture as
///     `apple_sdk` / `zig`.
async fn try_manifest_first(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    tag: &str,
    tag_prefix: Option<&str>,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let manifest = manifest_lookup::get_or_fetch().await;
    let candidates = manifest.lookup_release(&repo.owner, &repo.repo, tag);
    // Tag normalization: known_tools may resolve a tag prefix like
    // `cargo-audit/v0.21.0`, so try the bare tag first and the
    // composed tag if nothing matched. Mirrors the candidate-tag
    // expansion `github::fetch_release` does over the live API.
    let candidates_fallback;
    let candidates = if candidates.is_empty() {
        let prefixed = match tag_prefix {
            Some(prefix) => format!("{prefix}{}", tag.trim_start_matches('v')),
            None => return Ok(None),
        };
        candidates_fallback = manifest.lookup_release(&repo.owner, &repo.repo, &prefixed);
        if candidates_fallback.is_empty() {
            return Ok(None);
        }
        candidates_fallback.to_vec()
    } else {
        candidates
    };

    // The asset matcher reuses the existing platform-keyword scoring
    // logic. We synthesize a one-off `AssetInfo` list from the manifest
    // entries and let `match_asset` pick the best fit for our target.
    let asset_infos: Vec<github::AssetInfo> = candidates
        .iter()
        .map(|entry| github::AssetInfo {
            name: entry.asset.clone(),
            download_url: entry.url.clone(),
        })
        .collect();
    let asset = match github::match_asset(&asset_infos, target) {
        Ok(asset) => asset,
        Err(_) => return Ok(None),
    };
    let matched_entry = candidates
        .iter()
        .find(|e| e.asset == asset.name && e.url == asset.download_url)
        .copied()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "manifest-first: matched asset {} but could not find its entry",
                asset.name
            ))
        })?;

    // Tag → version conversion: trim a leading `v` and any monorepo
    // prefix the asset is filed under, to match the layout the live
    // path uses for `~/.soldr/bin/<name>-<version>/`.
    let version = match tag_prefix {
        Some(prefix) => matched_entry
            .tag
            .strip_prefix(prefix)
            .unwrap_or(&matched_entry.tag),
        None => matched_entry.tag.as_str(),
    }
    .trim_start_matches('v')
    .to_string();

    if let Some(r) = check_cache(paths, cache_name, &version, binary_names, target)? {
        return Ok(Some(r));
    }

    eprintln!(
        "soldr: manifest-first hit for {}/{} {} → {}",
        repo.owner, repo.repo, matched_entry.tag, matched_entry.asset
    );

    let binary_path = archive::download_and_extract_with_pin(
        paths,
        cache_name,
        &version,
        &matched_entry.url,
        target,
        binary_names,
        Some((&matched_entry.asset, &matched_entry.sha256)),
    )
    .await?;

    Ok(Some(FetchResult {
        binary_path,
        version,
        cached: false,
    }))
}

fn annotate_release_fetch_error(
    err: SoldrError,
    repo: &github::RepoInfo,
    version: &VersionSpec,
    target: &TargetTriple,
) -> SoldrError {
    let version_desc = match version {
        VersionSpec::Latest => "latest".to_string(),
        VersionSpec::Exact(value) => value.clone(),
    };
    let prefix = format!(
        "release lookup failed for {}/{} requested version {} target {}",
        repo.owner,
        repo.repo,
        version_desc,
        target.triple()
    );
    match err {
        SoldrError::ToolNotFound(message) => {
            SoldrError::ToolNotFound(format!("{prefix}: {message}"))
        }
        SoldrError::Network(message) => SoldrError::Network(format!("{prefix}: {message}")),
        SoldrError::UnsupportedPlatform(message) => {
            SoldrError::UnsupportedPlatform(format!("{prefix}: {message}"))
        }
        SoldrError::Other(message) => SoldrError::Other(format!("{prefix}: {message}")),
        other => SoldrError::Other(format!("{prefix}: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Local cache
// ---------------------------------------------------------------------------

pub(super) fn check_cache(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    binary_names: &[&str],
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let tool_dir = paths.bin.join(format!("{cache_name}-{version}"));
    let bin_name = format!(
        "{}{}",
        binary_names
            .first()
            .ok_or_else(|| SoldrError::Other(format!(
                "no binary names configured for {cache_name}"
            )))?,
        target.binary_ext()
    );
    let binary_path = tool_dir.join(&bin_name);

    if binary_names.iter().all(|binary_name| {
        tool_dir
            .join(format!("{binary_name}{}", target.binary_ext()))
            .exists()
    }) {
        Ok(Some(FetchResult {
            binary_path,
            version: version.to_string(),
            cached: true,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_fetch_predicate_only_retries_network_and_not_found() {
        // `is_transient_fetch_error` gates the retry loop inside
        // `fetch_repo_binary_with_paths`. The two transient classes —
        // GitHub-Releases 404 during propagation, network hiccups — must
        // retry; everything else (malformed archive, missing asset for
        // the target triple, IO errors) is terminal.
        assert!(is_transient_fetch_error(&SoldrError::ToolNotFound(
            "no release found for yfedoseev/crgx".into(),
        )));
        assert!(is_transient_fetch_error(&SoldrError::Network(
            "github api unavailable".into(),
        )));
        assert!(!is_transient_fetch_error(&SoldrError::Archive(
            "corrupt archive".into(),
        )));
        assert!(!is_transient_fetch_error(&SoldrError::Other(
            "no asset matches target x86_64-pc-windows-msvc".into(),
        )));
        assert!(!is_transient_fetch_error(&SoldrError::UnsupportedPlatform(
            "wasm32".into(),
        )));
    }

    // ── Bundled single-binary tools ─────────────────────────────────
    // Locks the contract used by the npm shim and setup-soldr action:
    // when the env var points at a directory containing the requested
    // binary (`tool.exe` on Windows), `resolve_local_single_binary`
    // returns that path verbatim with `cached: true` and a
    // `local-<version>` label.

    fn platform_binary_name(binary_stem: &str) -> String {
        if cfg!(windows) {
            format!("{binary_stem}.exe")
        } else {
            binary_stem.to_string()
        }
    }

    #[test]
    fn resolve_local_crgx_returns_binary_path() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join(platform_binary_name("crgx"));
        std::fs::write(&stub, b"stub").unwrap();

        let result = resolve_local_single_binary(
            tmp.path(),
            CRGX_LOCAL_DIR_ENV_VAR,
            "crgx",
            MANAGED_CRGX_VERSION,
        )
        .expect("should resolve");
        assert_eq!(result.binary_path, stub);
        assert!(result.cached, "bundled path must report cached=true");
        assert!(
            result.version.starts_with("local-"),
            "version should be `local-<ver>`, got: {}",
            result.version
        );
        assert!(
            result.version.contains(MANAGED_CRGX_VERSION),
            "version should embed MANAGED_CRGX_VERSION, got: {}",
            result.version
        );
    }

    #[test]
    fn resolve_local_cargo_chef_returns_binary_path() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join(platform_binary_name("cargo-chef"));
        std::fs::write(&stub, b"stub").unwrap();

        let result = resolve_local_single_binary(
            tmp.path(),
            CARGO_CHEF_LOCAL_DIR_ENV_VAR,
            "cargo-chef",
            CARGO_CHEF_PINNED_VERSION,
        )
        .expect("should resolve");
        assert_eq!(result.binary_path, stub);
        assert!(result.cached, "bundled path must report cached=true");
        assert_eq!(result.version, format!("local-{CARGO_CHEF_PINNED_VERSION}"));
    }

    #[test]
    fn resolve_local_crgx_errors_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("not-a-dir");

        let err = resolve_local_single_binary(
            &nonexistent,
            CRGX_LOCAL_DIR_ENV_VAR,
            "crgx",
            MANAGED_CRGX_VERSION,
        )
        .expect_err("missing dir should error");
        let msg = err.to_string();
        assert!(
            msg.contains("not a directory"),
            "error must explain the dir is missing, got: {msg}"
        );
    }

    #[test]
    fn resolve_local_crgx_errors_when_binary_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty directory — no crgx binary inside.

        let err = resolve_local_single_binary(
            tmp.path(),
            CRGX_LOCAL_DIR_ENV_VAR,
            "crgx",
            MANAGED_CRGX_VERSION,
        )
        .expect_err("missing binary should error");
        let msg = err.to_string();
        assert!(
            msg.contains("is missing"),
            "error must explain the binary is missing, got: {msg}"
        );
        assert!(
            msg.contains(&platform_binary_name("crgx")),
            "error should name the expected binary, got: {msg}"
        );
    }

    crate::timed_test!(resolver_order_parses_default_and_subsets, {
        // Unset / empty / whitespace → all hops enabled.
        assert_eq!(ResolverOrder::parse(""), ResolverOrder::all());
        assert_eq!(ResolverOrder::parse("   "), ResolverOrder::all());
        // Subset selections — embed-only, live-only, api-only.
        let embed_only = ResolverOrder::parse("embed");
        assert!(embed_only.try_embed);
        assert!(!embed_only.try_live);
        assert!(!embed_only.try_api);
        let live_api = ResolverOrder::parse("live,api");
        assert!(!live_api.try_embed);
        assert!(live_api.try_live);
        assert!(live_api.try_api);
        let api_only = ResolverOrder::parse("api");
        assert!(!api_only.try_embed);
        assert!(!api_only.try_live);
        assert!(api_only.try_api);
        // Whitespace + case-insensitive + unknown-token tolerance.
        let mixed = ResolverOrder::parse(" EMBED , Live , garbage ");
        assert!(mixed.try_embed);
        assert!(mixed.try_live);
        assert!(!mixed.try_api);
    });

    crate::timed_test!(resolver_order_env_var_skips_embed_when_unset, {
        // Defensive: ensure the canonical "all on" form keeps embed on.
        // The actual env-var override is exercised in the integration
        // test under `tests/embed_first_resolver.rs` (touching the live
        // process env from a unit test is racy).
        let order = ResolverOrder::all();
        assert!(order.try_embed && order.try_live && order.try_api);
    });

    crate::timed_test!(try_embedded_manifest_v6_misses_on_empty_blob, {
        use crate::core::SoldrPaths;
        // The shipped embed (before this PR's build-time refresh ran)
        // can legitimately be the empty envelope on a fresh checkout.
        // Either way, the lookup for a tool that doesn't exist must
        // return Ok(None) — never an error, never a panic.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let repo = github::RepoInfo {
            owner: "definitely-does-not-exist".to_string(),
            repo: "nor-does-this".to_string(),
        };
        let target = TargetTriple {
            arch: crate::core::Arch::X86_64,
            os: crate::core::Os::Linux,
            env: crate::core::Env::Gnu,
        };
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(try_embedded_manifest_v6(
                &paths,
                "nonesuch",
                &["nonesuch"],
                &repo,
                "1.0.0",
                None,
                &target,
            ))
            .expect("must return Ok, not Err");
        assert!(result.is_none(), "unknown tool must miss");
    });

    #[test]
    fn annotate_release_fetch_error_includes_version_and_target() {
        let repo = github::RepoInfo {
            owner: "LukeMathWalker".to_string(),
            repo: "cargo-chef".to_string(),
        };
        let target = TargetTriple {
            arch: crate::core::Arch::Aarch64,
            os: crate::core::Os::MacOs,
            env: crate::core::Env::None,
        };

        let err = annotate_release_fetch_error(
            SoldrError::ToolNotFound(
                "GitHub release lookup failed: HTTP 404 Not Found".to_string(),
            ),
            &repo,
            &VersionSpec::Exact("0.1.73".to_string()),
            &target,
        );
        let msg = err.to_string();

        assert!(msg.contains("LukeMathWalker/cargo-chef"));
        assert!(msg.contains("0.1.73"));
        assert!(msg.contains("aarch64-apple-darwin"));
        assert!(msg.contains("release lookup"));
    }

    // soldr#936 smoke-test-or-evict regression tests.

    crate::timed_test!(smoke_test_missing_file_errors, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bogus = tmp.path().join("not-a-binary");
        let err = smoke_test_or_evict(&bogus, "fake-tool")
            .expect_err("missing file must fail smoke");
        assert!(
            err.to_string().contains("not a file after extract"),
            "expected 'not a file after extract' in error, got: {err}"
        );
    });

    crate::timed_test!(smoke_test_corrupt_shell_evicts_file, {
        // soldr#936's actual motivating case: a downloaded artifact
        // is actually a corrupted shell script that fails with
        // `4: Syntax error: ")" unexpected`. Smoke runs --version,
        // detects the failure, deletes the bogus file.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bogus_bin = tmp.path().join("cargo-zigbuild");
        std::fs::write(
            &bogus_bin,
            "#!/bin/sh\nsomething_that_will_definitely_fail_to_exec_or_be_a_command\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&bogus_bin).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&bogus_bin, p).unwrap();
        }

        let result = smoke_test_or_evict(&bogus_bin, "cargo-zigbuild");
        // On Windows the shebang fails to execute at all → exec error
        // path. On Unix the script runs but exits 1 → exit-status
        // path. Both paths must evict + error.
        assert!(result.is_err(), "smoke test must fail for corrupt binary");
        assert!(
            !bogus_bin.is_file(),
            "smoke failure must evict the corrupted binary at {bogus_bin:?}"
        );
    });
}
