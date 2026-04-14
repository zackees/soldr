# soldr Implementation Guide

This document defines the implementation direction for soldr.

For user-facing command behavior, see [docs/API.md](docs/API.md).

---

## What soldr is

One binary. Two visible jobs. One internal job.

1. Build front door
   `soldr cargo ...` is the primary user experience for Rust builds.
2. Tool fetcher
   `soldr <tool> ...` fetches and runs Rust CLI tools.
3. Internal wrapper
   soldr participates in builds by sitting in the `RUSTC_WRAPPER` slot after `soldr cargo ...` wires it up.

The important product rule is that users should think in terms of `soldr cargo ...`, not in terms of manually exporting `RUSTC_WRAPPER`.

## What soldr is not

- Not a separate build language
- Not a project scaffolder
- Not a Rust toolchain manager

soldr can delegate to Cargo, but it should not try to replace Cargo's flags, profiles, or dependency model.

---

## Core Principles

### 1. Front-door UX first

The normal build path is:

```bash
soldr cargo build
soldr cargo test
soldr cargo check
```

If the user has to understand `RUSTC_WRAPPER` just to get value from soldr, the product shape is wrong.

### 2. Cargo compatibility

The front door must preserve normal Cargo arguments. soldr should delegate to real Cargo, not reimplement Cargo semantics.

### 3. Wrapper mode is an implementation detail

Wrapper mode still matters, but it exists to support the front door. It is not the primary mental model.

### 4. Pre-built tools first

When users run `soldr <tool> ...`, prefer pre-built binaries before any source build path.

### 5. MSVC by default on Windows

On Windows, soldr should prefer MSVC targets unless the project explicitly requires GNU.

### 6. Bootstrapper mindset

soldr should prove it can build:

- itself
- other Rust software

That bootstrap story is a first-class requirement, not a side effect.

---

## Command Model

### Primary commands

```text
soldr cargo <cargo-args...>
soldr <tool>[@version] [tool-args...]
soldr status
soldr clean
soldr config
soldr cache
soldr version
```

### Internal execution model

For `soldr cargo ...`:

1. Resolve real `cargo` via `rustup`
2. Resolve matching real `rustc` via `rustup`
3. Set `RUSTC_WRAPPER` to the current soldr binary
4. Start managed zccache and pass its binary/session state through the environment
5. Delegate to Cargo with unchanged user flags

For wrapper mode:

1. Detect `rustc` invocation shape
2. Resolve the real `rustc`
3. Delegate cache-enabled builds into the managed zccache binary
4. Fall through to the real compiler when caching is disabled or unavailable

---

## Architecture

```text
crates/
|-- soldr-core
|-- soldr-fetch
|-- soldr-cache
`-- soldr-cli
```

### soldr-core

Owns:

- configuration
- target detection
- cache paths
- shared error types

### soldr-fetch

Owns:

- tool resolution
- archive download and extraction
- tool cache management

### soldr-cache

Owns:

- wrapper behavior around `rustc`
- cache keying
- artifact storage
- daemon and IPC work

### soldr-cli

Owns:

- mode detection
- command dispatch
- Cargo delegation
- fetched-tool process execution

---

## Implementation Phases

### Phase 1: Cargo Front Door

Done when:

- `soldr cargo build` works
- `soldr cargo test` works
- wrapper mode is wired automatically
- users no longer need manual `RUSTC_WRAPPER` setup for the common case

### Phase 2: Tool Fetching

Done when:

- `soldr maturin build`
- `soldr cargo-dylint check`
- `soldr rustfmt ...`

all resolve quickly from cache or pre-built binaries.

### Phase 3: Build Cache

Done when:

- `soldr cargo ...` enables managed zccache by default
- `soldr --no-cache cargo ...` cleanly bypasses the cache path
- wrapper mode routes cache-enabled builds into managed zccache instead of pure pass-through
- cache commands report and manage real zccache state

### Phase 4: Bootstrap Validation

Done when:

- soldr can build itself per target
- soldr can build a pinned third-party Rust project per target
- CI exposes one workflow per badge target

---

## CI Expectations

The repository should verify two things independently:

1. soldr builds on each supported target
2. soldr can bootstrap and build another Rust project on each supported target

Badge visibility matters, so these should be separate workflow entry points rather than a single hidden matrix.

Reusable workflow templates are fine, but the public workflows should remain one file per badge target.

---

## Design Guardrails

- Do not regress to a `RUSTC_WRAPPER`-first UX in docs or examples.
- Do not proxy Cargo by reimplementing Cargo flags.
- Do not require users to learn internal wrapper mechanics for the happy path.
- Keep the wrapper contract compatible with normal Cargo execution.

---

## References

- [README.md](README.md)
- [docs/API.md](docs/API.md)
