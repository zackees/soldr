# setup-soldr

Public GitHub Action for installing one released `soldr` binary, provisioning the resolved Rust toolchain with `rustup`, and restoring a cacheable runner-local root for Soldr, Cargo, and rustup state.

This repository is intended to be generated from `zackees/soldr`. The source-of-truth contract and release process still live in `soldr` issue #137 and `docs/SETUP_SOLDR_PUBLIC_ACTION.md`.

## Usage

### Linux

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-linux:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v0
        with:
          cache: true
      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

### macOS

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-macos:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v0
        with:
          cache: true
      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

### Windows

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v0
        with:
          cache: true
      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

## Inputs

| Input | Meaning |
|---|---|
| `version` | Soldr release tag or version to install. Empty means latest release. |
| `cache` | Restore and save the action-managed cache/state root. |
| `cache-dir` | Override the runner-local cache/state root. |
| `cache-key-suffix` | Optional escape hatch appended to the cache key. |
| `toolchain` | Explicit Rust toolchain channel override. |
| `toolchain-file` | Alternate toolchain file path when `toolchain` is empty. |
| `trust-mode` | Optional `SOLDR_TRUST_MODE` value. |
| `build-cache` | Restore and save the Soldr-owned zccache compilation artifact cache across runs. Default `true`; set to `false` to opt out. |
| `target-cache` | Restore and save the zccache-owned Rust artifact plan cache for fast CI rebuilds. |
| `target-cache-mode` | Target cache mode. Default `thin` asks soldr to generate a bounded dependency-artifact plan for zccache; `full` asks zccache to cache the whole `target-dir`; `off` disables target artifact caching. The old `hot` value is accepted as a deprecated alias for `thin`. |
| `target-dir` | Cargo target directory used in target-cache key shaping. |
| `tool-shims` | Optional PATH shim mode. Set to `cargo` to make later `cargo ...` steps run through `soldr cargo ...`; default `false`. |

## Outputs

| Output | Meaning |
|---|---|
| `soldr-path` | Installed Soldr binary path added to `PATH`. |
| `soldr-version` | Installed Soldr version reported by `soldr version --json`. |
| `cache-dir` | Action-managed runner-local cache/state root. |
| `cache-hit` | Whether the action restored an exact cache hit. |
| `build-cache-hit` | Whether the Soldr-owned zccache compilation cache was restored. Empty only when `build-cache` is disabled. |
| `target-cache-hit` | Whether the Rust artifact plan cache was restored. |
| `target-cache-mode` | Effective target cache mode. |
| `toolchain` | Exact Rust toolchain channel configured for the action. |
| `tool-shims-dir` | Directory containing generated tool shims when enabled. |

## Notes

- The action installs exactly one released `soldr` binary for the active runner target.
- The normal path provisions Rust with `rustup`, bootstrapping `rustup` when it is absent.
- The action rehydrates `SOLDR_CACHE_DIR`, `CARGO_HOME`, and `RUSTUP_HOME` under the selected cache root.
- The action restores the Soldr-owned zccache cache root by default so child branches can reuse parent-branch build state.
- The default target cache mode is `thin`, which avoids action-owned `target/` snapshots by having soldr pass a bounded Rust artifact plan to zccache. Use `target-cache-mode: full` only for tightly scoped jobs where the whole target directory is known to stay bounded.
- The action exports `ZCCACHE_CACHE_DIR` to keep managed zccache artifact storage under `SOLDR_CACHE_DIR`.
- `tool-shims: cargo` prepends a Cargo shim for existing workflows that cannot rewrite every `cargo ...` command to `soldr cargo ...`.
- A restored target directory is a Cargo fast path, not a guarantee: build scripts without precise `cargo:rerun-if-*` inputs can still be dirty on fresh checkouts because source mtimes differ.

## Development

Regenerate this repository bundle from the source repository with the exporter in `zackees/soldr`.
