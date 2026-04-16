# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

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

Four-crate Rust workspace under `crates/`:

- **soldr-core** — Shared types, config (`~/.soldr/config.toml`), target triple resolution (MSVC default on Windows at runtime), error types. No I/O beyond config files.
- **soldr-fetch** — Binary resolution. Ships two sub-modules:
  - `known_tools` — registry of ecosystem tools with explicit GitHub `(owner, repo)`, cargo subcommand mapping, and optional monorepo tag prefix (e.g. `cargo-audit/v0.21.0`). Keeps dispatch off the crates.io round-trip and handles per-tool release quirks.
  - `trust` — SHA-256 computation + `SOLDR_TRUST_MODE` / `SOLDR_CHECKSUMS_FILE` enforcement. Every fetch emits a `trust: verified` or `trust: unverified` line and a pin mismatch is a hard error regardless of mode.
  - Resolution chain: local cache → registry-or-crates.io repo lookup → GitHub Releases asset download → extract.
- **soldr-cache** — `RUSTC_WRAPPER` logic: hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction.
- **soldr-cli** — Thin dispatch layer. `main()` with mode detection, clap for built-ins, exec for tool fetch. The cargo front door (`soldr cargo ...`) inspects the first positional arg; if it matches a `known_tools` `cargo_subcommand`, the corresponding `cargo-<sub>` binary is fetched and prepended to `PATH` before cargo runs.

Dependency flow: `soldr-cli → {soldr-core, soldr-fetch, soldr-cache}`, both fetch and cache depend on `soldr-core`.

Python package (`src/soldr/`) wraps the CLI binary via Maturin as `soldr._native`.

## Supported Tools

Two categories, surfaced as first-class subcommands or via the generic fetch path:

**Rustup toolchain passthroughs** (resolved via `rustup which`):
`rustc`, `rustfmt`, `clippy-driver`, `rustdoc`, `rust-gdb`, `rust-lldb`, `rust-analyzer`.

**Ecosystem fetches** (registered in `known_tools`, pulled from GitHub Releases):
- cargo subcommands invoked via `soldr cargo <sub>`: `nextest`, `deny`, `audit`, `llvm-cov`, `udeps`, `semver-checks`, `expand`, `watch`.
- top-level tools invoked directly via `soldr <tool>`: `cross`, `mdbook`, `cbindgen`, `wasm-pack`, `trunk`, `sccache`.

Anything not registered falls through the generic External subcommand, which resolves via crates.io → GitHub Releases.

## Key Design Rules

- **Frozen built-in commands**: `status`, `clean`, `config`, `cache`, `version`, `help` plus the toolchain passthroughs listed above. Never add `build`, `test`, `lint`, `fmt`, `check`, `doc`, `bench`, `publish` — prevents namespace collision with tool names.
- **MSVC on Windows always**: Default to `x86_64-pc-windows-msvc` (or aarch64). Only use GNU if `rust-toolchain.toml` explicitly says so. Target resolved at runtime, not compile-time.
- **Pre-built first**: Try every binary source before `cargo install`. Resolution order matters.
- **RUSTC_WRAPPER defaults to zccache**: If `RUSTC_WRAPPER` is not set, soldr defaults to using `zccache` as the wrapper.
- **Daemon auto-starts**: First `RUSTC_WRAPPER` call starts the cache daemon transparently. No manual `soldr start`.
- **Integrity is default**: every fetch records sha256. Pins are opt-in via `SOLDR_CHECKSUMS_FILE`; `SOLDR_TRUST_MODE=strict` refuses unpinned fetches.
- **Version independence**: Users install once and forget. CI should pin: `pip install soldr==X.Y.Z`.

## Toolchain

- Rust 1.94.1 (rust-toolchain.toml), edition 2021, MSRV 1.75
- Python >=3.10 (for PyPI distribution via Maturin)
- uv for Python dependency management
- Workspace dependencies shared in root `Cargo.toml`

## Reference Docs

- `DESIGN.md` — Authoritative implementation guide, architecture decisions, phase roadmap
- `docs/API.md` — Full CLI specification, environment variables, cache layout
- `docs/TRUST_BOUNDARIES.md` — Runtime fetch policy, what integrity is enforced, what remains follow-up
- `README.md` — User-facing motivation and prior art comparison
