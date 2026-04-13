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
./test                                 # pytest -n auto

# Maturin (Python+Rust packaging)
uv run maturin develop                 # Build & install in venv
uv run maturin build --release         # Build wheel
```

## Architecture

Four-crate Rust workspace under `crates/`:

- **soldr-core** — Shared types, config (`~/.soldr/config.toml`), target triple resolution (MSVC default on Windows at runtime), error types. No I/O beyond config files.
- **soldr-fetch** — Binary resolution chain: local cache → binstall metadata → GitHub Releases → QuickInstall → `cargo install` (last resort). Manages `~/.soldr/bin/`.
- **soldr-cache** — `RUSTC_WRAPPER` logic: hash inputs (blake3), check `~/.soldr/cache/`, daemon IPC (Unix socket / Windows named pipe), LRU eviction.
- **soldr-cli** — Thin dispatch layer. `main()` with mode detection, clap for built-ins, exec for tool fetch. No business logic here.

Dependency flow: `soldr-cli → {soldr-core, soldr-fetch, soldr-cache}`, both fetch and cache depend on `soldr-core`.

Python package (`src/soldr/`) wraps the CLI binary via Maturin as `soldr._native`.

## Key Design Rules

- **Frozen built-in commands**: `status`, `clean`, `config`, `cache`, `version`, `help`. Never add `build`, `test`, `lint`, `fmt`, `check`, `doc`, `bench`, `publish` — prevents namespace collision with tool names.
- **MSVC on Windows always**: Default to `x86_64-pc-windows-msvc` (or aarch64). Only use GNU if `rust-toolchain.toml` explicitly says so. Target resolved at runtime, not compile-time.
- **Pre-built first**: Try every binary source before `cargo install`. Resolution order matters.
- **RUSTC_WRAPPER defaults to zccache**: If `RUSTC_WRAPPER` is not set, soldr defaults to using `zccache` as the wrapper.
- **Daemon auto-starts**: First `RUSTC_WRAPPER` call starts the cache daemon transparently. No manual `soldr start`.
- **Version independence**: Users install once and forget. CI should pin: `pip install soldr==X.Y.Z`.

## Toolchain

- Rust 1.85 (rust-toolchain.toml), edition 2021, MSRV 1.75
- Python >=3.10 (for PyPI distribution via Maturin)
- uv for Python dependency management
- Workspace dependencies shared in root `Cargo.toml`

## Implementation Status

Currently in **Phase 1 (Tool Fetcher MVP)**. All crate lib.rs files are stubs; CLI skeleton exists with clap commands that print "(not yet implemented)". See DESIGN.md for the four-phase roadmap.

## Reference Docs

- `DESIGN.md` — Authoritative implementation guide, architecture decisions, phase roadmap
- `docs/API.md` — Full CLI specification, environment variables, cache layout
- `README.md` — User-facing motivation and prior art comparison

## Known Issues

- CLI currently has `Build` and `Run` subcommands that contradict DESIGN.md (which says no `soldr build` and tool fetch should be the bare `soldr <tool>` form, not `soldr run <tool>`)
- LICENSE file says MIT but Cargo.toml specifies BSD-3-Clause
