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
- Not a Rust toolchain manager *(under active reconsideration — see [docs/RUSTUP_REPLACEMENT_ANALYSIS.md](docs/RUSTUP_REPLACEMENT_ANALYSIS.md) and issue #235; the current recommendation is a hybrid native dist fetcher with rustup as a fallback, but until that work lands the position remains as stated)*

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

1. Resolve Cargo through Soldr's front door while retaining the caller's host
   toolchain context for host-owned binaries and the managed context for
   Soldr-owned binaries
2. Resolve rustc from the same selected toolchain context
3. Set `RUSTC_WRAPPER` to a compiler-named Soldr shim
4. Auto-start `soldr-daemon`, which owns the in-process zccache service, and
   pass only Soldr/session correlation state through the Cargo environment
5. Delegate to Cargo with unchanged user flags

For wrapper mode:

1. Detect the compiler-wrapper invocation shape
2. Resolve the real compiler from the selected toolchain context
3. Send cache-enabled compiler work over Soldr IPC to the zccache service
   embedded in `soldr-daemon`
4. Run the compiler directly only when caching is disabled or the Soldr daemon
   is unavailable and fallback policy permits it

### One daemon: embedded zccache (soldr#1467)

soldr runs exactly one long-lived process: soldr-daemon. The zccache build
cache is hosted *inside* it as an embedded service — wrapper invocations
ferry each compile to the daemon over the `Request::Compile` IPC verb. No
standalone `zccache-daemon` or `zccache-download-daemon` process is ever
spawned, and nothing in soldr may reach the upstream lazy-spawn entry
points (enforced by `tests/no_standalone_spawn_lint.rs`).

Compiler shims are named `rustc`, `clippy-driver`, or `zccache-soldr`, but a
long-lived daemon process must never inherit one of those executable
identities. The Cargo front door passes a canonical `soldr-daemon` multicall
alias to compiler children; a standalone compiler shim lazily materializes the
same alias before recovery. Lifecycle refuses a compiler-named fallback rather
than publishing a PID that its own recycled-PID safety check cannot trust.

Daemon startup has two distinct locks: the short-lived `.spawn.lock` suppresses
wrapper herds, while `root-owner.lock` is held by the daemon for its full
lifetime and shared with explicit orphan-root maintenance. On Unix, the child
binds the socket before publishing its PID/version. Shutdown removes the PID
and socket only while they are still owned by that process (the socket is
fenced by device/inode identity). A retiring or idle-timed-out daemon therefore
cannot unlink a successor's endpoint.

The in-process `soldr zccache <args>` entrypoint
(`crates/soldr-cli/src/zccache_entry.rs`) passes through only the daemon-free
subcommands — `rust-plan`, `session-end`, `stop`, `cache-root` (plus
`--help` / `--version`) — and hard-errors everything else, pointing at the
embedded equivalents (`soldr status`, `soldr cache`, `soldr cargo <verb>`).
As defense-in-depth it exports `ZCCACHE_NO_SPAWN=1` (upstream guard
zackees/zccache#982) so even an allowlisted subcommand can never spawn.
Embedded parity for the refused subcommands is tracked in
zackees/zccache#905.

The daemon is also the primary cache-retention owner. One daemon maps to one
exact `SoldrPaths` root and persists its five-minute pressure / 24-hour age
schedule beneath that root. Embedded zccache runs the same bounded retention
engine as standalone zccache but receives the soldr-owned child root and never
the standalone `~/.zccache` default. The host coordinates history, PEP517,
cook, trash, target, and stale-generation cleanup around active build leases.
Builds hold a shared root-maintenance lock through sanitized history
publication; a pass holds the exclusive side from its first decision through
its last deletion. Daemon startup and explicit orphan-root maintenance also
share a version-blind root-owner lock. Shutdown waits for a pass that already
started before publishing the root as unowned.
Default daemon lifetime is unbounded so age retention continues without new
CLI invocations; a nonzero explicit idle timeout opts back into auto-exit. No
operating-system scheduler is installed.

`soldr doctor` reports leftovers from pre-embedded installs or direct
zccache CLI use: running `zccache-daemon*` processes and stale per-launch
copies under `<zccache-root>/*/runtime-binaries/`, with a cleanup hint.

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

Rustfmt resolution is cached, but recursive formatting itself is not skipped:
Cargo passes crate roots while rustfmt discovers child modules dynamically.
Only an invocation that explicitly sets `skip_children=true` may use zccache's
content-marker shortcut.

### Phase 3: Build Cache

Done when:

- `soldr cargo ...` enables the zccache service embedded in `soldr-daemon` by default
- `soldr --no-cache cargo ...` cleanly bypasses the cache path
- wrapper mode routes cache-enabled builds over Soldr IPC into the embedded
  service instead of spawning an external zccache process
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
