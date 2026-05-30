# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> [!IMPORTANT]
> ## ⚡ Performance work → read [PERF.md](PERF.md) FIRST ⚡
>
> **The Perf Matrix GitHub Action is the MOST important workflow in this repo.**
> If you are testing, measuring, optimizing, or regressing soldr's performance
> — read **[PERF.md](PERF.md)** before doing anything else.
>
> Branch naming (`perf/<plat>-<fix>-<scen>`) controls exactly which cells run.
> Pushing to a wrong branch name silently runs the full sweep — costly and slow.
> **Do not guess.** PERF.md has the complete 48-pattern scope table.

## What is soldr

A single Rust binary with two jobs:
1. **Tool fetcher** — download and run pre-built Rust tool binaries (like npx/crgx)
2. **Compilation cache** — sit in `RUSTC_WRAPPER` slot, hash rustc inputs, cache artifacts (like sccache). `RUSTC_WRAPPER` defaults to `zccache` if not explicitly set.

Mode is detected automatically from argv[1]: path-to-rustc → cache mode, built-in command → dispatch, anything else → tool fetch.

**soldr wraps rustc, NOT cargo.** This is the most important design decision. No `soldr build`, `soldr test`, etc. Cargo owns build orchestration; soldr owns per-unit caching. See DESIGN.md "Why no `soldr build`" for rationale.

## Build Commands

```bash
# Dev environment setup (installs uv if needed)
./install

# Rust
cargo build -p soldr-cli              # Build CLI binary
cargo test --workspace                 # Run all Rust tests
cargo clippy --workspace               # Lint Rust
cargo fmt --all -- --check             # Check Rust formatting

# Python (linting/testing the PyPI wrapper)
./lint                                 # ruff, black, isort, flake8, pylint, mypy
./test                                 # full build + test pipeline

# Maturin (Python+Rust packaging)
uv run maturin develop                 # Build & install in venv
uv run maturin build --release         # Build wheel
```

## Architecture

**Monocrate.** Single Rust crate `soldr-cli` under `crates/soldr-cli/`, four module trees inside it. The four-crate split collapsed in 2026-05; see `crates/soldr-cli/tests/monocrate_guard.rs` for the regression test that fails the build if anyone re-introduces a second crate.

- **`src/core.rs`** (formerly `soldr-core`) — Shared types, config (`~/.soldr/config.toml`), target triple resolution (MSVC default on Windows at runtime), error types. No I/O beyond config files.
- **`src/fetch/`** (formerly `soldr-fetch`) — Binary resolution. Ships several sub-modules:
  - `known_tools` — registry of ecosystem tools with explicit GitHub `(owner, repo)`, cargo subcommand mapping, and optional monorepo tag prefix (e.g. `cargo-audit/v0.21.0`). Keeps dispatch off the crates.io round-trip and handles per-tool release quirks.
  - `trust` — SHA-256 computation + `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` enforcement. Every fetch emits a `trust: verified` or `trust: unverified` line and a pin mismatch is a hard error regardless of mode.
  - `install_zccache` / `rustup_init` — pinned zccache install flow + rustup auto-bootstrap.
  - Resolution chain: local cache → registry-or-crates.io repo lookup → GitHub Releases asset download → extract.
- **`src/cache_lib/`** (formerly `soldr-cache`) — `RUSTC_WRAPPER` logic: hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction, plus the `soldr save` / `soldr load` archive transport and the auto-GC orchestrator.
- **`src/main.rs` + sibling cli modules** — Mode detection in `main()`, clap for built-ins, exec for tool fetch. The cargo front door (`soldr cargo ...`) inspects the first positional arg; if it matches a `known_tools` `cargo_subcommand`, the corresponding `cargo-<sub>` binary is fetched and prepended to `PATH` before cargo runs.

A thin `src/lib.rs` re-exports `pub mod core; pub mod fetch; pub mod cache_lib;` so the four library integration tests (`tests/fetch_crgx.rs`, `tests/lib_install_zccache.rs`, `tests/save_bench.rs`, `tests/save_roundtrip.rs`) can keep using `use soldr_cli::core::*`-style imports after the collapse. The lib and bin trees each declare the three folded-in modules independently, so those modules compile twice — accept the small build-time cost for the test ergonomics.

Dependency flow inside the crate: every module reaches into `crate::core::*` for shared types; `fetch` and `cache_lib` each consume `core`; the cli-side modules consume all three.

Python package (`src/soldr/`) wraps the CLI binary via Maturin as `soldr._native`.

## Supported Tools

Two categories, surfaced as first-class subcommands or via the generic fetch path:

**Rustup toolchain passthroughs** (resolved via `rustup which`):
`rustc`, `rustfmt`, `clippy-driver`, `rustdoc`, `rust-gdb`, `rust-lldb`, `rust-analyzer`.

**Rustup front door** (top-level, direct exec of `rustup` itself — `rustup which rustup` doesn't work):
- `soldr rustup <args>` forwards to the system `rustup` binary. When the first non-flag positional is `target` or `component` and `rust-toolchain.toml` declares a `channel`, soldr injects `--toolchain <channel>` after the verb so per-toolchain state mutations land on the pinned toolchain. Pass `--toolchain` explicitly to opt out of injection.
- `soldr toolchain install` reads `[toolchain].channel` from `rust-toolchain.toml` and runs `rustup toolchain install <channel> --profile minimal --no-self-update`.
- `soldr toolchain prepare` chains install + `component add` + `target add` for every declared component / target, then `cargo install`s every entry under `[soldr.plugins]`.
- `soldr toolchain ensure [--json]` (issue #407 Phase 2) auto-bootstraps rustup if missing, runs the same pipeline as `prepare`, then smoke-verifies the resolved toolchain with `cargo --version` / `rustc --version`. `--json` emits a stable `schema_version: 1` payload consumed by `setup-soldr#133`.
- `soldr toolchain link --shim-dir <path> [--json] [--force]` (issue #407 Phase 3) writes PATH shim files for `cargo`, `rustfmt`, `clippy-driver`, `rustc`, and `rustdoc` that re-exec the running soldr binary. Idempotent (existing-matches → skip); `--force` overwrites differing content. JSON payload uses the same `schema_version: 1` shape as `ensure`.
- `soldr toolchain doctor [--json]` (issue #407 Phase 4) runs env-detection probes (musl-cc availability, pre-populated `target/` warning) and emits either a human summary or the same `schema_version: 1` JSON shape as `ensure`/`link`. Namespaced under `toolchain` to avoid colliding with the top-level `soldr doctor` system check.
  - Manifest example:
    ```toml
    [toolchain]
    channel = "1.94.1"

    [soldr.plugins]
    cargo-nextest = "0.9"
    cargo-zigbuild = { version = "0.18", locked = true }
    cargo-deny = "*"          # any version — `--version` is omitted
    ```
  - `prepare` invokes the cargo binary resolved via `resolve_toolchain_binary("cargo")` directly (NOT through the rustc wrapper) so installs land in soldr-managed `$CARGO_HOME`. The active cargo already obeys `rust-toolchain.toml`, so no channel is threaded through.

**Ecosystem fetches** (registered in `known_tools`, pulled from GitHub Releases):
- cargo subcommands invoked via `soldr cargo <sub>`: `nextest`, `deny`, `audit`, `llvm-cov`, `udeps`, `semver-checks`, `expand`, `watch`, `chef`, `zigbuild`, `xwin`.
- top-level tools invoked directly via `soldr <tool>`: `cross`, `mdbook`, `cbindgen`, `wasm-pack`, `trunk`, `sccache`.
- `cargo-chef` powers the `soldr cook` content-addressable dep-prebuild (issue #359). It is pinned to v0.1.73 — the most recent release that still ships pre-built archives for Windows MSVC and macOS in addition to the Linux assets the newer releases publish.

Anything not registered falls through the generic External subcommand, which resolves via crates.io → GitHub Releases.

## Key Design Rules

- **Frozen built-in commands**: `status`, `clean`, `config`, `cache`, `version`, `help`, `rustup`, `toolchain`, `doctor`, `optimize`, `cook` plus the toolchain passthroughs listed above. Never add `build`, `test`, `lint`, `fmt`, `check`, `doc`, `bench`, `publish` — prevents namespace collision with tool names.
- **MSVC on Windows always**: Default to `x86_64-pc-windows-msvc` (or aarch64). Only use GNU if `rust-toolchain.toml` explicitly says so. Target resolved at runtime, not compile-time.
- **Pre-built first**: Try every binary source before `cargo install`. Resolution order matters.
- **RUSTC_WRAPPER defaults to zccache**: If `RUSTC_WRAPPER` is not set, soldr defaults to using `zccache` as the wrapper.
- **Daemon auto-starts**: First `RUSTC_WRAPPER` call starts the cache daemon transparently. No manual `soldr start`.
- **Parent-cache sharing is default-on**: For managed-zccache builds soldr seeds `ZCCACHE_PATH_REMAP=auto` on the child cargo (issue #352, Tier L1.x). zccache then normalizes absolute source paths inside compiled artifacts so two git worktrees of the same repo serve each other's cache hits via hardlinks. Escape hatch: `SOLDR_PATH_REMAP=off` suppresses the injection; setting `ZCCACHE_PATH_REMAP` yourself wins. Requires a real `.git/` checkout — tarball/zip checkouts silently fall back to no remap.
- **Integrity is default**: every fetch records sha256. Pins are opt-in via `SOLDR_CHECKSUMS_FILE`; `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.
- **Version independence**: Users install once and forget. CI should pin: `pip install soldr==X.Y.Z`.
- **Local zccache for debugging**: `SOLDR_ZCCACHE_LOCAL_DIR=<path>` skips the managed GitHub-Releases fetch and uses the user's locally-built `zccache.exe` / `zccache-daemon.exe` / `zccache-fp.exe`. Sibling `.pdb` files (Windows), `.dwp` files (Linux), or `.dSYM` directories (macOS) are copied alongside the binaries into `~/.soldr/bin/zccache-local-<sha256[..12]>/` so debuggers can resolve symbols when attaching to the daemon. `soldr doctor` prints a `managed zccache:` section with the resolved binary paths and a `symbol path` line suitable for `cdb -y` / `_NT_SYMBOL_PATH`. The companion helper `bench/build_local_zccache.sh` builds the sibling `zccache` checkout (default `~/dev/zccache`) and prints the env-var export hint. When unset, today's managed-fetch behavior is unchanged. Compatibility note: `SOLDR_ZCCACHE_BIN` (the cli-only test override) is preserved — the new env var is a separate, more comprehensive knob that also drives daemon and fingerprint binary resolution.
- **All Rust toolchain commands go through soldr**: `cargo`, `rustup`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `rustdoc`, `rust-gdb`, `rust-lldb`, and `rust-analyzer` must be invoked as `soldr <tool> ...` (or `uv run soldr <tool> ...`). This includes invocations with leading env-var assignments — `RUSTUP_TOOLCHAIN=... cargo build` is the same policy violation as `cargo build`. The hook at `.claude/hooks/tool_guard.py` enforces this in Claude Code shell tools; the helper script `bench/build_local_zccache.sh` and any documented workflow must follow the same rule. Env-vars prefixed before `soldr` are fine — the policy is about routing the tool, not forbidding env overrides.

## Agent Completion Rules

- **Finish on a branch**: When an agent completes a task, it must put the work on a dedicated branch rather than leaving it only in the local worktree.
- **Push the branch**: The branch must be pushed to `origin` before the task is considered complete.
- **Open a pull request**: The agent must create a PR for the branch when permissions allow.
- **Always report the merge URL**: The final user-facing summary must include the PR URL the user should open to review and merge the work.
- **Fallback if PR creation is blocked**: If the GitHub integration cannot open the PR directly, the agent must still push the branch and provide the exact GitHub URL the user needs to open or complete the PR manually.

## Release Publishing Rules

- **Release PRs must bump the package version**: A release is triggered by merging a PR to `main` that bumps `[workspace.package].version` in `Cargo.toml` and the matching `"version"` in `package.json` to a version that is not already published.
- **Do not rely on workflow dispatch alone**: Running `Autonomous Release` manually without an unpublished package version will make the prepare job set `should_release=false`, so build and publish jobs will be skipped.
- **Tags are release outputs, not normal agent inputs**: The release workflow derives `vX.Y.Z` from `Cargo.toml` and creates the matching GitHub tag and release when the tag does not already exist. Do not manually create or push `vX.Y.Z` tags unless the owner explicitly asks for a recovery operation.
- **Check release state before claiming a release is ready**: Verify `Cargo.toml`, `package.json`, PyPI `soldr`, npm `@zackees/soldr`, and `git ls-remote --tags origin vX.Y.Z`. If the candidate version or tag already exists, either bump to the next patch version or stop and report the conflict.

## Toolchain

- Rust 1.94.1 (rust-toolchain.toml), edition 2021, MSRV 1.75
- Python >=3.10 (for PyPI distribution via Maturin)
- uv for Python dependency management
- Workspace dependencies shared in root `Cargo.toml`

## Reference Docs

- **`PERF.md` — Performance testing. Read this BEFORE running any perf work. See callout at the top of this file.**
- `DESIGN.md` — Authoritative implementation guide, architecture decisions, phase roadmap
- `docs/API.md` — Full CLI specification, environment variables, cache layout
- `docs/CROSS_COMPILE.md` — Linux ↔ Windows cross-compile recipes (`cargo-zigbuild`, `cargo-xwin`)
- `docs/TRUST_BOUNDARIES.md` — Runtime fetch policy, what integrity is enforced, what remains follow-up
- `README.md` — User-facing motivation and prior art comparison

## Dogfooding

The repo builds itself through soldr so every contributor populates and hits the same cache.

- `./test` routes every `cargo` step through `soldr cargo` when `soldr` is on `PATH`. On a fresh checkout without soldr the script prints a one-line warning to stderr and falls back to bare `cargo` (no caching).
- `.claude/hooks/tool_guard.py` is a `PreToolUse` guard wired in `.claude/settings.json`. It denies bare `cargo`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `python`, `python3`, `pip`, `pip3` in Claude Code shell tools. Route through `soldr cargo ...` / `uv run ...` / `uv pip ...` to satisfy it.
- Unit tests for the hook live next to it: `uv run --no-project --directory .claude/hooks python -m unittest test_tool_guard`. The `--directory` flag puts `tool_guard.py` on `sys.path` so the sibling import resolves.

## Serialization (issue #580)

- **Binary transports and persisted-state metadata MUST use Protocol Buffers** (via `prost`), not `bincode` / `rmp-serde` / other schema-less formats. The wire schemas live as hand-written `#[derive(prost::Message)]` types beside a `.proto` file that documents them — see `crates/soldr-cli/src/daemon/wire.rs` + `wire.proto` and `crates/soldr-cli/src/rust_plan_proto.rs` + `rust_plan_manifest.proto` for the existing pattern. The schema file is the source of truth; round-trip unit tests catch drift.
- **Daemon IPC** (`crates/soldr-cli/src/daemon/{protocol,ipc,wire}.rs`) carries prost-encoded `WireRequest` / `WireResponse` in the frame body. The header is unchanged from prior versions; `PROTOCOL_VERSION` is bumped on every body-format change so peers at different versions error cleanly rather than silently mis-decoding.
- **Persistent redb rows** added on or after #580 are tagged-byte: `[0x01][prost body]`. Readers classify the leading byte — `0x01` strips and prost-decodes, anything else falls through to a `bincode::deserialize` legacy decoder so rows written by pre-#580 daemons still resolve. The classifier lives in `crate::daemon::wire::classify_redb_row`. The `cook_index` table uses dual versioning (`cook_index_v1` bincode-only, `cook_index_v2` prost-only) instead of tagged-byte; new writes always go to v2, reads consult v2 then v1.
- **`bincode` is on the way out** but not gone yet. The only acceptable bincode call sites are:
  1. The redb legacy-fallback decoders that exist solely to read pre-#580 rows (`crate::daemon::db` + `crate::cache_lib::cook_index` v1 fallback paths).
  2. The `LegacyZccacheDaemonLink` shim in `crate::daemon::db` for the even-older pre-#265 row shape.
- **Human-edited config (`config.toml`, `rust-toolchain.toml`) stays JSON/TOML.** Protobuf mandate applies only to binary transports + archived metadata.
- **Lint enforcement** of "no new bincode usage outside the legacy modules" is a follow-up (see the issue tracker). Until that lands, treat the rule above as the convention; new modules that touch persistent storage or IPC must use prost.

## Test Infrastructure

- **Per-test watchdog (`timed_test!`)**: Tests must be declared with the `timed_test!` macro from `soldr_cli::test_util`. The default deadline is **2 minutes**; pass a `Duration` as the second argument to override (e.g. `timed_test!(name, Duration::from_secs(300), { ... })`). If the body does not return in time the watchdog prints `TEST HUNG (>Ns): <name>` plus a backtrace to stderr and aborts the test binary, guaranteeing a single hung test cannot block the whole suite. Implementation: `crates/soldr-cli/src/test_util.rs`. The self-test feature `test-watchdog-self-test` plus the `#[ignore]`d `deliberate_hang` cases verify the abort path end-to-end.
- **Lint enforcement (`tests/timed_test_lint.rs`)**: A regression-guard integration test walks `src/**/*.rs` and `tests/*.rs` and fails the build if any *new* file declares a bare `#[test]` instead of using `timed_test!`. Pre-existing files are grandfathered via `LEGACY_ALLOWLIST` in the lint file; the list shrinks as files are migrated. Opt-outs: pair the test with `#[ignore]` (an ignored test cannot hang the suite) or annotate the line with `// allow-bare-test: <reason>` for the rare cases that genuinely cannot use the macro.
