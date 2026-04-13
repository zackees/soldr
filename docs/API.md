# soldr API Reference

## Overview

soldr is two things in one binary:

1. **A tool fetcher**: `soldr <tool> [args]` downloads and runs any crate binary instantly
2. **A compilation cache**: `RUSTC_WRAPPER=soldr` makes cargo cache every rustc invocation

These two roles are detected automatically based on how soldr is invoked.

---

## Invocation Modes

### Mode 1: Tool Fetcher (default)

```
soldr <tool>[@<version>] [tool-args...]
```

Fetch a pre-built binary for `<tool>` and execute it with `[tool-args...]`.

```bash
soldr maturin build --release          # fetch maturin, run it
soldr cargo-dylint                      # fetch cargo-dylint, run it
soldr rustfmt src/main.rs              # fetch rustfmt, run it
soldr maturin@1.7.0 build              # fetch specific version
```

**Resolution order:**
1. Local cache (`~/.soldr/bin/`) — instant if previously fetched
2. Binstall metadata from crate's `Cargo.toml` (`[package.metadata.binstall]`)
3. GitHub Releases (standard naming conventions)
4. QuickInstall registry
5. `cargo install` from source (last resort, requires Rust toolchain)

**Target resolution (Windows):**
- Always `x86_64-pc-windows-msvc` (or `aarch64-pc-windows-msvc`)
- Unless `rust-toolchain.toml` explicitly specifies a GNU target
- Never baked at compile time; always resolved at runtime

**Caching:**
- `soldr tool` — fetches latest, checks for updates every 24h
- `soldr tool@1.2.3` — exact version, cached forever
- `soldr tool@latest` — force re-check for latest version

### Mode 2: Compilation Cache (RUSTC_WRAPPER)

```bash
export RUSTC_WRAPPER=soldr
cargo build --release
```

When cargo sets `RUSTC_WRAPPER=soldr`, cargo invokes `soldr rustc <args>` for every compilation unit. soldr detects this (first arg is a path ending in `rustc` or equals `rustc`) and acts as a transparent compilation cache:

1. Hash the inputs (source files, flags, dependencies, rustc version)
2. Check `~/.soldr/cache/` for a matching artifact
3. Cache hit → return cached `.o` / `.rlib` / `.rmeta` (~1ms)
4. Cache miss → invoke real `rustc`, store output, return it

**This is invisible to the user.** No flag forwarding, no cargo wrapping. Cargo does its thing, soldr silently caches. Exactly how sccache works.

**Auto-start daemon:** The first RUSTC_WRAPPER invocation starts the cache daemon if it's not running. No manual `soldr start` needed.

### Mode Detection

When soldr is invoked, it determines its mode:

```
argv[1] ends with "rustc" or is "rustc"
  → Mode 2: Compilation cache (RUSTC_WRAPPER)

argv[1] is a built-in command (status, clean, config, cache, version, help)
  → Run built-in

argv[1] is anything else
  → Mode 1: Tool fetcher (fetch + run)
```

---

## Built-in Commands

### `soldr status`

Show cache statistics and daemon state.

```
$ soldr status
soldr 0.1.0

Daemon:        running (pid 12345)
Tool cache:    ~/.soldr/bin/ (14 tools, 89 MB)
Build cache:   ~/.soldr/cache/ (2,341 artifacts, 412 MB)

Recent tools:
  maturin 1.7.4      (cached 2h ago)
  cargo-dylint 3.1.0  (cached 5d ago)
  rustfmt 1.7.1       (cached 12d ago)

Build cache hit rate: 94% (last 24h)
```

### `soldr clean`

Clear caches.

```bash
soldr clean              # clear everything (tools + build cache)
soldr clean --tools      # clear only tool cache
soldr clean --cache      # clear only build cache
soldr clean --older 30d  # clear entries older than 30 days
```

### `soldr config`

View or set configuration.

```bash
soldr config                          # show current config
soldr config set cache-dir /tmp/soldr # override cache directory
soldr config set max-cache-size 2GB   # limit build cache size
```

**Config file:** `~/.soldr/config.toml`

```toml
[cache]
dir = "~/.soldr"
max_size = "2GB"
eviction = "lru"

[fetch]
# Registries to search for pre-built binaries
registries = ["github", "quickinstall"]
# Allow source compilation as fallback
allow_build = true

[daemon]
# Auto-start daemon on first RUSTC_WRAPPER invocation
auto_start = true
```

### `soldr cache`

Direct cache inspection (for debugging).

```bash
soldr cache list                   # list cached artifacts
soldr cache inspect <hash>         # show details for a cached artifact
soldr cache export <path>          # export cache for sharing/CI
soldr cache import <path>          # import cache from another machine
```

### `soldr version`

```
$ soldr version
soldr 0.1.0
```

### `soldr help`

```
$ soldr help
soldr — Instant tools. Instant builds.

Usage:
  soldr <tool>[@version] [args...]   Fetch and run a crate binary
  RUSTC_WRAPPER=soldr cargo build    Transparent compilation caching

Commands:
  status    Show cache stats and daemon state
  clean     Clear caches
  config    View or set configuration
  cache     Inspect and manage build cache
  version   Print version
  help      Show this help

Examples:
  soldr maturin build --release      Fetch maturin, run it
  soldr rustfmt src/main.rs          Fetch rustfmt, run it
  soldr maturin@1.7.0 build          Pin to a specific version
  soldr clean --older 30d            Evict stale cache entries

Cache directory: ~/.soldr/
Config file:     ~/.soldr/config.toml
```

---

## Install

```bash
# One-liner (Linux/macOS)
curl -fsSL https://soldr.dev/install.sh | sh

# One-liner (Windows PowerShell)
irm https://soldr.dev/install.ps1 | iex

# pip (all platforms)
pip install soldr

# npm (all platforms)
npm install -g soldr

# Cargo (from source)
cargo install soldr-cli

# cargo-binstall (pre-built binary)
cargo binstall soldr-cli
```

All install methods support `--version`:
```bash
curl -fsSL https://soldr.dev/install.sh | sh -s -- --version 0.1.0
pip install soldr==0.1.0
npm install -g soldr@0.1.0
```

Default: latest. CI pipelines should pin.

---

## Environment Variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTC_WRAPPER` | Set to `soldr` to enable compilation caching | `zccache` |
| `SOLDR_CACHE_DIR` | Override cache directory | `~/.soldr` |
| `SOLDR_MAX_CACHE_SIZE` | Maximum build cache size | `2GB` |
| `SOLDR_LOG` | Log level (`error`, `warn`, `info`, `debug`, `trace`) | `warn` |
| `SOLDR_NO_DAEMON` | Disable daemon, run cache in-process | `false` |
| `SOLDR_OFFLINE` | No network access, only use local cache | `false` |

---

## Cache Directory Layout

```
~/.soldr/
├── config.toml              # User configuration
├── bin/                     # Fetched tool binaries
│   ├── maturin-1.7.4/       # One dir per tool@version
│   │   └── maturin(.exe)
│   ├── cargo-dylint-3.1.0/
│   │   └── cargo-dylint(.exe)
│   └── ...
├── cache/                   # Compilation artifacts
│   ├── ab/cd/ef012345...    # Content-addressed by input hash
│   └── ...
├── daemon.pid               # Daemon PID file
└── daemon.sock              # Daemon IPC socket (Unix) / named pipe (Windows)
```

---

## GitHub Action

```yaml
- name: Setup soldr
  run: curl -fsSL https://soldr.dev/install.sh | sh

- name: Build with caching
  env:
    RUSTC_WRAPPER: soldr
  run: cargo build --release
```

No separate action needed. Just install + set the env var. Cargo does the rest, soldr caches invisibly.

---

## Comparison

| Feature | soldr | sccache | crgx | cargo-binstall |
|---|---|---|---|---|
| Compilation caching | Yes (RUSTC_WRAPPER) | Yes | No | No |
| Tool fetching | Yes (first-class) | No | Yes | Yes |
| Pre-built binary download | Yes | No | Yes | Yes |
| Daemon auto-start | Yes | Manual | N/A | N/A |
| MSVC default on Windows | Yes | N/A | No (compile-time) | Yes (tries both) |
| Single cache directory | Yes | Separate | Separate | Separate |
| pip/npm install | Yes | No | No | No |
