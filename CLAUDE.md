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

**soldr wraps rustc, NOT cargo.** This is the most important design decision: cargo owns build orchestration, soldr owns per-unit caching. See DESIGN.md "Why no `soldr build`" for rationale. As of #685 (phase 2 of #682) `soldr test` / `soldr clippy` / etc. are accepted as **dispatch shorthand** for `soldr cargo test` / `soldr cargo clippy` — they route through the cargo front door and do not become soldr-native verbs (the `Commands::Cargo` arm is still where the work happens).

## Two build paths — blessed vs legacy (soldr#1010)

`soldr build --target <triple>` is the **blessed-default surface**; `soldr cargo build --target <triple>` is the **explicit legacy passthrough**. Both invoke cargo under the hood — the difference is the toolchain stack each surface mounts and which behaviors evolve at each level:

| Verb | Surface intent | Underlying mechanism (today) | Evolution (soldr#1010) |
|---|---|---|---|
| **`soldr build --target X`** | **blessed default** | Uses cargo under the hood after preparing catalogue-driven sysroots, compiler/linker env, and shims for supported targets. | Resolves required libs/headers from `https://zackees.github.io/soldr-toolchain/catalogue.v1.json`, materializes under `~/.soldr/sdk/<triple>/`, exports env vars, ships a `clang`/`clang-cl` shim so supported cross-compiles land without defaulting through `cargo xwin` or `cargo zigbuild`. Zero-auth, zero-API-quota, pre-compressed `tar.zst`. |
| **`soldr cargo build --target X`** | **explicit legacy** | Passes through the cargo front door and preserves explicit cargo subcommands such as `cargo xwin build` or `cargo zigbuild`. | Stays as the documented fallback when users want the historical path or when blessed misses an asset. `SOLDR_USE_LEGACY_XWIN=1` / `SOLDR_USE_LEGACY_ZIGBUILD=1` surface the same behavior on `soldr build` for diagnostic toggling. |

The split is **a surface contract, not an implementation contract**. `soldr build` may delegate to cargo internally; what matters is that callers asking for `soldr build` get the soldr-blessed toolchain story (with whatever extras land there over time), while callers asking for `soldr cargo build` get the legacy cargo-xwin/zigbuild path verbatim. Internal sharing of the dispatch is allowed and expected.

Friendly target aliases (`win-x64`, `mac-arm64`, etc.) are accepted by both verbs and resolve identically.

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

**Internal workspace split (#1490).** The 2026-05 monocrate collapse is being reversed into `publish = false` internal crates (the zccache pattern, no amalgamation): `crates/soldr-core` is extracted; fetch/cache/daemon splits follow phase by phase. soldr publishes no crates, so workspace membership has no external surface — the old `monocrate_guard.rs` test was deleted with the split. `tests/timed_test_lint.rs` walks every workspace crate under `crates/`.

- **`crates/soldr-core`** — foundation crate: shared types, config (`~/.soldr/config.toml`), target triple resolution (MSVC default on Windows at runtime), error types, the daemon wire schema (`core::wire`), Windows Defender exclusion plumbing (`defender`), `self_relocate`, and the `timed_test!` watchdog. No I/O beyond config files. soldr-cli re-exports all of it at the old paths (`soldr_cli::core`, `soldr_cli::defender`, …), so consumers are unchanged.
- **`crates/soldr-fetch`** — Binary resolution (re-exported as `soldr_cli::fetch`; `build.rs` + `embed/` live with it since `OUT_DIR` is per-crate). Ships several sub-modules:
  - `known_tools` — registry of ecosystem tools with explicit GitHub `(owner, repo)`, cargo subcommand mapping, and optional monorepo tag prefix (e.g. `cargo-audit/v0.21.0`). Keeps dispatch off the crates.io round-trip and handles per-tool release quirks.
  - `trust` — SHA-256 computation + `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` enforcement. Every fetch emits a `trust: verified` or `trust: unverified` line and a pin mismatch is a hard error regardless of mode.
  - `install_zccache` / `rustup_init` — pinned zccache install flow + rustup auto-bootstrap.
  - Resolution chain: local cache → registry-or-crates.io repo lookup → GitHub Releases asset download → extract.
- **`src/cache_lib/`** (formerly `soldr-cache`) — `RUSTC_WRAPPER` logic: hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction, plus the `soldr save` / `soldr load` archive transport and the auto-GC orchestrator. The `[cook]` eviction pass lives in `cache_lib::cook_gc` (issue #589) and is kicked from `gc/auto.rs` on the same throttle as the disk-pressure tiers.
- **`src/soldr_main.rs` + sibling cli modules** — Mode detection in `run()` (the `src/main.rs` binary is a 3-line shim calling `soldr_cli::run()` since #1490 Phase 1), clap for built-ins, exec for tool fetch. The cargo front door (`soldr cargo ...`) inspects the first positional arg; if it matches a `known_tools` `cargo_subcommand`, the corresponding `cargo-<sub>` binary is fetched and prepended to `PATH` before cargo runs.

`src/lib.rs` declares every module exactly once (#1490 Phase 1 removed the historical lib/bin double-declaration, which compiled ~40K LOC twice per build). The CLI entry logic lives in `src/soldr_main.rs`, glob-re-exported at the crate root so `crate::<item>` paths inside the tree resolve unchanged; integration tests keep their `use soldr_cli::core::*`-style imports.

Dependency flow: every module reaches into `crate::core::*` for shared types (resolved through the soldr-core re-export); `fetch` and `cache_lib` each consume `core`; the cli-side modules consume all three.

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
- cargo subcommands invoked via `soldr cargo <sub>`: `nextest`, `deny`, `audit`, `llvm-cov`, `udeps`, `semver-checks`, `expand`, `watch`, `chef`, `zigbuild`, `xwin`, `binstall`, `machete`.
- top-level tools invoked directly via `soldr <tool>`: `cross`, `mdbook`, `cbindgen`, `wasm-pack`, `trunk`, `sccache`, `bacon`, `just`, `typos`.
- `cargo-chef` powers the `soldr cook` content-addressable dep-prebuild (issue #359). It is pinned to v0.1.73 — the most recent release that still ships pre-built archives for Windows MSVC and macOS in addition to the Linux assets the newer releases publish.

Anything not registered falls through the generic External subcommand, which resolves via crates.io → GitHub Releases.

## Key Design Rules

- **Frozen built-in commands**: `status`, `clean`, `config`, `cache`, `version`, `help`, `rustup`, `toolchain`, `doctor`, `optimize`, `cook`, `archive`, `build-from-source`, **`build` (soldr#1010 blessed surface — see "Two build paths" above)** plus the toolchain passthroughs listed above. These are clap-captured and must NOT be repurposed. Bare cargo built-in verbs — `test`, `check`, `run`, `bench`, `doc`, `fmt`, `clippy`, `tree`, `update`, `fix`, `add`, `remove`, `metadata`, `pkgid`, `search`, `vendor`, `yank`, `owner`, `login`, `logout`, `init`, `new`, `generate-lockfile`, `verify-project`, `locate-project`, `report`, `install`, `uninstall`, `publish` — route to `cargo <verb>` via the External arm (see `CARGO_BUILTIN_VERBS` in `cli_args.rs` and the phase-2 hop in `Commands::External` of `main.rs`). They are NOT soldr-native verbs and may not be reused as such; their soldr meaning is "shorthand for `cargo <verb>`." `build` is the **exception** — it has been promoted to a soldr-native surface and is no longer a pure cargo-builtin alias.
- **MSVC on Windows always**: Default to `x86_64-pc-windows-msvc` (or aarch64). Only use GNU if `rust-toolchain.toml` explicitly says so. Target resolved at runtime, not compile-time.
- **Pre-built first**: Try every binary source before `cargo install`. Resolution order matters.
- **RUSTC_WRAPPER defaults to zccache**: If `RUSTC_WRAPPER` is not set, soldr defaults to using `zccache` as the wrapper.
- **Daemon auto-starts**: First `RUSTC_WRAPPER` call starts the cache daemon transparently. No manual `soldr start`.
- **Recovery from a wedged cache** (#1364): if a build hangs on the compile cache, run `soldr --no-cache cargo ...` (note: `--no-cache` goes *before* `cargo`) or set `ZCCACHE_DISABLE=1` — both bypass the wrapper + daemon and run rustc directly. `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=<n>` shortens the 30-min compile-reply backstop to fail fast.
- **Default zccache location stays boring**: Managed `soldr cargo ...` starts and ends zccache sessions without a soldr-private daemon namespace and without setting `ZCCACHE_CACHE_DIR` by default. If the caller sets `ZCCACHE_CACHE_DIR`, soldr forwards it as an explicit zccache override.
- **Parent-cache sharing is default-on**: For managed-zccache builds soldr seeds `ZCCACHE_PATH_REMAP=auto` on the child cargo (issue #352, Tier L1.x). zccache then normalizes absolute source paths inside compiled artifacts so two git worktrees of the same repo serve each other's cache hits via hardlinks. Escape hatch: `SOLDR_PATH_REMAP=off` suppresses the injection; setting `ZCCACHE_PATH_REMAP` yourself wins. Works for non-git checkouts too: since zccache#353, `ZCCACHE_PATH_REMAP=auto` with no `.git/` ancestor falls back to the cwd as the remap root and still injects `--remap-path-prefix=<cwd>=.`, so tarball/zip/git-archive checkouts produce path-independent artifacts and share hits (the `.git/` walk is only how the preferred worktree root is discovered).
- **Integrity is default**: every fetch records sha256. Pins are opt-in via `SOLDR_CHECKSUMS_FILE`; `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.
- **Version independence**: Users install once and forget. CI should pin: `pip install soldr==X.Y.Z`.
- **Local zccache for debugging**: `SOLDR_ZCCACHE_LOCAL_DIR=<path>` skips the managed GitHub-Releases fetch and uses the user's locally-built `zccache.exe` / `zccache-daemon.exe` / `zccache-fp.exe`. Sibling `.pdb` files (Windows), `.dwp` files (Linux), or `.dSYM` directories (macOS) are copied alongside the binaries into `~/.soldr/bin/zccache-local-<sha256[..12]>/` so debuggers can resolve symbols when attaching to the daemon. `soldr doctor` prints a `managed zccache:` section with the resolved binary paths and a `symbol path` line suitable for `cdb -y` / `_NT_SYMBOL_PATH`. The companion helper `bench/build_local_zccache.sh` builds the sibling `zccache` checkout (default `~/dev/zccache`) and prints the env-var export hint. When unset, today's managed-fetch behavior is unchanged. Compatibility note: `SOLDR_ZCCACHE_BIN` (the cli-only test override) is preserved — the new env var is a separate, more comprehensive knob that also drives daemon and fingerprint binary resolution.
- **All Rust toolchain commands go through soldr**: `cargo`, `rustup`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `rustdoc`, `rust-gdb`, `rust-lldb`, and `rust-analyzer` must be invoked as `soldr <tool> ...` (or `uv run soldr <tool> ...`). This includes invocations with leading env-var assignments — `RUSTUP_TOOLCHAIN=... cargo build` is the same policy violation as `cargo build`. The hook at `.claude/hooks/tool_guard.py` enforces this in Claude Code shell tools; the helper script `bench/build_local_zccache.sh` and any documented workflow must follow the same rule. Env-vars prefixed before `soldr` are fine — the policy is about routing the tool, not forbidding env overrides.

## Agent Development Environment Rule (issue #1105)

**For all changes, develop and debug in the local Docker Linux scripts.** Cross-compile to Windows/macOS later, do not let the cross-compile story block feature work.

- **Develop features under Linux first.** The fastest, most reproducible inner loop is `docker build` + `docker run` against the harnesses already in this repo: `examples/docker-cross-win/`, `ci/docker-aarch64-windows-msvc-cross/`, `ci/docker-darwin-cross/`, `ci/docker-aarch64-musl-cross/`, and `docker/cook-shared-cache/`. Use them for the feature build, the unit-test pass, and the smoke run before you ever need a Windows host.
- **Land the Linux-side behavior first, then handle the cross-compile.** Native Windows or macOS only behaviors (vswhere, link.exe, etc.) get test seams (env-var overrides, fixture-driven probes) so the pure logic is exercised under Linux. The host-specific end-to-end gates stay `cfg(target_os = "...")` integration tests and run on the per-PR matrix lanes — they do not block local iteration.
- **Default debugging flow.** Reach for `docker run --rm -v "$PWD:/src" -w /src <harness-image> bash <harness>/build.sh` BEFORE spinning up a Windows VM or asking the user to run things on a Windows host. The harness images already pin the toolchain, soldr release, cargo-xwin / zigbuild, and xwin SDK splat so a fresh clone of the agent's branch reproduces a CI lane locally.
- **When the cross-compile genuinely needs Windows tooling** (live MSVC SDK probe, registry reads, real `link.exe` resolution) gate that path with a runtime opt-out (`SOLDR_*_DISCOVERY=off`-style env vars) and a Linux-friendly fixture path so the same module still compiles + unit-tests on the Linux harness.

This rule was set during the #1105 fix: the `rust-lld` LIB-injection feature was developed and tested entirely under `examples/docker-cross-win/` before the Windows-only end-to-end gate was added.

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

## Bumping soldr's own version (release PRs)

When opening a release PR, **three files must be bumped in lockstep** or every CI lane fails immediately with `error: cannot update the lock file because --locked was passed to prevent this` (the v0.7.65 trap from #1024 / #1025):

1. `Cargo.toml` — `[workspace.package].version`.
2. `package.json` — top-level `"version"`.
3. `Cargo.lock` — the `soldr-cli` package's `version` field. Refresh with a no-op build (`soldr cargo build -p soldr-cli`) AFTER bumping `Cargo.toml`, then `git add Cargo.lock` so the new version is committed alongside the other two.

The regression guard at `crates/soldr-cli/tests/version_lockstep.rs` reads all three files and asserts they match — `soldr cargo test -p soldr-cli --test version_lockstep` (or any full `cargo test` run) fails the build if any of the three drifts. The same test runs on every PR via the `Lint` job.

A pre-merge `cargo metadata --frozen` would also catch the trap (it refuses to run when the lockfile is stale relative to manifests) but adds a separate workflow. The in-tree test covers the same ground with no CI plumbing.

### zccache is embedded (no managed-version pin)

soldr#1368 removed the externally-downloaded managed zccache binary: the zccache CLI now ships as a compiled-in soldr `[[bin]]` built from the `_vender/zccache` submodule library dep. There is no longer a `MANAGED_ZCCACHE_VERSION` pin to keep in lockstep — the only zccache pin is the `_vender/zccache` submodule commit.

## Bumping the vendored zccache pin

soldr#1368 deleted the `MANAGED_ZCCACHE_VERSION` managed-binary download.
The zccache CLI (`zccache`, and the daemon/fingerprint helpers) is compiled
into soldr from the `_vender/zccache` submodule via the
`zccache = { path = "../../_vender/zccache/crates/zccache" }` dep. To move to a
newer zccache, bump the submodule commit:

```bash
cd _vender/zccache && git fetch && git checkout <commit>
cd - && git add _vender/zccache
```

There is no separate managed-version constant, contract `managed_version`, or
download staging to keep in lockstep any more. `zccache::core::VERSION` (the
vendored crate's own version) is what `soldr status` / `soldr doctor` /
`soldr cache` report. The crgx / cargo-chef managed pins are unaffected.

## Reference Docs

- **`PERF.md` — Performance testing. Read this BEFORE running any perf work. See callout at the top of this file.**
- `DESIGN.md` — Authoritative implementation guide, architecture decisions, phase roadmap
- `docs/API.md` — Full CLI specification, environment variables, cache layout
- `docs/CROSS_COMPILE.md` — blessed cross-compile recipes, including managed Windows GNU and MSVC `cargo-xwin`
- `docs/TRUST_BOUNDARIES.md` — Runtime fetch policy, what integrity is enforced, what remains follow-up
- `README.md` — User-facing motivation and prior art comparison

## GitHub Actions workflow conventions

- **Non-trivial logic in workflows belongs in `.github/scripts/*.py`, NOT inline YAML.** The `cross-compile-all-targets.yml` workflow used to inline curl + jq chains for every release-asset lookup, which (a) couples the workflow tightly to GitHub Actions' shell wrapper, (b) is hard to unit-test, (c) duplicates parsing logic between lanes, and (d) makes the YAML unreadable. The current pattern is: write a small Python script under `.github/scripts/` that takes CLI args, expose it with `python3 .github/scripts/<name>.py ...` from the YAML, and keep the YAML to orchestration only (matrix, env, artifact upload/download). Examples: `build_manifest.py`, `tool_query.py`, `print_build_banner.sh`, `ts_step.py`, `run_with_ts.sh`. The scripts have docstrings + smoke-runnable from a developer's shell, so debugging doesn't require pushing a branch.

## Dogfooding

The repo builds itself through soldr so every contributor populates and hits the same cache.

- `./test` routes every `cargo` step through `soldr cargo` when `soldr` is on `PATH`. On a fresh checkout without soldr the script prints a one-line warning to stderr and falls back to bare `cargo` (no caching).
- `.claude/hooks/tool_guard.py` is a `PreToolUse` guard wired in `.claude/settings.json`. It denies bare `cargo`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `python`, `python3`, `pip`, `pip3` in Claude Code shell tools. Route through `soldr cargo ...` / `uv run ...` / `uv pip ...` to satisfy it.
- Unit tests for the hook live next to it: `uv run --no-project --directory .claude/hooks python -m unittest test_tool_guard`. The `--directory` flag puts `tool_guard.py` on `sys.path` so the sibling import resolves.

## Serialization (issues #580 + #603)

- **Binary transports and persisted-state metadata MUST use Protocol Buffers** (via `prost`), not `bincode` / `rmp-serde` / other schema-less formats. The wire schemas live as hand-written `#[derive(prost::Message)]` types beside a `.proto` file that documents them — see `crates/soldr-cli/src/core/wire.rs` + `wire.proto` (conversions in `src/daemon/wire.rs`) and `crates/soldr-cli/src/rust_plan_proto.rs` + `rust_plan_manifest.proto` for the existing pattern. The schema file is the source of truth; round-trip unit tests catch drift.
- **Daemon IPC** (`crates/soldr-cli/src/daemon/{protocol,ipc,wire}.rs`) carries prost-encoded `WireRequest` / `WireResponse` in the frame body. The header is unchanged from prior versions; `PROTOCOL_VERSION` is bumped on every body-format change so peers at different versions error cleanly rather than silently mis-decoding.
- **Persistent redb rows** are `[0x01][prost body]` (the `0x01` tag is reserved for future format extensions). Reads enforce the tag and refuse anything else. On daemon startup, `crate::daemon::db::ensure_initialized` and `crate::cache_lib::cook_index::ensure_initialized` sweep their tables and **drop any row that does not carry the tag** — this is the one-time pre-#580 cleanup. Cook artifacts on disk are unaffected; only the index rows get evicted.
- **`bincode` is no longer a workspace dep.** As of #603 there are zero bincode call sites in the crate. Re-introduction is blocked by clippy: workspace-root `clippy.toml` lists `bincode::serialize` / `bincode::deserialize` under `disallowed-methods`, and `[workspace.lints.clippy]` sets `disallowed_methods = "deny"` (issue #602). `cargo clippy --workspace -- -D warnings` fails on any new call site.
- **Human-edited config (`config.toml`, `rust-toolchain.toml`) stays JSON/TOML.** Protobuf mandate applies only to binary transports + archived metadata.

## Test Infrastructure

- **Per-test watchdog (`timed_test!`)**: Tests must be declared with the `timed_test!` macro from `soldr_cli::test_util`. The default deadline is **2 minutes**; pass a `Duration` as the second argument to override (e.g. `timed_test!(name, Duration::from_secs(300), { ... })`). If the body does not return in time the watchdog prints `TEST HUNG (>Ns): <name>` plus a backtrace to stderr and aborts the test binary, guaranteeing a single hung test cannot block the whole suite. Implementation: `crates/soldr-cli/src/test_util.rs`. The self-test feature `test-watchdog-self-test` plus the `#[ignore]`d `deliberate_hang` cases verify the abort path end-to-end.
- **Lint enforcement (`tests/timed_test_lint.rs`)**: A regression-guard integration test walks `src/**/*.rs` and `tests/*.rs` and fails the build if any *new* file declares a bare `#[test]` instead of using `timed_test!`. Pre-existing files are grandfathered via `LEGACY_ALLOWLIST` in the lint file; the list shrinks as files are migrated. Opt-outs: pair the test with `#[ignore]` (an ignored test cannot hang the suite) or annotate the line with `// allow-bare-test: <reason>` for the rare cases that genuinely cannot use the macro.
