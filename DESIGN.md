# DESIGN.md — soldr Implementation Guide

This document is the guiding light for implementing soldr. Every PR, every code review, every architecture decision should trace back to something in here. For the full CLI specification, see [docs/API.md](docs/API.md).

---

## What soldr is

One binary. Two jobs:

1. **Fetch any Rust tool instantly** — download pre-built binaries from GitHub Releases, never compile from source unless forced. Like crgx/npx but with correct Windows target detection.

2. **Cache every rustc invocation transparently** — sit in the `RUSTC_WRAPPER` slot, hash inputs, return cached artifacts on hit. Like sccache/zccache but with auto-start daemon and unified cache.

These are detected automatically. No mode flags.

## What soldr is NOT

- **Not a cargo wrapper.** soldr never wraps `cargo build`. It wraps `rustc`. The user runs cargo directly with all the flags they know. soldr is invisible. (See: "Why no `soldr build`" below.)

- **Not a task runner.** No `soldr lint`, `soldr test`, `soldr build`. Those are scope creep that creates flag-forwarding nightmares. cargo has dozens of flags; reimplementing or proxying them is a trap.

- **Not a project scaffolder.** No `soldr init`. Templates baked into binaries rot when CI syntax changes. Use `cargo init` or `cargo generate` with a template repo.

- **Not a version manager.** soldr doesn't manage Rust toolchains. That's rustup's job. soldr fetches _tool binaries_ (maturin, cargo-dylint, etc.), not the Rust compiler itself.

---

## Core Principles

### 1. Invisible by default

The compilation cache should require zero thought. `RUSTC_WRAPPER` defaults to `zccache` if not explicitly set — no configuration needed. The daemon auto-starts on first invocation, auto-evicts old entries, and never prompts.

### 2. Pre-built first, always

When fetching a tool, try every pre-built source before touching `cargo install`:

1. Local cache (`~/.soldr/bin/`)
2. Binstall metadata (`[package.metadata.binstall]` in Cargo.toml on crates.io)
3. GitHub Releases (standard naming patterns)
4. QuickInstall registry
5. `cargo install` (last resort — slow, requires full toolchain)

### 3. MSVC on Windows, always

On Windows, assume `x86_64-pc-windows-msvc` (or `aarch64-pc-windows-msvc`). Period. Unless `rust-toolchain.toml` explicitly says GNU. MSVC links against `vcruntime140.dll` (always present). GNU requires shipping `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` — extra baggage for no benefit.

crgx gets this wrong by baking the target at compile time. soldr resolves it at runtime.

### 4. Bootstrapping tool — version doesn't matter (mostly)

soldr is like uv: users install it once and it just works. They don't care about the version. `pip install soldr` gets latest. The bash one-liner gets latest. But CI pipelines _should_ pin: `pip install soldr==0.1.0`. Support `--version` in every install path.

### 5. Frozen built-in command list

The built-in commands are: `status`, `clean`, `config`, `cache`, `version`, `help`. This list is **frozen**. Every other first argument is treated as a tool name to fetch-and-run. This prevents namespace collisions with real tools. Do not add `build`, `test`, `lint`, `fmt`, `check`, `doc`, `bench`, or `publish` as built-in commands — ever.

---

## Why no `soldr build`

This is the most important design decision. An expert review ([conversation record](https://github.com/zackees/soldr/issues/1)) identified this as the biggest risk:

> `soldr build` wrapping `cargo build` creates a flag forwarding nightmare. Cargo has `--release`, `--target`, `--features`, `--no-default-features`, `--manifest-path`, `--jobs`, `--profile`, `--timings`, `-p`, `--workspace`, `--exclude`, `--bin`, `--lib`, `--example`, `--test`, `--bench`, and dozens more. You either forward everything (maintenance hell) or support a subset (users hit walls).

sccache solved this correctly: it's a `RUSTC_WRAPPER`, not a cargo wrapper. The user runs `cargo build` with all the flags they already know. The cache is invisible.

**The rule:** soldr wraps `rustc`, not `cargo`. Cargo owns the build orchestration. soldr owns the per-unit caching.

---

## Mode Detection

When soldr's `main()` runs:

```
if argv[1] looks like a path to rustc (ends with "/rustc" or "\rustc" or "rustc.exe")
    or argv[1] == "rustc":
    → RUSTC_WRAPPER mode: act as compilation cache

else if argv[1] is in BUILT_IN_COMMANDS:
    → run the built-in command

else:
    → tool fetch mode: parse argv[1] as "crate[@version]", fetch binary, exec with remaining args
```

This is clean and unambiguous. No flags, no env var sniffing for mode selection.

---

## Architecture

```
crates/
├── soldr-core/       # Config, paths, cache layout, target resolution, errors
├── soldr-fetch/      # Binary resolution chain: cache → binstall → github → quickinstall → cargo install
├── soldr-cache/      # RUSTC_WRAPPER logic: hash inputs, check cache, store artifacts, daemon IPC
└── soldr-cli/        # main(), mode detection, clap for built-ins, exec for tool fetch
```

### soldr-core

Owns:
- `~/.soldr/` directory layout and creation
- `config.toml` parsing and defaults
- Target triple resolution (runtime, not compile-time; MSVC default on Windows)
- Version types (`Latest`, `Exact(semver)`, `Pinned`)
- Shared error types

Does not own: any I/O beyond config file reading.

### soldr-fetch

Owns:
- Crate metadata fetching from crates.io (reads `[package.metadata.binstall]`)
- GitHub Release asset discovery (tries standard naming patterns)
- QuickInstall registry queries
- Archive download, extraction, verification
- Tool cache management (`~/.soldr/bin/`)
- Fallback to `cargo install` when no pre-built binary found

Key types:
```rust
pub struct FetchRequest {
    pub crate_name: String,
    pub version: VersionSpec,     // Latest | Exact("1.2.3") | LatestForce
    pub target: TargetTriple,
}

pub enum FetchResult {
    Cached(PathBuf),              // Already in ~/.soldr/bin/
    Downloaded(PathBuf),          // Fetched from remote, now cached
    BuiltFromSource(PathBuf),     // cargo install fallback
    NotFound(FetchError),
}
```

### soldr-cache

Owns:
- RUSTC_WRAPPER entry point (called by cargo with rustc args)
- Input hashing (source files, compiler flags, dependency fingerprints, rustc version)
- Artifact storage and retrieval (`~/.soldr/cache/`)
- Cache daemon (IPC server for concurrent builds)
- Auto-start daemon on first invocation
- LRU eviction when cache exceeds `max_size`
- Cache statistics tracking

Key flow:
```
cargo calls: soldr rustc --crate-name foo --edition 2021 -C opt-level=3 ...
  1. Hash all inputs → cache key
  2. Ask daemon: have this key?
     YES → write cached artifacts to expected output paths, exit 0
     NO  → invoke real rustc, capture outputs, store in cache, exit with rustc's code
```

### soldr-cli

Owns:
- `main()` with mode detection (see above)
- Built-in command dispatch via clap
- Tool fetch mode: parse `crate[@version]`, call soldr-fetch, exec the binary
- Process lifecycle: exec/spawn the fetched tool, forward exit code

Does not own: any business logic. This is a thin dispatch layer.

---

## Implementation Phases

### Phase 1: Tool Fetcher (MVP)

The fastest path to a useful tool. No compilation caching yet.

1. **soldr-core**: config, paths, target resolution
2. **soldr-fetch**: local cache check → GitHub Releases download → exec
3. **soldr-cli**: mode detection, `soldr <tool> [args]`, `soldr version`, `soldr help`
4. **Install scripts**: `install.sh` (curl one-liner), `pyproject.toml` (pip)
5. **CI**: build matrix for 6 platforms, publish to PyPI + GitHub Releases

**Done when:** `soldr maturin build --release` works on all 6 platforms, downloading maturin from GitHub Releases in <3 seconds.

### Phase 2: Compilation Cache

Port the zccache compilation cache into soldr-cache.

1. **soldr-cache**: RUSTC_WRAPPER entry point, input hashing, artifact storage
2. **Daemon**: auto-start, IPC (Unix domain socket / Windows named pipe)
3. **soldr-cli**: `soldr status`, `soldr clean`, `soldr cache list`

**Done when:** `RUSTC_WRAPPER=soldr cargo build` on a warm cache is 2x+ faster than without, across all 6 platforms.

### Phase 3: Full Resolution Chain

Flesh out the fetch resolution to match cargo-binstall's maturity.

1. **Binstall metadata**: read `[package.metadata.binstall]` from crates.io
2. **QuickInstall**: query the quickinstall registry as fallback
3. **cargo install fallback**: build from source when no binary found
4. **soldr config**: expose all config knobs
5. **soldr cache export/import**: share caches across CI runners

### Phase 4: Distribution

1. **npm package**: thin wrapper that downloads the platform binary (like esbuild)
2. **cargo binstall support**: `[package.metadata.binstall]` in soldr-cli's Cargo.toml
3. **soldr.dev**: landing page with install instructions
4. **GitHub Action**: (probably just `curl | sh` + `RUSTC_WRAPPER=soldr`, no custom action needed)

---

## Target Platforms

| Target | Runner | Priority |
|---|---|---|
| `x86_64-unknown-linux-gnu` | ubuntu-24.04 | P0 |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm | P0 |
| `x86_64-apple-darwin` | macos-14 | P0 |
| `aarch64-apple-darwin` | macos-15 | P0 |
| `x86_64-pc-windows-msvc` | windows-2025 | P0 |
| `aarch64-pc-windows-msvc` | windows-2025 | P1 |

---

## What we learned building zccache

These hard-won lessons should be carried into soldr:

1. **CARGO_MAKEFLAGS jobserver FDs don't survive deep process chains.** When running through `uv → maturin → cargo → wrapper`, the daemon's jobserver FDs (8,9) become invalid. Workaround: don't set CARGO_MAKEFLAGS, or detect when FDs are dead.

2. **GitHub Releases need standard naming.** The binstall ecosystem expects `{name}-{version}-{target}.tar.gz`. Don't invent a custom scheme.

3. **`pip install` is the universal fallback.** Every CI runner has Python. When the binary download fails, `pip install` always works. Keep this fallback.

4. **The daemon must auto-start.** Requiring `soldr start` before builds is friction. The first RUSTC_WRAPPER call should start the daemon transparently.

5. **Cache save on PR builds wastes budget.** GHA cache is limited. PR builds should restore but not save. Push-to-main builds save.

6. **`cargo publish` must happen in dependency order.** The workspace has internal dependencies. Publish leaf crates first, wait for crates.io propagation, then publish dependents.

---

## Files to reference

- [docs/API.md](docs/API.md) — Full CLI specification, environment variables, cache layout
- [README.md](README.md) — User-facing description and motivation
- This file — Implementation guide and architecture decisions
