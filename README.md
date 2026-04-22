# Soldr / Rust


<img width="1536" height="1024" alt="ChatGPT Image Apr 19, 2026, 09_43_32 PM" src="https://github.com/user-attachments/assets/87d94693-3542-4f4f-8b02-600bf0b9810e" />


*A tool to download rust tool sets and aggressive cache your build. Instant warm compiles. Gh and local builds. Just add soldr before all your build commands*

Rendered benchmarks: [zackees.github.io/soldr](https://zackees.github.io/soldr/)

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

## GitHub Actions setup

The current GitHub Actions entry point is the repository root action in this repository:

```yaml
- uses: zackees/soldr@<ref>
  with:
    version: 0.7.5
    cache: true

- run: soldr cargo build --locked --release
- run: soldr cargo test --locked
```

That action:

- installs `soldr`
- bootstraps `rustup` into the cached runner-local root when the runner does not already have it
- preinstalls the exact Rust toolchain from `rust-toolchain.toml` by default via `rustup`
- restores a cacheable runner-local root for Soldr, Cargo, and rustup state
- restores and saves the zccache compilation artifact cache at `~/.zccache` by default; set `build-cache: false` to disable it
- puts `soldr` on `PATH` for later steps
- is the extraction source for the planned public `zackees/setup-soldr` action product

If your project pins Rust in `rust-toolchain.toml`, let the action read that file or pass the exact value with `toolchain:`. Do not preinstall a different generic toolchain such as `stable` and assume `soldr` will reconcile it later. The action exports `RUSTUP_TOOLCHAIN` after installation so later `cargo`, `rustc`, and `soldr cargo ...` steps stay on the toolchain it just installed instead of asking `rustup` to resolve a pinned file lazily.

On GitHub-hosted runners, this means you usually do not need a separate toolchain setup action for the normal path. The action still uses `rustup` under the hood today, but it bootstraps `rustup` itself when the runner does not already have it.
On runners without `rustup`, the action downloads and installs it into the cached runner-local root before provisioning the requested toolchain.

For same-repository validation, use `uses: ./`. This repository smoke-tests that path in [setup-soldr-action.yml](./.github/workflows/setup-soldr-action.yml). GitHub Marketplace publication still requires extracting this action into a separate public action repository because GitHub requires a single root `action.yml` and no workflow files in the published repository. The repo-contained extraction plan and intended beta `zackees/setup-soldr@v0` contract live in [docs/SETUP_SOLDR_PUBLIC_ACTION.md](./docs/SETUP_SOLDR_PUBLIC_ACTION.md). Until that public repo exists, treat `zackees/soldr@<ref>` as the current contract and pin a full commit SHA or explicit release tag instead of assuming `@v0`. For fuller examples and fallback patterns, see [INTEGRATION.md](./INTEGRATION.md).

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

- **Compilation caching** (the zccache half): `soldr cargo ...` now fetches and manages a pinned `zccache` release for Rust builds. soldr owns the zccache daemon/session wiring; zccache's artifact store still uses its current default cache root.

```bash
# Build through soldr's front door:
soldr cargo build --release
soldr cargo test
soldr --no-cache cargo test
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
  +-- fetch/start managed zccache when cache is enabled
  +-- set soldr as the compiler wrapper for this build
  +-- have soldr wrapper mode delegate to managed zccache
  +-- delegate to cargo with your existing flags

soldr maturin build --release
  +-- maturin cached? --> run instantly
  +-- not cached?     --> download pre-built binary (2s) --> run
```

## Design goals

- **One obvious command**: Fetch tools, pick the right Windows target, and run through managed zccache through the same entry point.
- **Front-door builds**: `soldr cargo ...` is the primary build UX.
- **Invisible caching**: `soldr cargo ...` uses a soldr-managed zccache by default, with `soldr --no-cache cargo ...` as the opt-out.
- **Real cache controls**: `soldr status`, `soldr cache`, and `soldr clean` report and manage the soldr-managed zccache state instead of placeholder behavior.
- **One cache boundary, eventually**: soldr keeps its own tools and zccache session state in `~/.soldr/`. Current zccache artifacts still live in zccache's default cache root until upstream exposes a supported cache-dir override.
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
