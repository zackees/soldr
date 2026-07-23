# Soldr / Rust


<img width="1536" height="1024" alt="ChatGPT Image Apr 19, 2026, 09_43_32 PM" src="https://github.com/user-attachments/assets/87d94693-3542-4f4f-8b02-600bf0b9810e" />


*A tool to download rust tool sets and aggressive cache your build. **2× faster cross-PR builds** via content-addressed caching that swatinem's per-key cache cannot share. GH and local builds. Just add soldr before all your build commands.*

Third-party comparison (soldr vs Swatinem/rust-cache vs ...): published from `zackees/setup-soldr` — see its `comparison-cluster` workflow and the rendered page once the migration in soldr#674 completes. The legacy `https://zackees.github.io/soldr/` page is being repurposed for soldr-internal per-scenario regression history (no third-party comparison surface lives on this repo anymore).

[![CI](https://github.com/zackees/soldr/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/ci.yml)
[![Autonomous Release](https://github.com/zackees/soldr/actions/workflows/release-auto.yml/badge.svg?branch=main)](https://github.com/zackees/soldr/actions/workflows/release-auto.yml)

## Performance

[![soldr vs sccache vs bare cargo - Rust workload](https://raw.githubusercontent.com/zackees/soldr/benchmark-stats/benchmark-rust-only.jpg)](https://zackees.github.io/soldr/)
[![soldr vs sccache vs bare cargo - Rust+C workload](https://raw.githubusercontent.com/zackees/soldr/benchmark-stats/benchmark-rust-c.jpg)](https://zackees.github.io/soldr/)

*[performance details](https://zackees.github.io/soldr/)*

Cold builds are a wash; clean-target reconstruction from a warm compiler cache and cross-worktree sharing are where soldr's wrapper architecture pays off. The README chart's clean-target row intentionally deletes `target/`; it does not claim Cargo's intact-target freshness fast path. Full historical trend + interactive view: [zackees.github.io/soldr](https://zackees.github.io/soldr/). For the swatinem/rust-cache comparison (GHA target-dir caching, a different layer) see [PERF.md](PERF.md#readme-comparison-bars-issue-785).

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

Published npm archives and PyPI wheels support both Intel (`x86_64`) and Apple
Silicon (`arm64`) macOS. Intel artifacts are cross-built through soldr's
blessed Apple SDK path and smoke-tested on an Intel macOS runner before release.

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

For Windows x64 → Windows GNU builds via managed MinGW-w64 GCC, and Linux →
Windows MSVC cross-compilation via the blessed `soldr build` surface (with
`soldr cargo xwin` retained as an explicit legacy fallback),
see [docs/CROSS_COMPILE.md](./docs/CROSS_COMPILE.md).

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
- run the built-in zccache service so Rust builds get transparent caching without manual wrapper setup

If soldr solves that one problem well, it becomes a super tool: the command you reach for first, because it makes the rest of the stack behave.

- **Tool acquisition** (the crgx half): Need `maturin`, `cargo-dylint`, or any crate binary? soldr fetches a pre-built binary from GitHub Releases in seconds. No `cargo install` from source. Cached locally for instant reuse. On `0.5.x`, this is still an upstream trust decision rather than a repo-side trust guarantee; see [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md).

- **Compilation caching** (the zccache half): `soldr cargo ...` uses the zccache service compiled into Soldr. soldr owns the daemon/session wiring and keeps its object store under the active Soldr root at `cache/zccache/daemon-state/embedded-v1/<zccache-version>/objects`, so production and development roots remain isolated.

```bash
# Build through soldr's front door:
soldr cargo build --release
soldr cargo test
soldr --no-cache cargo test
soldr purge
SOLDR_RUSTC_WRAPPER=sccache soldr cargo build
SOLDR_RUSTC_WRAPPER=none soldr cargo build

# Shorthand: drop the `cargo` prefix for cargo's built-in verbs and for
# any cargo subcommand soldr already prebuilds. `soldr cargo <verb>` is
# preserved as the explicit escape hatch (always works), and the
# collision verbs `clean`, `config`, and `version` keep their
# soldr-native meaning. See docs/API.md "Cargo Verb Shorthand" for the
# full list.
soldr build --release          # == soldr cargo build --release
soldr test --workspace         # == soldr cargo test --workspace
soldr clippy -- -D warnings    # == soldr cargo clippy -- -D warnings
soldr nextest run              # == soldr cargo nextest run

# Fetch and run any Rust tool instantly:
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
```

## Use soldr as your PEP 517 build backend (instead of maturin)

For Rust+Python packages, point `pyproject.toml` at soldr instead of
maturin and `pip install .` / `uv pip install .` route the whole build
through soldr:

```toml
[build-system]
requires = ["soldr"]
build-backend = "soldr"

# Your existing [tool.maturin] section stays exactly as it is —
# soldr drives a pinned maturin under the hood, so all maturin
# configuration keeps working unchanged.
[tool.maturin]
manifest-path = "crates/my-crate/Cargo.toml"
module-name = "my_pkg._native"
python-source = "src"
```

That is the entire change — no `maturin` entry in `requires`, no other
files touched. What you get over `build-backend = "maturin"`:

- **Pinned maturin, fetched on demand** — soldr downloads a pinned
  maturin binary (or provisions the PyPI wheel in an isolated
  uv-managed env if the binary fetch misses). Reproducible across
  machines; nothing to add to your dependencies.
- **Toolchain pinning** — the build uses the rustup toolchain your
  `rust-toolchain.toml` declares (MSVC on Windows), even when a stray
  GNU cargo or mingw shadows it on `PATH`.
- **Managed cmake + ninja** — cmake-based `*-sys` crates
  (`libz-ng-sys`, `zstd-sys`, ...) configure with pinned tools from the
  soldr toolchain archive instead of whatever `cmake`/`make` your
  `PATH` happens to serve.
- **Compilation caching** — rustc invocations run under soldr's
  `RUSTC_WRAPPER`, so repeat builds hit the cache.

Local PEP 517 builds use an explicit fast `dev` profile by default:
`opt-level = 0`, 256 codegen units, line-table debug information, no LTO, and
incremental compilation. Explicit Maturin/Cargo settings and
`SOLDR_PEP517_PROFILE` remain authoritative per setting; release pipelines
should select their release profile explicitly.

Successful wheel and editable builds print a one-line cache and timing summary
to stderr. Set `SOLDR_PEP517_STATS=off` to silence it; `SOLDR_PEP517_STATS=full`
(and detected verbose `pip`/`uv` frontends) also prints the complete session
statistics payload.

Soldr also caches the last successful wheel for each project/build mode under
`<effective-soldr-root>/pep517/wheels/`. The backend asks the selected soldr
binary for this root, so official (`.soldr`), development (`.soldr-dev`), and
custom roots remain separate even when `SOLDR_CACHE_DIR` was initially unset.
Before packaging it scans source and staged-artifact
metadata (relative path, size, and modification time); an unchanged tree
hardlinks the cached wheel into pip's requested output directory and skips
wheel rebuilding/compression. Set `SOLDR_PEP517_WHEEL_CACHE=off` to opt out.

Projects whose packaging backend is not maturin can use soldr as a managed
wrapper by selecting a delegate in `pyproject.toml`:

```toml
[build-system]
requires = ["soldr", "setuptools>=64"]
build-backend = "soldr"

[tool.soldr.pep517]
delegate-backend = "setuptools.build_meta"
```

The delegate receives the normal PEP 517/660 hooks and return values while
soldr supplies its target, profile, linker, and cache environment. This is
intended for projects with custom staging such as native CLI scripts plus a
PyO3 extension. Maturin remains the default when no delegate is configured.
The same explicit profile settings work for delegates as for maturin; for
example, `pip install . --config-settings profile=release` selects the
release profile, while an ordinary install uses the fast local `dev` profile.

The backend also asks soldr to try its fastest supported linker locally. If
that linker fails with a linker-availability error, soldr retries once with
the platform linker and remembers the successful fallback in its cache. An
equivalent later build uses the fallback immediately and prints a warning.
Set `SOLDR_PEP517_LINKER=none` to disable this policy. An explicit
`SOLDR_LINKER=fast` is treated as a deliberate choice: it reports the failure
without silently downgrading to the system linker.
Note: soldr's own wheel is built with plain `build-backend = "maturin"`,
not with itself — using soldr as its own build backend created a
bootstrap cycle where a broken installed soldr made the fix in this
repo uninstallable (`pip install .` pulled the *published* soldr wheel
and shelled out to the system `soldr` binary).

## How it works

soldr is a **chameleon binary**: one executable that picks its role from `argv[1]` on every invocation. Three roles:

```mermaid
flowchart TD
    A["soldr &lt;argv[1]&gt; ..."] --> B{"classify argv[1]"}
    B -->|"path to rustc"| C["<b>Cache mode</b><br/>invoked as RUSTC_WRAPPER by cargo.<br/>hash inputs, query cache,<br/>forward to real rustc on miss."]
    B -->|"built-in verb<br/>cargo · rustup · status · cache · ..."| D["<b>Dispatch mode</b><br/>run internal handler,<br/>or exec a toolchain binary<br/>resolved via rustup."]
    B -->|"anything else<br/>maturin · cargo-nextest · cbindgen · ..."| E["<b>Tool-fetch mode</b><br/>resolve via known_tools,<br/>download from GitHub Releases,<br/>exec the fetched binary."]
```

When you run `soldr cargo build`, the two other modes both come into play. soldr acts as the dispatch-mode front door, then cargo re-invokes soldr once per crate as the RUSTC_WRAPPER — that second soldr is the cache-mode instance that talks to the managed zccache daemon:

```mermaid
sequenceDiagram
    autonumber
    participant U as User
    participant S1 as soldr (front door)
    participant C as cargo
    participant S2 as soldr (RUSTC_WRAPPER)
    participant Z as zccache daemon
    participant R as real rustc

    U->>S1: soldr cargo build --release
    S1->>S1: dispatch mode: cargo verb
    S1->>Z: start managed zccache if needed
    S1->>C: exec cargo (RUSTC_WRAPPER=soldr)
    loop per crate
        C->>S2: soldr [path-to-rustc] [args]
        S2->>Z: query cache (hash of inputs)
        alt cache hit
            Z-->>S2: cached artifact
            S2-->>C: emit artifact, exit 0
        else cache miss
            Z->>R: forward to rustc
            R-->>Z: fresh artifact
            Z->>Z: store keyed by input hash
            Z-->>S2: artifact
            S2-->>C: emit artifact, exit 0
        end
    end
    C-->>S1: build complete
    S1-->>U: exit
```

Tool fetches are much simpler — no cargo, no rustc, no wrapper handshake:

```text
soldr maturin build --release
  +-- maturin cached?  --> run instantly
  +-- not cached?      --> download pre-built binary (2s) --> run
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
- **One cache boundary**: official soldr keeps its tools and cache state under `~/.soldr/`; development builds use `~/.soldr-dev/`. Use `SOLDR_CACHE_DIR` to select an explicit root.
- **Bounded while idle**: the long-lived daemon owns only its selected root,
  checks pressure every five minutes, expires old state daily, and installs no
  OS scheduler. Embedded zccache defaults to 5% of the filesystem clamped to
  40–200 GiB, becomes aggressive for entries older than four days near full,
  and expires artifacts after 30 days. Production, development (`.soldr-dev`),
  custom, and standalone `.zccache` roots never sweep one another.
- **Disposable-worktree friendly on Windows**: for build orchestration, soldr can relocate itself under `~/.soldr/runtime/soldr-self/` so `RUSTC_WRAPPER` does not keep using a worktree-local `soldr.exe`; stale runtime copies are cleaned up periodically.
- **Pre-built first**: Download a pre-built binary before compiling from source. Fall back gracefully.
- **Cargo-compatible**: soldr preserves normal cargo arguments instead of forcing a separate workflow.
- **Cross-platform**: Linux, macOS, Windows (x86_64 + aarch64).
- **MSVC by default on Windows**: Always targets `x86_64-pc-windows-msvc` (or `aarch64-pc-windows-msvc`) unless the active project explicitly selects another target in `.cargo/config.toml`, `.cargo/config`, or `rust-toolchain.toml`. MSVC links against `vcruntime140.dll` which ships with every modern Windows install. The GNU target requires shipping `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` with every binary, which is extra baggage for most projects; when a project explicitly needs `x86_64-pc-windows-gnu`, `soldr prepare --target x86_64-pc-windows-gnu` provisions managed MinGW-w64 GCC on Windows x64. This matches the Rust ecosystem default: rustup, cargo-binstall, and nearly all published release binaries target MSVC. crgx gets this wrong by baking the target at compile time, causing it to look for GNU binaries when compiled under MSYS2.

## Architecture

**Monocrate.** A single Rust crate `soldr-cli` under `crates/soldr-cli/`, with four module trees inside it. The earlier four-crate workspace (`soldr-core` / `soldr-fetch` / `soldr-cache` / `soldr-cli`) was collapsed in 2026-05; the regression guard `crates/soldr-cli/tests/monocrate_guard.rs` fails the build if anyone reintroduces a second crate.

```
soldr/
|-- crates/
|   `-- soldr-cli/
|       |-- src/
|       |   |-- core/            # Shared types, config, cache paths (formerly soldr-core)
|       |   |-- fetch/           # Binary resolution + download (formerly soldr-fetch)
|       |   |-- cache_lib/       # RUSTC_WRAPPER + daemon IPC (formerly soldr-cache)
|       |   `-- main.rs + cli/   # Mode detection, dispatch, cargo front door
|       `-- tests/
|-- src/soldr/                    # Python package (maturin bin bindings)
`-- tests/
```

| Module | Role |
|---|---|
| `src/core/` | Shared types, config (`~/.soldr/config.toml`), target-triple resolution (MSVC default on Windows), cache paths, error types. No I/O beyond config files. |
| `src/fetch/` | Binary resolution. `known_tools` registry, `trust` (SHA-256 pins + `SOLDR_TRUST_MODE` enforcement), rustup auto-bootstrap, resolution chain (local cache → repo lookup → GitHub Releases → extract). |
| `src/cache_lib/` | `RUSTC_WRAPPER` mode: hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction, `soldr save` / `soldr load` archive transport, auto-GC. |
| `src/main.rs` + cli/ | Mode detection (chameleon dispatch), clap for built-ins, exec for tool fetch, cargo front door (`soldr cargo ...`). |

A thin `src/lib.rs` re-exports `pub mod core; pub mod fetch; pub mod cache_lib;` so the library integration tests can keep `use soldr_cli::core::*`-style imports. This is not a supported public Rust library API.

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
