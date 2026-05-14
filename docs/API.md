# soldr API Reference

This file is the CLI reference for `soldr`.

For the product-level support contract about what counts as a supported external API, see [API_BOUNDARY.md](./API_BOUNDARY.md).

## Overview

soldr is a single front door for Rust tool execution and Rust builds.

It has three invocation modes:

1. `soldr cargo ...`
   Delegates to the real Cargo binary while wiring soldr-managed zccache into the build path.
2. `soldr <tool> [args...]`
   Fetches and runs a Rust CLI tool binary.
3. `soldr rustc ...`
   Low-level passthrough wrapper mode for explicit `RUSTC_WRAPPER=soldr` usage.

The primary user experience is `soldr cargo ...`.

## Machine-Facing Support Level

Current support policy:

- the supported external integration surface is invoking the `soldr` executable through documented commands and flags
- the internal Rust crates are not a supported public API
- wrapper mode and internal environment variables are operational mechanics, not a general-purpose API contract
- human-oriented command output is not the stable machine-facing protocol
- the first stable machine-facing protocol is the JSON mode on selected commands documented below

---

## Invocation Modes

### Mode 1: Cargo Front Door

```bash
soldr cargo build --release
soldr cargo test
soldr cargo run -- --help
soldr --no-cache cargo build
```

Behavior:

- Prefer direct `cargo` and `rustc` binaries from repo-local or explicit `CARGO_HOME/bin`
- Fall back to `rustup which <tool>` when no matching binary is present in that repo-contained toolchain location
- Fetch a pinned managed `zccache` release when caching is enabled
- Set `RUSTC_WRAPPER` to the current soldr binary
- Pass the managed `zccache` binary path into wrapper mode through the environment
- Start a per-build zccache session under Soldr's owned zccache cache root
- Delegate to Cargo with the exact flags the user passed

Current cache-control behavior:

- caching is enabled by default for `soldr cargo ...`
- `soldr --no-cache cargo ...` disables soldr's compilation-cache path for that invocation
- `soldr cargo --no-cache ...` is rejected; `--no-cache` is a top-level soldr flag only
- zccache integration currently targets Rust builds through the cargo front door
- managed zccache artifacts and daemon state live under Soldr's cache root through `ZCCACHE_CACHE_DIR`
- toolchain binaries (`rustc`, `rustfmt`, `clippy-driver`, etc.) are resolved directly from `RUSTUP_HOME` / `CARGO_HOME` / `PATH` before any `rustup` call; `rustup which` is only used as a fallback when the direct probe fails. The sole exception is when `RUSTUP_TOOLCHAIN` is explicitly set to a non-empty value — in that case soldr skips the direct probe and asks `rustup` for the matching toolchain binary so the pinned channel always wins

This is the normal build entry point.

### Mode 2: Tool Fetcher

```bash
soldr <tool>[@<version>] [tool-args...]
```

Examples:

```bash
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
soldr maturin@1.7.0 build
```

Resolution order:

1. Local cache in `~/.soldr/bin/`
2. crates.io repository lookup
3. GitHub Releases for that repository

Current implementation note:

- the broader binstall/QuickInstall/`cargo install` fallback chain is planned behavior, not the current shipped fetch path

### Mode 3: Internal Wrapper Mode

Wrapper mode is entered when Cargo invokes soldr as the configured `RUSTC_WRAPPER`.

Typical shape:

```text
soldr /path/to/rustc --crate-name foo ...
```

In this mode, soldr should act as the transparent build-assistance layer around `rustc`.

Current implementation status:

- Wrapper mode still transparently resolves the real `rustc`, preferring direct binaries before `rustup`
- The normal cache-enabled build path now runs through soldr wrapper mode and delegates into managed `zccache`
- If caching is disabled, wrapper mode falls through to real `rustc` without zccache involvement

---

## Mode Detection

When soldr starts, it decides its mode in this order:

1. If `argv[1]` looks like `rustc` or a path to `rustc`, enter wrapper mode.
2. Otherwise, parse CLI commands with Clap.
3. `cargo` is a first-class built-in subcommand.
4. Any unknown first argument is treated as a tool name to fetch and run.

---

## Built-in Commands

### `soldr cargo`

Run Cargo through soldr's front door.

```bash
soldr cargo build --release
soldr cargo test --workspace
soldr cargo check -p soldr-cli
soldr --no-cache cargo test
```

### `soldr status`

Show cache and target information.

Stable machine-facing mode:

```bash
soldr status --json
```

### `soldr clean`

Clear the managed local zccache artifact cache and remove soldr's zccache session state directory.

### `soldr config`

Show or set configuration.

### `soldr cache`

Inspect managed zccache status.

Stable machine-facing mode:

```bash
soldr cache --json
```

### `soldr version`

Print soldr version.

Stable machine-facing mode:

```bash
soldr version --json
```

### `soldr gc`

Review reclaimable Cargo `target/` directories tracked in
`~/.soldr/state.redb`. Implemented by issue #234 and made safe-by-default
by issue #289. The wrapper-mode hot path upserts each invocation's
resolved workspace `target/` path with the current timestamp; `soldr gc`
walks the registry, drops missing rows, applies safety guards, and
prints an info summary without prompting or deleting anything.

```bash
soldr gc                                      # info summary only
soldr gc --json                               # machine-readable summary
soldr gc --older-than 30d --larger-than 1GB   # summary with tunable filters
soldr gc purge                                # interactive deletion flow
soldr gc purge --all                          # delete every eligible candidate
soldr gc purge --older-than 30d --larger-than 1GB
soldr gc purge --json                         # machine-readable purge report
```

Defaults:

- `--older-than 10d`
- `--larger-than 256M`

Compatibility:

- `soldr gc --dry-run` is accepted as a temporary alias for `soldr gc`
- `soldr gc --all` is rejected with a pointer to `soldr gc purge --all`

Safety guards (from `docs/TARGET_GC_PROPOSAL.md`):

- skip a candidate whose workspace `Cargo.lock` was modified within
  the staleness window
- skip a candidate whose `target/.cargo-lock` exists (active build)
- only consider paths under `gc.allowlist_roots` (default: `~/dev`)

The summary includes the registry path, eligible candidate count, total
reclaimable size, skipped/dropped counts, and the largest eligible
target directories with size and last-used age.

Configure additional allowlist roots via `~/.soldr/config.toml`:

```toml
[gc]
allowlist_roots = ["~/dev", "/work/repos"]
```

During build-like `soldr cargo ...` invocations, soldr checks free
space on the relevant target/current filesystem before spawning Cargo.
When less than 2 GB is available, it emits a yellow stderr warning that
recommends `soldr gc`. Disk-space detection failures are ignored so they
never fail the build.

---

## Structured JSON Output

The supported JSON protocol currently exists on:

- `soldr status --json`
- `soldr cache --json`
- `soldr version --json`

The JSON response always includes:

- `schema_version`
- `command`

Current schema version:

- `schema_version: 1`

Compatibility rules for schema version `1`:

- existing fields keep their current meaning
- fields may be added in later releases without changing `schema_version`
- removing a field, renaming a field, or changing the meaning/type of an existing field requires a new schema version
- human-readable stdout for commands without `--json` is not covered by this compatibility promise

Example:

```json
{
  "schema_version": 1,
  "command": "version",
  "soldr_version": "0.7.4"
}
```

---

## Help Surface

```text
Usage:
  soldr <COMMAND>
  soldr <TOOL>[@version] [args...]

Commands:
  cargo    Run Cargo through soldr
  status   Show cache status and tool info
  clean    Clear caches
  config   Show or set configuration
  cache    Inspect the compilation cache
  version  Show version
  gc       Review reclaimable Cargo target/ directories; use gc purge to delete
```

---

## Environment Variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTC_WRAPPER` | Internal build hook used by `soldr cargo ...` | unset |
| `SOLDR_CACHE_ENABLED` | Internal toggle propagated from `soldr cargo ...` into wrapper mode | `1` |
| `SOLDR_RUSTC_WRAPPER` | Override soldr's managed zccache wrapper with another wrapper binary, or disable wrapper injection with `none` / empty | unset |
| `SOLDR_REAL_CARGO`, `SOLDR_REAL_RUSTC`, ... | Internal real-tool path overrides used by setup-soldr PATH shims to avoid recursive tool lookup | unset |
| `SOLDR_ZCCACHE_BIN` | Managed zccache binary path passed from soldr front door into wrapper mode | unset |
| `SOLDR_CACHE_DIR` | Override cache directory | `~/.soldr` |
| `SOLDR_RELOCATED_EXE` | Internal recursion guard set after Windows self-relocation | unset |
| `SOLDR_ORIGINAL_EXE` | Internal path to the original executable when Windows self-relocation is active | unset |
| `ZCCACHE_CACHE_DIR` | zccache cache-root override set by soldr for managed zccache commands | `~/.soldr/cache/zccache` |
| `ZCCACHE_SESSION_ID` | Per-build zccache session identifier set by soldr | unset |
| `SCCACHE_DIR` | sccache cache-root override soldr injects when `SOLDR_RUSTC_WRAPPER=sccache` and the caller has not set it themselves | `~/.soldr/cache/sccache` |
| `SOLDR_LOG` | Log level | `warn` |
| `SOLDR_OFFLINE` | Disable network access for tool fetches | `false` |
| `SOLDR_RUST_PLAN_SKIP_WARM_RESTORE` | Default-on: skip `rust-plan restore` when `target/` is already warm from a prior step in the same GitHub Actions job + attempt (issue #229). Set to a falsy value (`0` / `false` / `no` / `off`) to opt out. | unset (on) |
| `SOLDR_TARGET_CACHE_TAR_THREADS` | Reader-thread count for the target-cache tar walk in zccache, AND for soldr's own thin-slice manifest walk (issue #272). `auto` lets each side pick a vCPU-bounded count (capped at 8). `1` disables parallelism (sequential walk). Any positive integer sets an explicit count, clamped to `[1, 8]` on the soldr side. soldr validates the value at the cargo front door and uses it when statting bundle files for the `manifest.v2.json` thin-slice manifest; the bulk multi-GB `target/` tar walk lives in zccache. | unset (`auto`) |
| `SOLDR_LINKER` | Pick the linker injected for `soldr cargo ...` builds (issue #285). Accepted values: `default` (no injection — keep the rust-toolchain default), `ld` (system linker — also no injection on every supported platform), `mold` (Linux only; hard error elsewhere), `rust-lld` (cross-platform via rustup), `fast` (mold on Linux when present on `PATH`, otherwise rust-lld; rust-lld on macOS and Windows). The choice resolves to `CARGO_TARGET_<TRIPLE>_LINKER` and `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` injected into the spawned cargo process; the active target is the same one Cargo would pick (`--target` flag, `CARGO_BUILD_TARGET`, or the host triple). A `linker = "..."` field in `~/.soldr/config.toml` is honored when the env var is unset. | unset |

`RUSTC_WRAPPER=soldr cargo build` remains a valid low-level passthrough path, but it is no longer the preferred user-facing workflow.
When `SOLDR_RUSTC_WRAPPER` is set to a non-empty value such as `sccache`, soldr puts that binary in the wrapper slot instead of its managed zccache. If it is set to `none` or an empty string, soldr leaves `RUSTC_WRAPPER` unset for that build.

When soldr manages zccache itself, a caller-provided `ZCCACHE_CACHE_DIR` must match the cache root derived from `SOLDR_CACHE_DIR`; conflicting values are rejected. Custom wrapper modes leave caller-provided wrapper environment alone — when `SOLDR_RUSTC_WRAPPER=sccache` and the caller has set `SCCACHE_DIR` themselves, soldr forwards their value rather than overriding it.

`soldr cargo ...` only starts the managed build cache for compile-like Cargo subcommands such as `build`, `check`, `test`, `run`, `doc`, `clippy`, and `nextest`. Non-build Cargo commands such as `cargo metadata` and `cargo --version` pass through without starting zccache.

On Windows, soldr may copy the running `soldr.exe` into `SOLDR_CACHE_DIR/runtime/soldr-self/<version-and-hash>/soldr.exe` and re-run the command from that relocated copy before build orchestration starts. This keeps disposable worktree builds from repeatedly using the worktree-local `soldr.exe` as `RUSTC_WRAPPER`. The trampoline sets `SOLDR_RELOCATED_EXE=1` and `SOLDR_ORIGINAL_EXE=<original path>` as a recursion guard and preserves argv, inherited environment, stdio, and exit status. Stale relocated copies are purged by a best-effort runtime GC step that runs periodically and skips copies that cannot be removed because they are still locked.

`SOLDR_RUST_PLAN_SKIP_WARM_RESTORE` is a default-on short-circuit for the `rust-plan restore` step. After a successful `rust-plan save`, soldr writes a sentinel next to the thin-slice bundle recording the plan inputs hash, target dir, `GITHUB_RUN_ID`, `GITHUB_JOB`, `GITHUB_RUN_ATTEMPT`, zccache session id, and a unix timestamp. On the next invocation, if the sentinel exists and every match field equals the current value — and the sentinel is no older than 5 minutes — soldr skips `rust-plan restore` and leaves the already-warm `target/` tree untouched. This avoids invalidating Cargo's mtime-based fingerprints when split CI steps share a checkout but spawn fresh shells per step (issue #229). The flag is enabled when unset; set it to a falsy value (`0`, `false`, `no`, `off`, or empty, case-insensitive) to opt out, and any other value (including the historical truthy spellings `1`, `true`, `yes`, `on`) keeps the short-circuit enabled. The gate is conservative: a missing, stale, or partially-mismatched sentinel falls through to the normal restore, so the short-circuit can never make a build less correct than the default path. Promoted to default-on after the #229 CI validation runs (PRs #247, #257, #260, #261, #262) landed cleanly on `main`.

---

## Cache Layout

```text
~/.soldr/
|-- bin/
|   `-- <tool>-<version>/
|-- cache/
|   |-- zccache/   # managed zccache artifact + state root (set via ZCCACHE_CACHE_DIR)
|   `-- sccache/   # injected when SOLDR_RUSTC_WRAPPER=sccache and SCCACHE_DIR is unset
|-- runtime/
|   `-- soldr-self/ # Windows self-relocated soldr.exe copies plus periodic GC marker
|-- config.toml
|-- state.redb             # redb state store, including tracked target/ dirs
|-- .gc_warning_marker     # last-emitted timestamp for the stale-target startup warning
`-- daemon.*
```

Both wrapper-cache subdirectories live entirely under the soldr-owned cache root so they never collide with a user-managed `~/.zccache` or the system-default `sccache` location on the same machine.

---

## GitHub Actions

```yaml
- name: Build through soldr
  run: soldr cargo build --release
```

For bootstrap verification of another Rust project:

```yaml
- name: Build third-party project through soldr
  run: soldr cargo build --locked --target ${{ matrix.target }}
```

---

## Summary

The key design rule is simple:

- users build through `soldr cargo ...`
- soldr owns the wrapper slot on the common path
- soldr delegates cache-enabled wrapper invocations into managed zccache
- users do not need to manually wire `RUSTC_WRAPPER` for the common path
