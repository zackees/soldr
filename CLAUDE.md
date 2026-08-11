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

A Rust command suite whose main multicall binary has two user-facing jobs:
1. **Tool fetcher** — download and run pre-built Rust tool binaries (like npx/crgx)
2. **Compilation cache** — sit in the `RUSTC_WRAPPER` slot, hash rustc inputs,
   and cache artifacts (like sccache). When no explicit wrapper override is
   set, `RUSTC_WRAPPER` points at a compiler-named Soldr shim that sends work
   to the zccache service embedded in the `soldr-daemon` sidecar.

Mode is detected automatically from argv[1]: path-to-rustc → cache mode, built-in command → dispatch, anything else → tool fetch.

**soldr wraps rustc, NOT cargo.** This is the most important design decision: cargo owns build orchestration, soldr owns per-unit caching. See DESIGN.md "Why no `soldr build`" for rationale. As of #685 (phase 2 of #682) `soldr test` / `soldr clippy` / etc. are accepted as **dispatch shorthand** for `soldr cargo test` / `soldr cargo clippy` — they route through the cargo front door and do not become soldr-native verbs (the `Commands::Cargo` arm is still where the work happens).

## Two build paths — blessed vs legacy (soldr#1010)

`soldr build --target <triple>` is the **blessed-default surface**; `soldr cargo build --target <triple>` is the **explicit legacy passthrough**. Both invoke cargo under the hood — the difference is the toolchain stack each surface mounts and which behaviors evolve at each level:

| Verb | Surface intent | Underlying mechanism (today) | Evolution (soldr#1010) |
|---|---|---|---|
| **`soldr build --target X`** | **blessed default** | Uses cargo under the hood after preparing catalogue-driven sysroots, compiler/linker env, and shims for supported targets. | Resolves required libs/headers from `https://zackees.github.io/soldr-toolchain/catalogue.v1.json`, materializes under `~/.soldr/sdk/<triple>/`, exports env vars, and ships required compiler/linker shims so supported cross-compiles do not invoke the legacy `cargo xwin` or `cargo zigbuild` subcommands. GNU Linux currently uses managed Zig directly through soldr-generated wrappers; a Zig-free implementation is tracked by #2220. Zero-auth, zero-API-quota, pre-compressed `tar.zst`. |
| **`soldr cargo build --target X`** | **explicit legacy** | Passes through the cargo front door and preserves explicit cargo subcommands such as `cargo xwin build` or `cargo zigbuild`. | Stays as the documented fallback when users want the historical path or when blessed misses an asset. `SOLDR_USE_LEGACY_XWIN=1` / `SOLDR_USE_LEGACY_ZIGBUILD=1` surface the same behavior on `soldr build` for diagnostic toggling. |

The split is **a surface contract, not an implementation contract**. `soldr build` may delegate to cargo internally; what matters is that callers asking for `soldr build` get the soldr-blessed toolchain story (with whatever extras land there over time), while callers asking for `soldr cargo build` get the legacy cargo-xwin/zigbuild path verbatim. Internal sharing of the dispatch is allowed and expected.

Friendly target aliases (`win-x64`, `mac-arm64`, etc.) are accepted by both verbs and resolve identically.

### Fresh source checkouts

After cloning or creating a fresh worktree, initialize Soldr's required
zccache submodule before invoking build commands through Soldr:

```bash
git submodule update --init _vender/zccache
```

Soldr detects this specific incomplete source-checkout state and prints the
same remedy before the build tool can report a less actionable
missing-manifest error. It deliberately does not initialize the submodule
automatically, because that would perform an unexpected network fetch during a
build.

## Build Commands

```bash
# Dev environment setup (installs uv if needed)
./install

# Rust
soldr cargo build -p soldr-cli              # Build CLI binary
soldr cargo test --workspace                 # Run all Rust tests
soldr cargo clippy --workspace               # Lint Rust
soldr cargo fmt --all -- --check             # Check Rust formatting

# Python (linting/testing the PyPI wrapper)
./lint                                 # ruff, black, isort, flake8, pylint, mypy
./test                                 # full build + test pipeline

# Maturin (Python+Rust packaging)
uv run maturin develop                 # Build & install in venv
uv run maturin build --release         # Build wheel
```

## Architecture

**Internal workspace split (#1490).** The 2026-05 monocrate collapse is being reversed into `publish = false` internal crates (the zccache pattern, no amalgamation): all five phases have landed: `soldr-core`, `soldr-fetch`, `soldr-cache`, and `soldr-daemon` are extracted, with `soldr-cli` as the facade + `[[bin]]` crate. soldr publishes no crates, so workspace membership has no external surface — the old `monocrate_guard.rs` test was deleted with the split. `crates/soldr-cli/tests/timed_test_lint.rs` walks every workspace crate under `crates/`.

- **`crates/soldr-core`** — foundation crate: shared types, config (`~/.soldr/config.toml`), target triple resolution (MSVC default on Windows at runtime), error types, the daemon wire schema (`core::wire`), Windows Defender exclusion plumbing (`defender`), `self_relocate`, and the `timed_test!` watchdog. No I/O beyond config files. soldr-cli re-exports all of it at the old paths (`soldr_cli::core`, `soldr_cli::defender`, …), so consumers are unchanged.
- **`crates/soldr-fetch`** — Binary resolution (re-exported as `soldr_cli::fetch`; `build.rs` + `embed/` live with it since `OUT_DIR` is per-crate). Ships several sub-modules:
  - `known_tools` — registry of ecosystem tools with explicit GitHub `(owner, repo)`, cargo subcommand mapping, and optional monorepo tag prefix (e.g. `cargo-audit/v0.21.0`). Keeps dispatch off the crates.io round-trip and handles per-tool release quirks.
  - `trust` — SHA-256 computation + `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` enforcement. Every fetch emits a `trust: verified` or `trust: unverified` line and a pin mismatch is a hard error regardless of mode.
  - `rustup_init` — rustup auto-bootstrap. zccache is a vendored Rust
    dependency hosted in-process by `soldr-daemon`, not a fetched tool.
  - Resolution chain: local cache → registry-or-crates.io repo lookup → GitHub Releases asset download → extract.
- **`crates/soldr-cache`** — `RUSTC_WRAPPER` logic (re-exported as `soldr_cli::cache_lib`): hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction, plus the `soldr save` / `soldr load` archive transport and the auto-GC orchestrator. The `[cook]` eviction pass lives in `cache_lib::cook_gc` (issue #589) and is kicked from `gc/auto.rs` on the same throttle as the disk-pressure tiers.
- **`crates/soldr-daemon`** — daemon runtime (re-exported as `soldr_cli::daemon` / `soldr_cli::zccache_embedded`): lifecycle (spawn/displacement/relocation), IPC server, wire codec, running-process v2 broker adoption, and the embedded zccache service the daemon hosts. Depends on soldr-core + soldr-cache.
- **`crates/soldr-cli/src/soldr_main.rs` + sibling CLI modules** — Mode detection in `run()` (`crates/soldr-cli/src/main.rs` is a 3-line binary shim calling `soldr_cli::run()` since #1490 Phase 1), clap for built-ins, exec for tool fetch. The cargo front door (`soldr cargo ...`) inspects the first positional arg; if it matches a `known_tools` `cargo_subcommand`, the corresponding `cargo-<sub>` binary is fetched and prepended to `PATH` before cargo runs.

`crates/soldr-cli/src/lib.rs` declares every module exactly once (#1490 Phase 1 removed the historical lib/bin double-declaration, which compiled ~40K LOC twice per build). The CLI entry logic lives in `crates/soldr-cli/src/soldr_main.rs`, glob-re-exported at the crate root so `crate::<item>` paths inside the tree resolve unchanged; integration tests keep their `use soldr_cli::core::*`-style imports.

Dependency flow: every module reaches into `crate::core::*` for shared types (resolved through the soldr-core re-export); `fetch` and `cache_lib` each consume `core`; `daemon` consumes `core` + `cache_lib`; the cli-side modules consume all four.

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

- **Frozen built-in commands**: `status`, `clean`, `config`, `cache`, `version`, `help`, `rustup`, `toolchain`, `doctor`, `optimize`, `cook`, `archive`, `build-from-source`, `wheel` (soldr#2139 abi3 wheel surface), **`build` (soldr#1010 blessed surface — see "Two build paths" above)** plus the toolchain passthroughs listed above. These are clap-captured and must NOT be repurposed. Bare cargo built-in verbs — `test`, `check`, `run`, `bench`, `doc`, `fmt`, `clippy`, `tree`, `update`, `fix`, `add`, `remove`, `metadata`, `pkgid`, `search`, `vendor`, `yank`, `owner`, `login`, `logout`, `init`, `new`, `generate-lockfile`, `verify-project`, `locate-project`, `report`, `install`, `uninstall`, `publish` — route to `cargo <verb>` via the External arm (see `CARGO_BUILTIN_VERBS` in `crates/soldr-cli/src/cli_args.rs` and the phase-2 hop in `Commands::External` of `crates/soldr-cli/src/soldr_main.rs`). They are NOT soldr-native verbs and may not be reused as such; their soldr meaning is "shorthand for `cargo <verb>`." `build` is the **exception** — it has been promoted to a soldr-native surface and is no longer a pure cargo-builtin alias.
- **Standalone native compiler verbs are frozen**: `cc` and `c++` are clap-captured catalogue compiler surfaces (soldr#2335) and must not fall through to external-tool fetching.
- **MSVC on Windows always**: Default to `x86_64-pc-windows-msvc` (or aarch64). Only use GNU if `rust-toolchain.toml` explicitly says so. Target resolved at runtime, not compile-time.
- **Pre-built first**: Try every binary source before `cargo install`. Resolution order matters.
- **The wrapper route stays inside Soldr**: If caching is enabled and no
  explicit `SOLDR_RUSTC_WRAPPER` override is set, `soldr cargo ...` installs a
  compiler-named Soldr shim in `RUSTC_WRAPPER`. Each compile re-enters Soldr
  and is sent over Soldr IPC to the zccache service embedded in
  `soldr-daemon`; Cargo never launches a standalone zccache wrapper/daemon on
  this path.
- **Soldr daemon auto-starts**: The first cacheable wrapper call starts
  `soldr-daemon`, which owns the embedded zccache service. No standalone
  zccache process is started and no manual `soldr start` is required.
- **Recovery from a wedged cache**: `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=<n>` shortens the 30-min compile-reply backstop so the build fails quickly. Inspect `soldr doctor`, `soldr status`, and `soldr logs paths`; restart the broker-owned route with `soldr daemon stop` followed by `soldr daemon start`. Cacheable compiler work never silently bypasses the broker/daemon. See [docs/DAEMON_TIMEOUTS.md](docs/DAEMON_TIMEOUTS.md).
- **Embedded cache location is Soldr-owned**: By default the service receives
  `~/.soldr/cache/zccache/daemon-state/embedded-v1` as its top-level cache
  root and zccache versions persistent state beneath `v<VERSION>/`. Per-build
  history is stored under `~/.soldr/cache/zccache/history/`.
  `SOLDR_CACHE_DIR` moves that embedded compiler-store boundary.
  `ZCCACHE_CACHE_DIR` remains a front-door/session, rust-plan, and rustfmt
  compatibility override; it does not relocate the service hosted by
  `soldr-daemon`.
- **Parent-cache sharing is default-on**: For managed-zccache builds soldr seeds `ZCCACHE_PATH_REMAP=auto` on the child cargo (issue #352, Tier L1.x). zccache then normalizes absolute source paths inside compiled artifacts so two git worktrees of the same repo serve each other's cache hits via hardlinks. Escape hatch: `SOLDR_PATH_REMAP=off` suppresses the injection; setting `ZCCACHE_PATH_REMAP` yourself wins. Works for non-git checkouts too: since zccache#353, `ZCCACHE_PATH_REMAP=auto` with no `.git/` ancestor falls back to the cwd as the remap root and still injects `--remap-path-prefix=<cwd>=.`, so tarball/zip/git-archive checkouts produce path-independent artifacts and share hits (the `.git/` walk is only how the preferred worktree root is discovered).
- **Integrity is default**: every fetch records sha256. Pins are opt-in via `SOLDR_CHECKSUMS_FILE`; `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.
- **Version independence**: Users install once and forget. CI should pin: `pip install soldr==X.Y.Z`.
- **Local zccache development**: Edit the `_vender/zccache` submodule and
  rebuild Soldr; the resulting Soldr binaries contain that source and its
  symbols. `SOLDR_ZCCACHE_LOCAL_DIR` and `SOLDR_ZCCACHE_BIN` are legacy
  compatibility names and do not replace the embedded service on the normal
  path. To deliberately test an external compiler wrapper, set
  `SOLDR_RUSTC_WRAPPER=/path/to/zccache` (or another wrapper) explicitly.
- **One canonical toolchain-home pair per execution, chosen by where the binary lives** (soldr#1799/#1768): soldr keeps private managed `RUSTUP_HOME`/`CARGO_HOME` for dylint's nightly, and they are applied **only** when the resolved binary physically lives inside those managed homes (`binaries::apply_resolved_toolchain_homes`). A host-resolved `cargo`/`rustc`/`rustfmt`/`clippy` always executes under the caller's own homes — never by ambient env leakage. This matters because the failure is silent: flipping homes between runs changes which rustc is used, which invalidates cargo's fingerprints and zccache's keys, so a warm build recompiles the world and is merely 10-50x slower, indefinitely. Every build log records `home_origin` (`caller` | `managed` | `repo-local`) beside the resolved `binary`, and `.github/scripts/check_toolchain_homes.py` fails CI when a row claims `managed` for a binary outside a managed root.
- **All Rust toolchain commands go through soldr**: `cargo`, `rustup`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `rustdoc`, `rust-gdb`, `rust-lldb`, and `rust-analyzer` must be invoked as `soldr <tool> ...` (or `uv run soldr <tool> ...`). This includes invocations with leading env-var assignments — `RUSTUP_TOOLCHAIN=... cargo build` is the same policy violation as `cargo build`. clud enforces this in agent shell tools (see Dogfooding below — the in-repo hook is no longer the enforcement point); the helper script `bench/build_local_zccache.sh` and any documented workflow must follow the same rule. Env-vars prefixed before `soldr` are fine — the policy is about routing the tool, not forbidding env overrides.

## Agent Development Environment Rule (issue #1105)

**For all changes, develop and debug in the local Docker Linux scripts.** Cross-compile to Windows/macOS later, do not let the cross-compile story block feature work.

- **Develop features under Linux first.** The fastest, most reproducible inner loop is `docker build` + `docker run` against the harnesses already in this repo: `examples/docker-cross-win/`, `ci/docker-aarch64-windows-msvc-cross/`, `ci/docker-darwin-cross/`, `ci/docker-aarch64-musl-cross/`, and `docker/cook-shared-cache/`. Use them for the feature build, the unit-test pass, and the smoke run before you ever need a Windows host.
- **Land the Linux-side behavior first, then handle the cross-compile.** Native Windows or macOS only behaviors (vswhere, link.exe, etc.) get test seams (env-var overrides, fixture-driven probes) so the pure logic is exercised under Linux. The host-specific end-to-end gates stay `cfg(target_os = "...")` integration tests and run on the per-PR matrix lanes — they do not block local iteration.
- **Default debugging flow.** For ordinary Linux builds, use `uv run --no-project python ci/perf_local.py cargo <args>` so every agent reuses the named `soldr-perf-local` runner and its persistent target/Cargo/soldr volumes. The runner mounts the shared checkout root, so repository worktrees use `docker exec -w` instead of creating per-agent containers. Use the target-specific harness scripts only for their cross-compile environments; do not create another general-purpose soldr development container. `ci/perf_local.py --stop` and `--reset-runner` preserve volumes, while `--wipe` is the explicit destructive reset. Reach for these Docker harnesses before spinning up a Windows VM or asking the user to run things on a Windows host.
- **When the cross-compile genuinely needs Windows tooling** (live MSVC SDK probe, registry reads, real `link.exe` resolution) gate that path with a runtime opt-out (`SOLDR_*_DISCOVERY=off`-style env vars) and a Linux-friendly fixture path so the same module still compiles + unit-tests on the Linux harness.

This rule was set during the #1105 fix: the `rust-lld` LIB-injection feature was developed and tested entirely under `examples/docker-cross-win/` before the Windows-only end-to-end gate was added.

## Working Location Rule

**Do all soldr work directly in this repository checkout. No sibling clones, no
git worktrees — not even under this repo.** Owner directive (2026-08-10):
create a feature branch here and work on it in place; do not `git clone` a
sibling copy (`../soldr-wt-*`, `../soldr2`, …) and do not `git worktree add` a
linked tree. Sibling/worktree checkouts break the Docker Linux runner
(`ci/perf_local.py` mounts *this* checkout root as `/repo`, so a tree outside it
is invisible) and fragment the warm cargo/soldr volumes. Switch branches in
place with `git checkout -b`; if another agent needs isolation, coordinate on a
branch, not a second working tree.

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

- Rust 1.94.1 (rust-toolchain.toml), edition 2021, MSRV 1.94.1
  (`[workspace.package].rust-version`). The MSRV and the pinned toolchain are
  the same version — soldr does not support building on an older compiler, so
  "will this still build on the MSRV?" is never a reason to avoid a newer std
  API. Guarded by `crates/soldr-cli/tests/msrv_doc_matches_manifest.rs`.
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

soldr#1368 removed the externally-downloaded managed zccache binary: the zccache CLI now ships as a compiled-in soldr `[[bin]]` built from the `_vender/zccache` submodule library dep. There is no longer a `MANAGED_ZCCACHE_VERSION` pin to keep in lockstep — the only zccache pin *in soldr's source* is the `_vender/zccache` submodule commit.

> [!IMPORTANT]
> That is true of the compiled-in library and of what `soldr status` reports.
> It is **not** true of release staging, which still downloads a prebuilt
> zccache keyed on the vendored crate's version — see
> "Bumping the vendored zccache pin" below before moving the submodule.

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

**Prerequisite: the target version must already be published as a zccache
release.** `cross-compile-all-targets.yml` reads the version straight out of
the vendored manifest and demands a matching prebuilt asset, which it stages
into the release archive:

```bash
zccache_version=$(sed -n 's/^version = "\(.*\)"/\1/p' \
  _vender/zccache/Cargo.toml | head -n1)
asset_metadata=$(python3 .github/scripts/toolchain_asset_query.py \
    --platform linux --arch x86 --extra musl \
    --version "$zccache_version" --json zccache)
```

So the pin cannot lead the release. Check before bumping — this fails fast
and names the versions that do exist:

```bash
uv run --no-project python .github/scripts/toolchain_asset_query.py \
  --platform linux --arch x86 --extra musl --version <new-version> --json zccache
```

If it prints `no release '<version>' in tool manifest`, publish zccache
first. soldr#2164 moved the pin to a version with no asset; every local
signal was green — builds, ~1460 tests, clippy, `verify_vendor_state.py`,
`loc_ratchet` — because **none of them exercise that fetch**, and
`bootstrap + linux-x86` went red on main instead.

There is no separate managed-version constant or contract `managed_version`
to keep in lockstep. `zccache::core::VERSION` (the vendored crate's own
version) is what `soldr status` / `soldr doctor` / `soldr cache` report, and
is also the string the asset query above uses. The crgx / cargo-chef managed
pins are unaffected.

## Reference Docs

- **`PERF.md` — Performance testing. Read this BEFORE running any perf work. See callout at the top of this file.**
- `DESIGN.md` — Authoritative implementation guide, architecture decisions, phase roadmap
- `docs/API.md` — Full CLI specification, environment variables, cache layout
- `docs/CONTRIBUTING_TESTS.md` — portable and native platform test conventions, including target-run archive coverage
- `docs/CROSS_COMPILE.md` — blessed cross-compile recipes, including managed Windows GNU and MSVC `cargo-xwin`
- `docs/DEBUG_SIDECARS.md` — debug-symbol sidecar policy for release archives (`.pdb` / `.dSYM` / `.dwp`, `manifest.json` `debug_info` contract)
- `docs/TRUST_BOUNDARIES.md` — Runtime fetch policy, what integrity is enforced, what remains follow-up
- `docs/DAEMON_TIMEOUTS.md` — Daemon timeout & stall runbook: failure mode → signal → bounded broker-owned recovery, plus `soldr doctor`/`soldr status` diagnostics
- `README.md` — User-facing motivation and prior art comparison

## GitHub Actions workflow conventions

- **Complex CI logic inline in workflow YAML is BANNED. If the logic can be moved to a `ci/*.py` (or `.github/scripts/*.py`) file, it must be.** GitHub Actions YAML is notoriously hard to test — the only way to exercise an inline `run:` block is to push a branch and watch a runner — while a Python file is unit-testable under `tests/`, smoke-runnable from a developer's shell, and reviewable as code. Workflow YAML stays orchestration-only: matrix definitions, `needs:` edges, env plumbing, artifact upload/download, and one-line invocations like `python3 ci/<name>.py ...` or `python3 .github/scripts/<name>.py ...`. Prefer `ci/*.py` for logic that is also useful outside Actions (local dev loops, e.g. `ci/perf_local.py`) and `.github/scripts/*.py` for workflow-only helpers; prefer extending an existing script over adding a new one, and never grow an inline bash/python block instead.
- The historical motivation: `cross-compile-all-targets.yml` used to inline curl + jq chains for every release-asset lookup, which (a) couples the workflow tightly to GitHub Actions' shell wrapper, (b) is hard to unit-test, (c) duplicates parsing logic between lanes, and (d) makes the YAML unreadable. Extracted examples: `build_manifest.py`, `tool_query.py`, `print_build_banner.sh`, `ts_step.py`, `run_with_ts.sh`. The scripts have docstrings and take CLI args, so debugging doesn't require pushing a branch. Existing workflows still carrying large inline blocks (e.g. `release-auto.yml`) are grandfathered but must shrink, not grow: any change touching an inline block should extract it rather than extend it.

## Per-file line ceiling (soldr#1966)

`.github/scripts/loc_ratchet.py` runs in the `Lint` job on every PR and enforces
the 1,500-line ceiling as a **ratchet**, not a threshold:

- a file at or under the ceiling must stay at or under it;
- a file already over it may not get **bigger**;
- shrinking, and deleting (i.e. splitting), are always allowed.

There is no grandfather list to maintain — the baseline is the file's size at
the merge base. Thirteen files are already over the ceiling, and blocking every
PR that touches them would have been worse than the drift, so the rule is
"don't make it worse" rather than "fix it before you may proceed".

If a PR fails this check, the addition belongs in a new module. The split the
check is asking for is a change that was already overdue — but note that
splitting a popular file is a **rename**, and renames conflict destructively
with in-flight branches (soldr#1962 became a modify/delete conflict where
taking the delete compiled, passed CI, and silently dropped the fix). Check
`git branch -a --contains` for live work before splitting a hot file.

## Dogfooding

The repo builds itself through soldr so every contributor populates and hits the same cache.

- `./test` routes every `cargo` step through `soldr cargo` when `soldr` is on `PATH`. On a fresh checkout without soldr the script prints a one-line warning to stderr and falls back to bare `cargo` (no caching).
- **Command policy is enforced by clud, not by an in-repo hook.** `.claude/settings.json` and `.codex/hooks.json` are both intentionally empty (`{}`); soldr#1634 delegated enforcement to clud's repo-scoped `.clud/settings.json`. `.codex/README.md` records the same arrangement. What is enforced is unchanged — bare `cargo`, `rustc`, `rustfmt`, `clippy-driver`, `cargo-clippy`, `cargo-fmt`, `python`, `python3`, `pip`, `pip3` are denied in agent shell tools; route through `soldr cargo ...` / `uv run ...` / `uv pip ...`.
- `.claude/hooks/tool_guard.py` is the **previous** implementation of that policy, kept in-tree and still unit-tested, but no longer wired to anything. Do not rely on it as the enforcement point, and do not assume a bare `cargo` is mechanically blocked by this repository alone.
- Unit tests for the hook live next to it: `uv run --no-project --directory .claude/hooks python -m unittest test_tool_guard`. The `--directory` flag puts `tool_guard.py` on `sys.path` so the sibling import resolves.

## Serialization (issues #580 + #603)

- **Binary transports and persisted-state metadata MUST use Protocol Buffers** (via `prost`), not `bincode` / `rmp-serde` / other schema-less formats. The daemon wire schema lives in `crates/soldr-core/src/core/wire.rs` beside `wire.proto`, with conversions in `crates/soldr-daemon/src/daemon/wire.rs`; the rust-plan schema lives in `crates/soldr-cli/src/rust_plan_proto.rs` beside `rust_plan_manifest.proto`. The schema file is the source of truth; round-trip unit tests catch drift.
- **Daemon IPC** (`crates/soldr-daemon/src/daemon/{protocol,ipc,wire}.rs`) carries prost-encoded `WireRequest` / `WireResponse` in the frame body. The header is unchanged from prior versions; `PROTOCOL_VERSION` is bumped on every body-format change so peers at different versions error cleanly rather than silently mis-decoding.
- **Persistent redb rows** are `[0x01][prost body]` (the `0x01` tag is reserved for future format extensions). Reads enforce the tag and refuse anything else. On daemon startup, `crate::daemon::db::ensure_initialized` and `crate::cache_lib::cook_index::ensure_initialized` sweep their tables and **drop any row that does not carry the tag** — this is the one-time pre-#580 cleanup. Cook artifacts on disk are unaffected; only the index rows get evicted.
- **`bincode` is no longer a workspace dep.** As of #603 there are zero bincode call sites in the crate. Re-introduction is blocked by clippy: workspace-root `clippy.toml` lists `bincode::serialize` / `bincode::deserialize` under `disallowed-methods`, and `[workspace.lints.clippy]` sets `disallowed_methods = "deny"` (issue #602). `cargo clippy --workspace -- -D warnings` fails on any new call site.
- **Human-edited config (`config.toml`, `rust-toolchain.toml`) stays JSON/TOML.** Protobuf mandate applies only to binary transports + archived metadata.

## Test Infrastructure

See `docs/CONTRIBUTING_TESTS.md` for the portable/native test boundary and how
platform behavioral tests reach the target-run lanes.

- **Per-test watchdog (`timed_test!`)**: Tests must be declared with the `timed_test!` macro re-exported by `soldr_cli` and implemented in `crates/soldr-core/src/test_util.rs`. The default deadline is **2 minutes**; pass a `Duration` as the second argument to override (e.g. `timed_test!(name, Duration::from_secs(300), { ... })`). If the body does not return in time the watchdog prints `TEST HUNG (>Ns): <name>` plus a backtrace to stderr and aborts the test binary, guaranteeing a single hung test cannot block the whole suite. The self-test feature `test-watchdog-self-test` plus the `#[ignore]`d `deliberate_hang` cases verify the abort path end-to-end.
- **Triaging a red lane that shows `0xC0000409`**: On Windows, `abort()` raises `__fastfail(FAST_FAIL_FATAL_APP_EXIT)` and Windows reports it as `0xC0000409 STATUS_STACK_BUFFER_OVERRUN`. **It is a watchdog timeout, not memory corruption** (soldr#1999). To attribute it:
  1. Find the `ABORT [ Ns] (n/total) <crate>::<binary> <test_name>` line in the log — nextest names the test that blew its budget. The `SLOW [>120.000s]` line just above it is the same test.
  2. Decide whether that named test can even reach your change. A tool-fetch-only or parser-only test, for example, cannot be affected by daemon/IPC changes.
  3. These Windows `target-run` lanes abort on `main` too, under runner contention — **rerun the failed job before blaming the PR** (`gh run rerun <run-id> --failed`), and diff a red lane against `main` *by test name*, not by lane name.
  4. Only raise a test's budget if the test is *legitimately* long. A stub-driven test that suddenly takes 120 s is an environmental stall; raising the budget masks the hang the watchdog exists to surface.
- **Lint enforcement (`crates/soldr-cli/tests/timed_test_lint.rs`)**: A regression-guard integration test walks every workspace crate's Rust sources and integration tests and fails the build if any *new* file declares a bare `#[test]` instead of using `timed_test!`. Pre-existing files are grandfathered via `LEGACY_ALLOWLIST` in the lint file; the list shrinks as files are migrated. Opt-outs: pair the test with `#[ignore]` (an ignored test cannot hang the suite) or annotate the line with `// allow-bare-test: <reason>` for the rare cases that genuinely cannot use the macro.
