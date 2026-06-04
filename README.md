# Soldr / Rust


<img width="1536" height="1024" alt="ChatGPT Image Apr 19, 2026, 09_43_32 PM" src="https://github.com/user-attachments/assets/87d94693-3542-4f4f-8b02-600bf0b9810e" />


*A tool to download rust tool sets and aggressive cache your build. **2× faster cross-PR builds** via content-addressed caching that swatinem's per-key cache cannot share. GH and local builds. Just add soldr before all your build commands.*

Third-party comparison (soldr vs Swatinem/rust-cache vs ...): published from `zackees/setup-soldr` — see its `comparison-cluster` workflow and the rendered page once the migration in soldr#674 completes. The legacy `https://zackees.github.io/soldr/` page is being repurposed for soldr-internal per-scenario regression history (no third-party comparison surface lives on this repo anymore).

[![Linux x64](https://github.com/zackees/soldr/actions/workflows/build-linux-x64.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-linux-x64.yml)
[![Linux ARM64](https://github.com/zackees/soldr/actions/workflows/build-linux-arm64.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-linux-arm64.yml)
[![Linux x64 musl](https://github.com/zackees/soldr/actions/workflows/build-linux-x64-musl.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-linux-x64-musl.yml)
[![Linux ARM64 musl](https://github.com/zackees/soldr/actions/workflows/build-linux-arm64-musl.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-linux-arm64-musl.yml)

[![macOS x64](https://github.com/zackees/soldr/actions/workflows/build-macos-x64.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-macos-x64.yml)
[![macOS ARM64](https://github.com/zackees/soldr/actions/workflows/build-macos-arm64.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-macos-arm64.yml)
[![Windows x64](https://github.com/zackees/soldr/actions/workflows/build-windows-x64.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-windows-x64.yml)
[![Windows ARM64](https://github.com/zackees/soldr/actions/workflows/build-windows-arm64.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/build-windows-arm64.yml)

**Instant tools. Instant builds. One command.**

soldr = [crgx](https://crgx.dev/) + [zccache](https://github.com/zackees/zccache) in a single tool.

**Where correct builds meet instant builds.**

The point of soldr is not to invent some brand-new primitive. The point is to combine the pieces that already work into one tool that people can actually rely on every day.

[zccache](https://github.com/zackees/zccache) is already excellent. [crgx](https://crgx.dev/) already proved the value of instant Rust tooling. soldr turns those into one front door:

- get the right Rust tool for the job
- get the right Windows ABI without thinking about it
- get transparent compilation caching without separate setup

That is the same reason [uv](https://github.com/astral-sh/uv) is compelling. uv did not win because it invented packaging, virtual environments, or Python installation. It won because it made the whole workflow feel like one tool instead of a pile of separate ones.

soldr aims for the same outcome in the Rust toolchain world.

Current release line:

- `0.5.x` is the secure front-door, tool-fetch, and built-in zccache-backed cache release line
- `1.0.0-rc` remains reserved for broader release hardening and bootstrap validation
- the supported external integration boundary remains the `soldr` executable, not the internal Rust crates; see [docs/API_BOUNDARY.md](./docs/API_BOUNDARY.md)
- practical integration examples for local builds and GitHub Actions live in [INTEGRATION.md](./INTEGRATION.md)

## Install from npm

```bash
npm install -g @zackees/soldr
soldr --version
```

The npm package is a small launcher that downloads the matching `soldr` GitHub
Release binary for your OS and architecture during install, verifies it against
the published `SHA256SUMS` file, and exposes the `soldr` command.

## GitHub Actions setup

The current GitHub Actions entry point is the public `setup-soldr` action:

```yaml
- uses: zackees/setup-soldr@v0
  with:
    cache: true

- run: soldr cargo build --locked --release
- run: soldr cargo test --locked
```

That action:

- installs `soldr`
- bootstraps `rustup` into the cached runner-local root when the runner does not already have it
- preinstalls the exact Rust toolchain from `rust-toolchain.toml` by default via `rustup`
- restores a cacheable runner-local root for Soldr, Cargo, and rustup state
- restores and saves the Soldr-owned zccache compilation artifact cache under `SOLDR_CACHE_DIR` by default; set `build-cache: false` to disable it
- restores and saves a zccache-owned Rust artifact plan cache by default for no-op CI fast paths; set `target-cache: false` to disable it or `target-cache-mode: full` to opt into explicit whole-target caching
- puts `soldr` on `PATH` for later steps

For existing workflows where rewriting every `cargo ...` command is high-friction, opt into Cargo PATH shims:

```yaml
- uses: zackees/setup-soldr@v0
  with:
    tool-shims: cargo

- run: cargo build --locked --release
- run: cargo test --locked
```

The shim mode is off by default. When enabled, the action resolves the real Cargo binary before prepending its shim directory, then exports that real path for Soldr so `cargo ...` can safely trampoline into `soldr cargo ...` without recursive PATH lookup.

If your project pins Rust in `rust-toolchain.toml`, let the action read that file or pass the exact value with `toolchain:`. Do not preinstall a different generic toolchain such as `stable` and assume `soldr` will reconcile it later. The action exports `RUSTUP_TOOLCHAIN` after installation so later `cargo`, `rustc`, and `soldr cargo ...` steps stay on the toolchain it just installed instead of asking `rustup` to resolve a pinned file lazily.

On GitHub-hosted runners, this means you usually do not need a separate toolchain setup action for the normal path. The action still uses `rustup` under the hood today, but it bootstraps `rustup` itself when the runner does not already have it.
On runners without `rustup`, the action downloads and installs it into the cached runner-local root before provisioning the requested toolchain.

The public action lives in [`zackees/setup-soldr`](https://github.com/zackees/setup-soldr) and is generated from this repository's root action source. This repository dogfoods `zackees/setup-soldr@v0` in [setup-soldr-action.yml](./.github/workflows/setup-soldr-action.yml). For fuller examples and fallback patterns, see [INTEGRATION.md](./INTEGRATION.md).

### Native vs cross targets

`soldr cargo --target ...` runs the build through soldr/zccache, but it does not fetch a target's Rust standard library. If the active toolchain does not already have that target installed, the canonical failure is `error[E0463]: can't find crate for core/std` (or `compiler_builtins`) at the first compile step.

Native host targets work by default because `rustup` installs the host triple as part of the toolchain. Cross targets must be declared explicitly. Building `aarch64-pc-windows-msvc` from a Windows x86 runner, for example, requires provisioning `aarch64-pc-windows-msvc` before any `soldr cargo --target aarch64-pc-windows-msvc` invocation.

Two equivalent ways to declare a cross target: declaratively via `rust-toolchain.toml`'s `[toolchain].targets` (preferred — `setup-soldr` honors it during toolchain install), or imperatively via `soldr rustup target add` / `soldr toolchain prepare` (see [#331](https://github.com/zackees/soldr/issues/331) and [PR #333](https://github.com/zackees/soldr/pull/333)).

```toml
# rust-toolchain.toml — declarative (preferred)
[toolchain]
channel = "1.94.1"
targets = ["aarch64-pc-windows-msvc"]
```

```bash
# CLI — imperative
soldr rustup target add aarch64-pc-windows-msvc
soldr cargo build --target aarch64-pc-windows-msvc
```

```bash
# Orchestrated
soldr toolchain prepare
soldr cargo build --target aarch64-pc-windows-msvc
```

The canonical multi-platform GitHub Actions tutorial lives in [`zackees/setup-soldr#90`](https://github.com/zackees/setup-soldr/issues/90).

For Linux → Windows cross-compilation (GNU via `cargo-zigbuild`, MSVC via `cargo-xwin`), see [docs/CROSS_COMPILE.md](./docs/CROSS_COMPILE.md).

### CI cache lineage

GitHub Actions caches are not shared across arbitrary sibling feature branches. A workflow run can restore caches from its own branch, the default branch, and for pull requests the PR base branch. It cannot directly restore caches created on another feature branch.

That means Soldr treats `main` as the canonical warm-cache source:

- CI runs on pushes to `main` and feature branches.
- A feature-branch push can save a branch-local cache entry in its own branch scope.
- Later pushes and PRs for that same branch restore that branch-local cache first.
- If the feature branch has no exact cache yet, GitHub falls back to the `main` cache lineage through the same stable keys.
- The heavy cache-producing CI runs on branch pushes, not `pull_request`, so each feature branch gets one useful cache lineage instead of a duplicate PR merge-ref lineage.

In practice this gives the exact parent/child model we want: `main` acts as the shared parent cache, feature branches read from that parent on miss, and each feature branch may also save its own preferred child cache when the workflow runs on `push`. Pull requests then reflect the branch-push CI state instead of creating a second heavy cache path. This repository is the first reference implementation of that pattern. For the full wiring and rollout notes, see [docs/CI_CACHE.md](./docs/CI_CACHE.md).

## Why soldr exists

On Windows, the real problem is not "how do I cache builds?" or "how do I download a tool binary?" in isolation.

The real problem is that the execution path is messy:

- the wrong `cargo` can win on `PATH`
- the wrong Windows target can get selected
- GNU can leak in where MSVC should have been used
- users end up debugging their toolchain instead of shipping code

soldr exists to make that path boring.

When you run `soldr`, the tool should do the obvious thing:

- pick MSVC on Windows by default
- fetch the tool you asked for
- cache it locally
- fetch and manage zccache so Rust builds get transparent caching without manual wrapper setup

If soldr solves that one problem well, it becomes a super tool: the command you reach for first, because it makes the rest of the stack behave.

- **Tool acquisition** (the crgx half): Need `maturin`, `cargo-dylint`, or any crate binary? soldr fetches a pre-built binary from GitHub Releases in seconds. No `cargo install` from source. Cached locally for instant reuse. On `0.5.x`, this is still an upstream trust decision rather than a repo-side trust guarantee; see [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md).

- **Compilation caching** (the zccache half): `soldr cargo ...` now fetches and manages a pinned `zccache` release for Rust builds. soldr owns the zccache daemon/session wiring and keeps managed zccache artifacts under Soldr's cache root.

```bash
# Build through soldr's front door:
soldr cargo build --release
soldr cargo test
soldr --no-cache cargo test
soldr purge
SOLDR_RUSTC_WRAPPER=sccache soldr cargo build
SOLDR_RUSTC_WRAPPER=none soldr cargo build

# Fetch and run any Rust tool instantly:
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
```

## How it works

```text
soldr cargo build --release
  +-- resolve the real cargo binary
  +-- on Windows, re-run from a soldr-owned runtime copy when needed
  +-- fetch/start managed zccache when cache is enabled
  +-- set soldr as the compiler wrapper for this build
  +-- have soldr wrapper mode delegate to managed zccache
  +-- delegate to cargo with your existing flags

soldr maturin build --release
  +-- maturin cached? --> run instantly
  +-- not cached?     --> download pre-built binary (2s) --> run
```

## Linker speed (the other half of fast CI)

soldr caches `rustc` invocations. It does **not** cache the linker step. If your build links many binaries (multiple `tests/*.rs` files, several `[[bin]]` targets, examples, benches), the dominant cost is often `ld`, and zccache will not help with that.

On Linux, switch to the `mold` linker for ~5-10x faster linking. Add to your repo's `.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[target.x86_64-unknown-linux-musl]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

Then in CI, install mold before any cargo step:

```yaml
- name: Install mold linker
  run: |
    sudo apt-get update
    sudo apt-get install -y --no-install-recommends mold
```

macOS uses `ld64`, which is already fast and rarely worth swapping. Windows uses MSVC's linker, which `mold` does not target.

If you also have many separate test binaries, consider consolidating them under one `tests/<name>.rs` entry point with sub-modules. Fewer linker invocations is itself a multiplicative win on top of mold.

## Design goals

- **One obvious command**: Fetch tools, pick the right Windows target, and run through managed zccache through the same entry point.
- **Front-door builds**: `soldr cargo ...` is the primary build UX.
- **Invisible caching**: `soldr cargo ...` uses a soldr-managed zccache by default, with `soldr --no-cache cargo ...` as the opt-out.
- **Real cache controls**: `soldr status`, `soldr cache`, and `soldr clean` report and manage the soldr-managed zccache state, while `soldr purge` removes all Soldr-managed cache artifacts for bug clearing and benchmarking.
- **One cache boundary**: soldr keeps its own tools, zccache session state, and managed zccache artifacts under `~/.soldr/` by default. Use `SOLDR_CACHE_DIR` to move that root.
- **Disposable-worktree friendly on Windows**: for build orchestration, soldr can relocate itself under `~/.soldr/runtime/soldr-self/` so `RUSTC_WRAPPER` does not keep using a worktree-local `soldr.exe`; stale runtime copies are cleaned up periodically.
- **Pre-built first**: Download a pre-built binary before compiling from source. Fall back gracefully.
- **Cargo-compatible**: soldr preserves normal cargo arguments instead of forcing a separate workflow.
- **Cross-platform**: Linux, macOS, Windows (x86_64 + aarch64).
- **MSVC by default on Windows**: Always targets `x86_64-pc-windows-msvc` (or `aarch64-pc-windows-msvc`) unless the active project explicitly selects another target in `.cargo/config.toml`, `.cargo/config`, or `rust-toolchain.toml`. MSVC links against `vcruntime140.dll` which ships with every modern Windows install. The GNU target requires shipping `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` with every binary, which is extra baggage for no benefit. This matches the Rust ecosystem default: rustup, cargo-binstall, and nearly all published release binaries target MSVC. crgx gets this wrong by baking the target at compile time, causing it to look for GNU binaries when compiled under MSYS2.

## Architecture

```
soldr/
|-- crates/
|   |-- soldr-core/      # Shared types, config, cache directory layout
|   |-- soldr-fetch/     # Binary resolution + download (the crgx half)
|   |-- soldr-cache/     # Compilation caching (the zccache half)
|   `-- soldr-cli/       # CLI entry point + wrapper mode
|-- src/soldr/           # Python package (maturin bin bindings)
`-- tests/
```

| Crate | Role |
|---|---|
| `soldr-core` | Cache paths, config, version types |
| `soldr-fetch` | Resolve crate binaries from crates.io metadata and GitHub Releases. Download and cache. |
| `soldr-cache` | zccache integration helpers, cache policy, session plumbing. |
| `soldr-cli` | Mode detection, cargo front door, built-in commands (`status`, `clean`, `config`, `cache`), tool fetch dispatch. |

These workspace crates are implementation details. They are not a supported public Rust library API.

## Prior art

Built on lessons from:
- [zccache](https://github.com/zackees/zccache) - 2.4x faster warm builds than sccache ([benchmark](https://github.com/zackees/zccache/issues/20))
- [crgx](https://crgx.dev/) - the npx of Rust, instant tool execution
- [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) - pre-built binary resolution
- [sccache](https://github.com/mozilla/sccache) - the original Rust compilation cache

## Security And Verification

- [SECURITY.md](./SECURITY.md) describes the current hardening posture and release policy.
- [docs/API_BOUNDARY.md](./docs/API_BOUNDARY.md) defines the supported machine-facing integration boundary.
- [docs/PYPI_TRUSTED_PUBLISHING.md](./docs/PYPI_TRUSTED_PUBLISHING.md) describes the optional Trusted Publishing path for hardened PyPI wheels.
- [`.github/workflows/release-auto.yml`](./.github/workflows/release-auto.yml) is the only release workflow: when a reviewed version bump lands on `main`, it derives the version from `Cargo.toml`, reruns the release gate, and performs final publication through the `release` environment where the release credentials live.
- [RELEASE.md](./RELEASE.md) documents the intended maximum-security release setup and owner workflow.
- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md) explains how to verify published release artifacts.
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md) inventories the external systems and artifacts `soldr` currently trusts, including the current `0.5.x` limits of runtime fetched-binary trust.

## License

BSD-3-Clause.
