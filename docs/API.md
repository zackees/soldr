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
- `soldr --zccache=system cargo ...` uses the `zccache` already on PATH instead of fetching the pinned managed release. The `zccache-daemon` and `zccache-fp` sibling binaries must live in the same directory as `zccache`. `--zccache=managed` (the default) restores the managed-fetch behavior.
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

For cross-target builds (`soldr cargo --target ...`), the target's Rust standard library must be provisioned separately — see the [native vs cross targets](../README.md#native-vs-cross-targets) section of the README.

### `soldr cook`

Content-addressable dependency pre-build (issue #359). `cook` is a shim
around [`cargo-chef`](https://github.com/LukeMathWalker/cargo-chef) — soldr
ships no Rust reimplementation of the recipe logic; it fetches the pinned
`cargo-chef` binary (currently v0.1.73), drives `cargo chef prepare` to
synthesise a `recipe.json` derived only from `Cargo.toml` + `Cargo.lock`,
then drives `cargo chef cook` to compile a stub project containing those
deps. The output lands in `target/` with no project source code touched,
so any subsequent `soldr cargo build` reuses the compiled deps.

Both phases route through `soldr cargo` so they automatically pick up
zccache (`RUSTC_WRAPPER`), `ZCCACHE_PATH_REMAP=auto`, the soldr linker
selection, and the soldr-managed `CARGO_HOME` / `RUSTUP_HOME`.

```bash
soldr cook                                  # debug cook, ephemeral recipe
soldr cook --release                        # release cook, ephemeral recipe
soldr cook --release --target x86_64-unknown-linux-musl
soldr cook --release --recipe-path recipe.json --prepare-only   # Docker phase 1
soldr cook --release --recipe-path recipe.json --cook-only      # Docker phase 2
soldr cook --release --keep-recipe          # cook + leave recipe.json behind
soldr cook --release -- --features extra,fast --no-default-features
```

Recognised flags:

- `--release` — forwarded to `cargo chef cook --release`.
- `--target <triple>` — forwarded to `cargo chef cook --target <triple>`.
- `--workspace` (alias `--all`) — forwarded to `cargo chef cook --workspace`.
- `--profile <name>` — forwarded to `cargo chef cook --profile <name>`.
- `-p` / `--package <name>` — repeatable; forwarded as `--package <name>`.
- `--recipe-path <path>` — write/read the recipe at this absolute or
  manifest-relative path. Without this flag the recipe lives in a temp
  dir and is deleted on exit.
- `--keep-recipe` — retain the recipe on disk (at `--recipe-path` if
  supplied, else `<cwd>/recipe.json`) so you can inspect it.
- `--prepare-only` — run `cargo chef prepare` only and exit. Used for
  the Docker recipe-layer pattern below.
- `--cook-only` — skip `prepare`; requires `--recipe-path`. Used for the
  Docker cook-layer pattern below.
- `--no-trim` — skip the post-cook `target/` trim. By default (issue
  #459) cook removes cargo-recreatable noise — incremental state, the
  synthetic stub binary, build-script binaries, large stderr blobs,
  debug sidecars, and `examples/`/`doc/`/`tests/` — so the downstream
  tarball ships dramatically fewer bytes (~30–40% drop on a typical
  cook output). Use `--no-trim` only if you genuinely need the full
  raw `target/` tree.
- Anything after `--` is forwarded verbatim to `cargo chef cook` (e.g.
  `--features`, `--no-default-features`, `--all-features`, `--tests`,
  `--benches`).

**Docker recipe pattern** — the canonical use case the issue highlights:

```dockerfile
FROM rust:1 AS chef
RUN cargo install soldr
WORKDIR /app

FROM chef AS planner
COPY . .
RUN soldr cook --release --prepare-only --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Heavy step — cached as long as recipe.json (i.e. Cargo.lock) is stable.
RUN soldr cook --release --cook-only --recipe-path recipe.json
COPY . .
RUN soldr cargo build --release --bin myapp
```

**Local-dev pattern** — first-time setup on a fresh clone:

```bash
git clone <repo> && cd <repo>
soldr cook --release         # one-time dep prebuild; ~10 min cold, ~0s warm
soldr cargo build --release  # builds only the project on top
```

**Cargo.lock missing.** `soldr cook` continues with a warning if
`Cargo.lock` is absent — cargo-chef will derive the recipe from
`Cargo.toml` alone, which weakens content-addressability. For
deterministic builds, commit `Cargo.lock` (libraries should still ship
`Cargo.lock` for reproducibility under `soldr cook`, even though cargo
normally `.gitignore`s lockfiles in library crates).

**Companion automation.** [`zackees/setup-soldr#110`](https://github.com/zackees/setup-soldr/issues/110)
proposes a GitHub Action that key-tarballs the resulting `target/` by
`Cargo.lock` hash; with `soldr cook` available as a primitive that
action's implementation reduces to: hash `Cargo.lock` → restore tarball
→ on miss, `soldr cook` + tar + save.

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

#### `soldr cache prune-target <path>`

Prune stale per-prefix cargo build artifacts inside a given `target/`
directory. Scanned subdirectories under each profile (`debug/`,
`release/`, …) are `deps/`, `.fingerprint/`, `incremental/`, and
`build/`.

Two strategies are available:

- **Default — orphan siblings (issue #336).** Keep the newest entry per
  `(parent_dir, prefix)` bucket. Each subdirectory's set of
  hash-siblings is pruned independently. Conservative: cargo tolerates
  orphans across the four subdirs as long as the live entry inside
  each is current.
- **`--keep-latest` — aggressive (issue #316).** Bucket by `prefix`
  alone; keep only the **newest hash family** per logical artifact
  name, deleting every other hash's files across the four subdirs.
  Shrinks a heavily-rebuilt target/ from 16+ GB to ~2 GB on real
  workloads. Use when you don't need the per-subdir orphan retention.

**Recency ranking.** Both strategies prefer cargo's authoritative
`target/<profile>/.fingerprint/<prefix>-<hash>/invoked.timestamp`
mtime — the same signal cargo's unstable `-Zgc` uses — and fall back
to the entry's own filesystem mtime when the fingerprint file is
missing or unreadable. The JSON output reports per-decision
provenance via `keep_decisions_from_fingerprint` and
`keep_decisions_from_mtime`.

```bash
soldr cache prune-target ./target                  # dry-run, orphan-siblings
soldr cache prune-target ./target --keep-latest    # dry-run, aggressive
soldr cache prune-target ./target --force          # actually delete (orphan-siblings)
soldr cache prune-target ./target --keep-latest --force  # actually delete (aggressive)
soldr cache prune-target ./target --dry-run --json # machine-readable plan
```

Defaults to a dry run for safety; pass `--force` (or `--no-dry-run`)
to actually delete entries. If `target/.cargo-lock` or any
`target/<profile>/.cargo-lock` is present the command refuses with a
non-zero exit code (suggests a live build).

JSON schema (`--json`):

```json
{
  "schema_version": 1,
  "command": "cache prune-target",
  "target_dir": "<absolute path>",
  "dry_run": true,
  "keep_latest": false,
  "scanned": 3,
  "kept": 1,
  "deleted": 2,
  "reclaimed_bytes": 0,
  "reclaimed_human": "0 B",
  "keep_decisions_from_fingerprint": 1,
  "keep_decisions_from_mtime": 0,
  "entries": [
    {
      "path": "<absolute path>",
      "prefix": "libfoo",
      "hash": "abcdef1234567",
      "size_bytes": 0,
      "size_human": "0 B",
      "mtime_unix": 1700000500,
      "action": "keep"
    }
  ]
}
```

**Automatic pre/post-compile pruning (issue #485).** The cargo front
door (`soldr cargo <build-like-subcommand>`) runs the
`--keep-latest` strategy against the resolved `target/` directory
both BEFORE spawning cargo and AFTER cargo succeeds. Hooks engage
for the same set of build-like subcommands that participate in
soldr's compilation cache (build, check, test, clippy, run, doc,
…). Non-build commands (e.g. `cargo metadata`) skip the hooks.

When a pass actually frees bytes a single stderr summary is
emitted, e.g.:

```
soldr: target-gc (before): pruned 4 stale hash families, reclaimed 218 MB
```

Passes that delete nothing stay silent. Passes that refuse because
an active `.cargo-lock` is present (parallel cargo invocations
sharing the same `target/`) also stay silent — the same guard the
manual subcommand uses.

Opt-out surface (all default-off; multiple may combine):

- `--no-gc-target` — skip both pre- and post-compile passes for this
  invocation. Stripped from the arg list before forwarding to cargo.
- `--no-gc-target-before` — skip only the pre-compile pass.
- `--no-gc-target-after` — skip only the post-compile pass.
- `SOLDR_NO_GC_TARGET=1` — env-var equivalent of `--no-gc-target`,
  for invocations the cargo arg list can't reach (e.g. a parent
  process that spawns cargo without going through `soldr cargo`).
  Truthy values (`1`, `true`, `yes`, any non-empty non-zero string)
  enable the opt-out; `0`, `false`, and unset disable it.

The hooks reuse the same `find_active_cargo_lock` guard as the
manual subcommand, so a parallel cargo build in the same `target/`
will never be raced.

### `soldr install-zccache`

Install zccache binaries into soldr's private dir so soldr stops
fetching the managed GitHub release. Pins a user-supplied set of three
zccache binaries (`zccache`, `zccache-daemon`, `zccache-fp`) into
`<SoldrPaths::bin>/zccache-pinned/`. Subsequent `soldr cargo ...`
invocations resolve the pinned binaries automatically.

```bash
soldr install-zccache <SOURCE>   # system | <path> | <url>
soldr install-zccache --remove   # un-pin, idempotent
soldr install-zccache --status   # report sidecar + drift
soldr install-zccache --json     # structured output
```

`<SOURCE>` accepts:

- `system` — copy the `zccache`, `zccache-daemon`, `zccache-fp`
  binaries already on `PATH`.
- A directory path containing the three binaries.
- An archive file (`.zip` / `.tar.gz` / `.tgz` / `.tar.zst`) — recursive
  search for binaries handles nested release layouts like
  `zccache-vX.Y.Z/`.
- An `http(s)://` URL pointing at such an archive.

Resolution chain becomes:

1. `SOLDR_ZCCACHE_LOCAL_DIR` env var (unchanged, highest priority).
2. Pinned install at `<SoldrPaths::bin>/zccache-pinned/` (this command).
3. Managed GitHub Releases fetch (unchanged, default).

Exactly one of `<SOURCE>`, `--remove`, or `--status` must be provided.

### `soldr version`

Print soldr version.

Stable machine-facing mode:

```bash
soldr version --json
```

### `soldr toolchain`

Project-aware orchestrators around `rustup` and `cargo install` that
read `rust-toolchain.toml` so users (and CI) don't have to thread the
pinned channel through every command.

```bash
soldr toolchain install   # rustup toolchain install <channel> --profile minimal --no-self-update
soldr toolchain prepare   # install + component add + target add + cargo install for [soldr.plugins]
soldr toolchain ensure    # bootstrap rustup if missing, run `prepare`, smoke-verify (cargo/rustc --version)
soldr toolchain ensure --json  # same, but emit a stable JSON payload (schema_version: 1)
soldr toolchain link --shim-dir <path> [--json] [--force]  # write PATH shim files (issue #407 Phase 3)
soldr toolchain doctor [--json]   # run env-detection probes (musl-cc, shared target/) (issue #407 Phase 4)
```

`prepare` runs, in order:

1. `rustup toolchain install <channel> --profile minimal --no-self-update`
2. `rustup component add --toolchain <channel> <component>` for every entry in `[toolchain].components`
3. `rustup target add --toolchain <channel> <target>` for every entry in `[toolchain].targets`
4. `cargo install <name> [--version V] [--locked] [--features ...] [--no-default-features]` for every entry in `[soldr.plugins]`

The first non-zero exit short-circuits the chain.

#### `soldr toolchain ensure`

One-shot "make sure this host can build" verb (issue #407 Phase 2):

1. Auto-bootstraps `rustup` into the soldr-managed bin dir if missing
   (reuses the same logic as `soldr bootstrap`). Respects
   `SOLDR_NO_BOOTSTRAP=1`.
2. Runs the same `install` + `component add` + `target add` + plugin-
   install pipeline as `prepare`.
3. Smoke-verifies the resolved toolchain by spawning `cargo --version`
   and `rustc --version`. Either failure marks the verify as failed and
   exits non-zero.

`ensure` is intended for one-stop bootstrap callers like
[setup-soldr#133](https://github.com/zackees/setup-soldr/issues/133)
that want to delegate every TS toolchain step to the soldr binary.
Existing `install` and `prepare` output formats are unchanged.

The `--json` payload (`schema_version: 1`) is the stable contract for
those callers — fields may be added in future schema versions but
existing field names and types will not change without a schema bump:

```json
{
  "schema_version": 1,
  "channel": "1.94.1",
  "rustup_bootstrapped": false,
  "components_added": ["rustfmt", "clippy"],
  "targets_added": ["x86_64-pc-windows-gnu"],
  "plugins_installed": ["cargo-zigbuild@0.18"],
  "smoke_verify": {
    "cargo_version": "cargo 1.94.1 (abc1234 2026-04-15)",
    "rustc_version": "rustc 1.94.1 (def5678 2026-04-15)",
    "ok": true
  },
  "elapsed_ms": 12345
}
```

Notes on the schema:

- `channel` is `null` when no `rust-toolchain.toml` is present; the rest
  of the payload still serializes so consumers can parse unconditionally.
- `rustup_bootstrapped` is `true` only when `ensure` actually fetched
  rustup-init for this invocation. A pre-existing rustup or
  `SOLDR_NO_BOOTSTRAP=1` keeps it `false`.
- `components_added` / `targets_added` / `plugins_installed` mirror the
  manifest entries that the `prepare` pipeline attempted. Plugin labels
  are `name` for bare or `*` versions, otherwise `name@version`.
- `smoke_verify.ok` is `false` when either spawn fails or returns
  non-zero. The JSON payload is emitted in both success and failure
  cases; only the process exit code differs.

#### `soldr toolchain link`

Write PATH shim files into `--shim-dir` so a child process that
resolves `cargo` / `rustfmt` / `clippy-driver` / `rustc` / `rustdoc`
through PATH gets routed back through `soldr <tool>` (issue #407 Phase
3, ports setup-soldr's `ensure-shims.ts`).

Each shim is platform-aware:

- **Unix**: a `#!/bin/sh` script that `exec`s `<soldr-path> <tool>
  "$@"`. File mode `0o755`.
- **Windows**: a `.cmd` script with CRLF line endings that calls
  `"<soldr-path>" <tool> %*`.

The absolute path to the running soldr binary (resolved via
`std::env::current_exe()`) is baked into each shim at write time, so
the shim does not depend on PATH to find soldr itself.

Idempotency:

- Existing shim file whose contents equal the expected body → left
  alone, reported as `skip_reason: "existing-matches"`. Mtime is
  preserved.
- Existing shim file whose contents differ AND `--force` not passed →
  left alone, reported as `skip_reason: "existing-differs"`. The
  caller can detect this and decide whether to re-run with `--force`.
- Existing shim file whose contents differ AND `--force` passed →
  overwritten, reported as `created: true`.

The `--json` payload (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "shim_dir": "/runner/.setup-soldr/shims",
  "tools": [
    {"name": "cargo", "shim_path": "/runner/.setup-soldr/shims/cargo", "created": true},
    {"name": "rustfmt", "shim_path": "/runner/.setup-soldr/shims/rustfmt", "created": false, "skip_reason": "existing-matches"},
    {"name": "clippy-driver", "shim_path": "/runner/.setup-soldr/shims/clippy-driver", "created": true},
    {"name": "rustc", "shim_path": "/runner/.setup-soldr/shims/rustc", "created": true},
    {"name": "rustdoc", "shim_path": "/runner/.setup-soldr/shims/rustdoc", "created": true}
  ],
  "elapsed_ms": 12
}
```

Notes on the schema:

- `tools[].skip_reason` is omitted (not `null`) when `created: true`.
- The `tools` array order is stable: `cargo`, `rustfmt`,
  `clippy-driver`, `rustc`, `rustdoc`.
- `link` never adds the shim directory to the caller's `PATH` — that
  is the caller's responsibility (e.g. setup-soldr emits a GitHub
  Actions `addPath` call after consuming the JSON).

#### `soldr toolchain doctor`

Run env-detection probes that ship the diagnostic intel `setup-soldr`
used to compute in TypeScript (issue #407 Phase 4, ports the
env-detection halves of setup-soldr's `detect-musl-cc.ts`,
`detect-shared-target-warning.ts`, and `diagnostics.ts`).

Namespaced under `toolchain` to avoid colliding with the top-level
`soldr doctor` system check.

Probes (run in stable order):

1. **`musl-cc`** — scans `PATH` for `musl-gcc` / `musl-clang` and, if
   found, captures the first line of `--version` output. Auto-skipped on
   non-Linux hosts (`details.skipped = "not-linux"`). When the host is
   Linux but no musl C compiler is on `PATH`, the probe still reports
   `ok: true` with `details.found = false` — not all Linux workflows
   need musl tooling, so missing it is informational rather than fatal.
2. **`shared-target-warning`** — walks the current workspace's
   `target/` (up to 3 levels deep) looking for a populated
   cargo `.fingerprint/` directory. Mirrors the prepopulated-target
   detector landed in PR #508. Reports `would_warn: true` when at least
   one fingerprint dir is found, signalling that a subsequent
   `soldr cargo build` may collide with the rust-plan restore path.

Exit code: `0` when every probe reports `ok: true`, `1` otherwise.

The `--json` payload (`schema_version: 1`) is the stable contract for
`setup-soldr#133` and similar consumers:

```json
{
  "schema_version": 1,
  "host": {"os": "linux", "arch": "x86_64", "libc": "gnu"},
  "probes": [
    {
      "name": "musl-cc",
      "ok": true,
      "details": {
        "musl_cc": "/usr/bin/musl-gcc",
        "tool": "musl-gcc",
        "version": "musl-tools 1.2.5-1"
      }
    },
    {
      "name": "shared-target-warning",
      "ok": true,
      "details": {
        "target_dir": "/workspace/target",
        "fingerprint_dirs_found": 0,
        "would_warn": false
      }
    }
  ],
  "elapsed_ms": 12
}
```

Notes on the schema:

- `host.os` is `std::env::consts::OS` (`linux` / `windows` / `macos`).
- `host.arch` is `std::env::consts::ARCH` (`x86_64` / `aarch64` / …).
- `host.libc` is `gnu` on Linux, `msvc` on Windows, `darwin` on macOS
  (CLAUDE.md mandates MSVC-default on Windows).
- The `probes` array preserves declaration order: `musl-cc` first,
  `shared-target-warning` second. Future probes will be appended.
- Each probe's `details` object is probe-specific. Consumers must
  switch on `probe.name` before reading nested keys.
- `probe.ok = false` is reserved for future probes; today both probes
  always set `ok: true` because a missing musl-cc or a clean target/
  is informational rather than blocking.

#### `[soldr.plugins]`

Top-level `[soldr]` section of `rust-toolchain.toml`. Currently
surfaces a `plugins` table keyed by cargo crate name. Each value is
either a bare version requirement or a detailed table.

```toml
[toolchain]
channel = "1.94.1"

[soldr.plugins]
cargo-nextest = "0.9"
cargo-zigbuild = { version = "0.18", locked = true }
cargo-deny    = "*"          # any version — `--version` is omitted
cargo-llvm-cov = { version = "0.6", features = ["no_cfg_coverage"], no_default_features = true }
```

Field semantics for the detailed shape:

| Field                 | Type           | Maps to                  |
|-----------------------|----------------|--------------------------|
| `version`             | string         | `--version <value>`. `"*"` or unset omits the flag entirely. |
| `locked`              | bool           | `--locked` when `true`.  |
| `features`            | list of string | `--features <a,b,c>` when non-empty. |
| `no_default_features` | bool           | `--no-default-features` when `true`. |

Installs are dispatched to the cargo binary resolved by soldr's
toolchain probe (`resolve_toolchain_binary("cargo")`) and invoked
directly — **not** through the rustc wrapper. This routes installs
into soldr-managed `$CARGO_HOME` while letting the active cargo honor
`rust-toolchain.toml` at exec time, so no explicit channel is threaded
through. `cargo install` is idempotent by design, so a second
`prepare` after a successful one is a no-op for plugins.

### `soldr doctor`

Diagnose drift between `rust-toolchain.toml` and the rustup state
currently installed for the declared channel. Read-only — `doctor`
never invokes `rustup toolchain install`, `rustup component add`, or
`rustup target add`. Exits `1` when drift is detected, `0` otherwise.
When no `rust-toolchain.toml` exists in the current working directory
the command exits `0` and reports that no manifest was found.

```bash
soldr doctor
soldr doctor --json
soldr doctor --refresh-defender-probe       # Windows: force fresh probe of the cache dir
```

Flags:

- `--json` — emit the stable machine-facing JSON form.
- `--refresh-defender-probe` — ignore the cached Defender real-time-scan
  probe result and run a fresh probe of the soldr cache directory.
  No-op outside Windows. Issue #357.

Example human output (drift detected):

```text
manifest: /home/user/project/rust-toolchain.toml
toolchain: 1.94.1
  status: installed

components (declared 2):
  rustfmt   installed
  clippy    MISSING

targets (declared 1):
  x86_64-unknown-linux-musl   installed

result: drift detected (1 missing component)
hint: run `soldr toolchain prepare` to bring installed state in sync with manifest
```

Example JSON output (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "doctor",
  "manifest_path": "/home/user/project/rust-toolchain.toml",
  "toolchain": {"channel": "1.94.1", "installed": true},
  "components": [
    {"name": "rustfmt", "installed": true},
    {"name": "clippy", "installed": false}
  ],
  "targets": [
    {"triple": "x86_64-unknown-linux-musl", "installed": true}
  ],
  "drift": true,
  "missing_components": ["clippy"],
  "missing_targets": [],
  "defender_probe": {
    "verdict": "scanned",
    "probed_path": "C:\\Users\\user\\.soldr\\cache",
    "median_write_ms": 412,
    "probed_at_unix": 1715000000,
    "refreshed_this_run": false
  }
}
```

**Defender probe** (issue #357, Windows-only). `soldr doctor` runs a
throttled empirical scan probe to detect whether the soldr cache
directory is being inspected by Windows Defender's real-time
protection. The probe writes a 1 MiB `.dll` file into the cache
directory, times the syscall, and classifies the median across 3
repeats: ≥80 ms means scanned, below means excluded (or running on a
trusted Dev Drive). State persists at `~/.soldr/defender-probe.json`
and refreshes only when the cache path changes, the soldr version
changes, the cached state is older than 7 days, or
`--refresh-defender-probe` is passed. The field is omitted from
output on non-Windows platforms.

Component installed-state matching is target-qualified: rustup's
`component list --installed` returns names like
`clippy-x86_64-unknown-linux-gnu`, so a declared `clippy` matches any
installed entry that either equals `clippy` exactly or starts with
`clippy-`.

### `soldr optimize`

Apply platform-specific hot-cache optimizations to remove
build-time penalties caused by antivirus / real-time scanners. On
Windows this adds soldr-owned cache paths (and optionally the current
project's `target/`) to Windows Defender's exclusion list. On macOS
and Linux it is a no-op with a clear message — soldr's workloads do
not require exclusions there.

```bash
soldr optimize                                # default scope: all
soldr optimize --scope global                 # only ~/.soldr/* paths
soldr optimize --scope project                # only <workspace>/target
soldr optimize --scope all                    # both
soldr optimize --dry-run --json               # plan-only output
soldr optimize --undo --scope global          # reverse soldr-added exclusions
```

Flags:

- `--scope {global|project|all}` — what to optimize. Defaults to
  `all`.
- `--undo` — reverse exclusions previously added by soldr. Only
  paths tracked in `~/.soldr/managed-defender-exclusions.json` are
  removed; entries the user added by hand are never touched.
- `--dry-run` — print the plan and exit without invoking PowerShell.
- `--json` — emit the stable machine-facing JSON form.
- `--manifest-path <PATH>` — explicit `Cargo.toml` for the `project`
  scope. When unset, soldr walks up from the current directory.

Platform behavior:

| Platform | Behavior |
|---|---|
| Windows 10 | Add Defender exclusions. UAC self-relaunch when not admin. |
| Windows 11 pre-22H2 | Same as Windows 10, plus an info note about Dev Drive in 22H2. |
| Windows 11 22H2+ | Same as Windows 10, plus a Dev Drive suggestion when `fsutil devdrv` is supported. |
| Windows + Defender disabled | Prints "Defender not active; no exclusions needed." Exits 0. |
| Windows + no PowerShell on PATH | Hard error; exits non-zero. |
| macOS / Linux | No-op; exits 0 with a message. |

The global scope covers:

- `~/.soldr/cache`
- `~/.soldr/bench`
- `~/.soldr/runtime`
- `~/.soldr/state.redb`
- The resolved zccache cache directory (`~/.soldr/cache/zccache`).
  When `ZCCACHE_CACHE_DIR` is set outside soldr's default, the
  resolved path is excluded explicitly and a warning suggests
  unsetting the override.

The project scope covers `<workspace_root>/target/`, where
`<workspace_root>` is the nearest ancestor of the current directory
containing a `Cargo.toml`. Without `--manifest-path` and without a
matching ancestor, the command exits with `no Rust project detected`.

UAC self-relaunch:

When the current process is not elevated, soldr re-launches itself
via `Start-Process powershell -Verb RunAs --as-elevated-helper`. The
elevated child writes its JSON status to a temp file referenced by
the `SOLDR_OPTIMIZE_HELPER_OUTPUT` env var; the parent reads,
prints, and deletes it. The same exit code is propagated. If UAC is
denied or unavailable, soldr prints instructions for running the
helper from an elevated PowerShell and exits non-zero — soldr will
never silently elevate via tokens or scheduled tasks.

CI auto-skip:

`soldr optimize` detects CI via `GITHUB_ACTIONS`, `CI`, `BUILDKITE`,
`CIRCLECI`, `TRAVIS` (truthy values), or a non-empty `JENKINS_URL`,
and exits 0 with a clear message. Ephemeral runners discard
Defender state at job end, so adding exclusions is a no-op there.

Example JSON output (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "optimize",
  "platform": "Windows10",
  "scope": "all",
  "undo": false,
  "dry_run": true,
  "defender_present": true,
  "defender_active": true,
  "actions": [
    {
      "path": "C:\\Users\\you\\.soldr\\cache",
      "action": "add",
      "scope": "global",
      "status": "planned"
    }
  ],
  "note": "dry-run: would add 6 paths to Defender exclusions"
}
```

Suppressing the pre-build warning:

When the cargo front door detects that the soldr cache directory is
being scanned by Defender, it emits a one-line warning to stderr
suggesting `soldr optimize global`. Set `SOLDR_QUIET_DEFENDER=1` to
silence it. The warning is also suppressed automatically in CI.

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
soldr gc list --kind cargo_target_incremental # list one taxonomy kind
soldr gc purge --target-incremental --all      # delete target/<profile>/incremental/
soldr gc purge --build-scripts --all           # delete build-script-build binaries
soldr gc purge --doc --all                     # delete target/doc/
soldr gc purge --subcommand-caches --all       # delete target/{criterion,nextest,...}/
soldr gc purge --registry-src --all            # delete extracted registry sources
soldr gc purge --git-checkouts --all           # delete git checkout worktrees
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

`gc list --json` uses the issue #323 taxonomy. Derived kinds can be
purged through explicit opt-in flags or `--kind`; primary kinds are
report-only and `gc purge --kind <primary>` is rejected before deletion.

Derived purge kinds:

- `cargo_target` (default `gc purge` behavior)
- `cargo_target_incremental`
- `cargo_target_build_script_binaries`
- `cargo_target_doc`
- `cargo_target_subcommand_caches`
- `cargo_registry_src`
- `cargo_git_checkouts`

Report-only primary kinds:

- `cargo_registry_cache`
- `cargo_git_db`
- `cargo_installed_binaries`
- `rustup_toolchain`

**`cargo_registry_src` last-used provenance (issue #349).** When
`soldr gc list --kind cargo_registry_src` (or unfiltered `gc list`)
walks `$CARGO_HOME/registry/src/...`, each entry's `last_used_unix`
field is preferentially derived from cargo's own
`$CARGO_HOME/.global-cache` SQLite tracker — the same data cargo's
unstable `-Zgc` uses to evict least-recently-used crate sources. When
the tracker is missing, locked, schema-drifted, or has no row for a
particular crate, soldr falls back to the directory's filesystem
mtime. The provenance is exposed as `last_used_source` on each entry:
`"global_cache"` or `"fs_mtime"`. The field is omitted from
`cargo_target` entries (no comparable tracker exists).

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

### `soldr gc cargo` (issue #323)

Shells out to nightly cargo's unstable `-Zgc clean gc` against
`$CARGO_HOME`. The CLI prepends `rustup run <toolchain>` so the workspace
`rust-toolchain.toml` is bypassed — `cargo` here always means "the cargo
shipped with the requested toolchain".

```bash
soldr gc cargo                                  # nightly, conservative defaults
soldr gc cargo --dry-run --json                 # plan + machine-readable report
soldr gc cargo --max-src-age 7days --max-crate-age 14days
soldr gc cargo --toolchain nightly-2026-01-01   # pin the nightly snapshot
```

Toolchain resolution: `--toolchain` flag → `$SOLDR_GC_CARGO_TOOLCHAIN`
→ `nightly` default. Missing toolchain is a hard error from explicit
`gc cargo`; the `gc sweep` orchestrator downgrades it to a skip so CI
runners without nightly still get the soldr target purge stage.

JSON shape (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "gc",
  "mode": "cargo",
  "toolchain": "nightly",
  "exit_code": 0,
  "dry_run": false,
  "args": ["-Zgc", "clean", "gc", "--max-src-age=7days"],
  "stdout_bytes": 612,
  "stderr_bytes": 0,
  "skipped": false,
  "skipped_reason": null
}
```

### `soldr gc locations` (issue #323)

Read-only enumeration of every cache directory soldr cares about. No
deletion, no last-used derivation. Walks `$CARGO_HOME/{registry/{src,
cache,index},git/{db,checkouts},.global-cache}`,
`$RUSTUP_HOME/{toolchains,update-hashes}`, `~/.soldr/cache/`, and
`~/.soldr/state.redb`. Missing paths are reported with `exists: false`
and zero size.

```bash
soldr gc locations
soldr gc locations --json
```

Per-entry JSON: `{kind, path, exists, size_bytes, size_human, file_count,
owner, purge_safety}`. `owner` is `cargo` / `rustup` / `soldr`;
`purge_safety` is `regenerable` (safe to delete; cargo will refetch) or
`user_action` (the user installed it on purpose — never auto-purge).

### `soldr gc sweep` (issue #323)

Orchestrator that combines `gc locations`, cargo's `clean gc`, and the
soldr target purge in one shot. Designed to be the user-facing "free
me some disk space" command.

```bash
soldr gc sweep                            # full pipeline, prompt for each target
soldr gc sweep --all --dry-run --json     # plan everything, delete nothing
soldr gc sweep --no-cargo-gc              # skip cargo (e.g. no nightly available)
soldr gc sweep --aggressive               # second cargo pass with tighter ages
```

Stages, in order:

1. `gc locations` (always — read-only).
2. cargo `clean gc` with conservative defaults (unless `--no-cargo-gc`
   or nightly is missing — auto-skipped).
3. soldr's target purge over registered workspaces. Respects `--all`
   (no prompt) and the configured `gc.allowlist_roots`.
4. `--aggressive` only: second cargo pass with
   `--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`,
   each clamped to `auto_gc.min_age_secs`.

### Automatic GC under disk pressure (issue #323)

soldr's cargo front door triggers a background auto-GC pass when free
space on any soldr-relevant volume drops below the configured trigger.
Per volume (Windows: drive letter; Unix: device id):

1. tier 1 — cargo `clean gc` with conservative ages,
2. tier 2 — soldr target purge with `larger_than = 256M` and
   `older_than = max(1h, auto_gc.min_age_secs)`,
3. tier 3 — cargo `clean gc` with `--max-src-age=7d
   --max-crate-age=14d --max-git-co-age=7d`,
4. stop. Anything more aggressive requires explicit
   `soldr gc sweep --aggressive`.

Configure in `~/.soldr/config.toml`:

```toml
[auto_gc]
enabled = true          # opt-out; on by default
trigger_free_gb = 20    # start GC when free space < this
target_free_gb = 30     # stop GC when free space >= this
min_age_secs = 3600     # never touch anything modified within this window
```

Behavior:

- Background-only. The GC pass runs on a detached thread named
  `soldr-auto-gc`, so the build never blocks waiting for it.
- Throttled to ~once per 5 minutes via `~/.soldr/.auto_gc_marker`.
- Set `SOLDR_AUTO_GC_DISABLED=1` to disable for a single invocation
  without editing config.
- Volumes that already have plenty of free space are skipped, even
  when another volume on the same machine is below the trigger.
- Every check that crosses the trigger writes a structured log line to
  `~/.soldr/logs/auto-gc.log`. The file rotates to
  `auto-gc.log.old` once it exceeds 10 MiB.
- Cargo's `.package-cache` mutex serializes auto-GC against any
  in-flight `cargo build`, so concurrent builds and GC don't race.

---

## Structured JSON Output

The supported JSON protocol currently exists on:

- `soldr status --json`
- `soldr cache --json`
- `soldr cache prune-target <path> --json`
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
| `SOLDR_ZCCACHE_LOCAL_DIR` | Override the managed-zccache resolution: instead of fetching from GitHub Releases (or installing from crates.io), use the locally-built binaries in this directory. Expected to contain `zccache.exe`, `zccache-daemon.exe`, and `zccache-fp.exe` (or no-extension equivalents on Unix). Adjacent `.pdb` files (Windows), `.dwp` files (Linux), or `.dSYM` directories (macOS) are copied alongside so debuggers can resolve symbols. Used to chase the zccache daemon-stdio hang on Windows where the released binary ships without easily discoverable PDBs. Run `soldr doctor` to confirm the resolved `symbol path`. | unset |
| `SOLDR_CACHE_DIR` | Override cache directory | `~/.soldr` |
| `SOLDR_CACHE_LIFECYCLE` | zccache daemon lifetime for `soldr cargo ...`. `job` keeps the scoped daemon alive for later soldr invocations in the same job. `command` ends the zccache session and stops the scoped daemon before `soldr cargo ...` exits; intended for self-build CI where later tests must not inherit the builder daemon. | `job` |
| `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS` | Maximum seconds to wait for command-lifetime zccache shutdown confirmation after `zccache stop`. | `30` |
| `SOLDR_RELOCATED_EXE` | Internal recursion guard set after Windows self-relocation | unset |
| `SOLDR_ORIGINAL_EXE` | Internal path to the original executable when Windows self-relocation is active | unset |
| `ZCCACHE_CACHE_DIR` | zccache cache-root override set by soldr for managed zccache commands | `~/.soldr/cache/zccache` |
| `ZCCACHE_SESSION_ID` | Per-build zccache session identifier set by soldr | unset |
| `ZCCACHE_PATH_REMAP` | zccache path-remap mode. soldr seeds `auto` on the child cargo for managed-zccache builds so multiple git worktrees of the same repo share cache hits (issue #352, Tier L1.x). Caller-supplied values are preserved. Requires a real `.git/` checkout — tarball/zip checkouts silently fall back to no remap. | unset (soldr injects `auto`) |
| `SOLDR_PATH_REMAP` | Escape hatch for the default `ZCCACHE_PATH_REMAP=auto` injection. `off` (case-insensitive) suppresses the injection; any other value, or unset, keeps the default behavior. | unset (`auto`) |
| `SCCACHE_DIR` | sccache cache-root override soldr injects when `SOLDR_RUSTC_WRAPPER=sccache` and the caller has not set it themselves | `~/.soldr/cache/sccache` |
| `SOLDR_LOG` | Log level | `warn` |
| `SOLDR_OFFLINE` | Disable network access for tool fetches | `false` |
| `SOLDR_RUST_PLAN_SKIP_WARM_RESTORE` | Default-on: skip `rust-plan restore` when `target/` is already warm from a prior step in the same GitHub Actions job + attempt (issue #229). Set to a falsy value (`0` / `false` / `no` / `off`) to opt out. | unset (on) |
| `SOLDR_TARGET_CACHE_TAR_THREADS` | Reader-thread count for the target-cache tar walk in zccache, AND for soldr's own thin-slice manifest walk (issue #272). `auto` lets each side pick a vCPU-bounded count (capped at 8). `1` disables parallelism (sequential walk). Any positive integer sets an explicit count, clamped to `[1, 8]` on the soldr side. soldr validates the value at the cargo front door and uses it when statting bundle files for the `manifest.v2.json` thin-slice manifest; the bulk multi-GB `target/` tar walk lives in zccache. | unset (`auto`) |
| `SOLDR_LINKER` | Pick the linker injected for `soldr cargo ...` builds (issue #285). Accepted values: `default` (no injection — keep the rust-toolchain default), `ld` (system linker — also no injection on every supported platform), `mold` (Linux only; hard error elsewhere), `rust-lld` (Windows MSVC and Linux/MinGW via `clang -fuse-ld=lld`; **no-op on macOS** — see below), `fast` (mold on Linux when present on `PATH`, otherwise rust-lld; rust-lld on Windows MSVC; **no-op on macOS** — see below). The choice resolves to `CARGO_TARGET_<TRIPLE>_LINKER` and `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` injected into the spawned cargo process; the active target is the same one Cargo would pick (`--target` flag, `CARGO_BUILD_TARGET`, or the host triple). A `linker = "..."` field in `~/.soldr/config.toml` is honored when the env var is unset. On macOS targets `rust-lld` and `fast` fall back silently to the platform default linker (issue #509): Apple clang only accepts `-fuse-ld=lld` when the toolchain wires up an `ld64.lld` shim, and stock macOS toolchains do not — injecting it would break even `cc-rs` build-script compilations. | unset |
| `SOLDR_QUIET_DEFENDER` | Suppress the once-per-day pre-build warning emitted by the cargo front door when Defender is actively scanning the soldr cache directory (issue #358). Truthy values silence the warning; the warning is also automatically suppressed in CI environments. | unset |
| `SOLDR_OPTIMIZE_HELPER_OUTPUT` | Internal: set by the parent soldr process when it re-launches itself elevated via `--as-elevated-helper`. The elevated child writes its JSON status to this path so the parent can read and propagate it. | unset |

`RUSTC_WRAPPER=soldr cargo build` remains a valid low-level passthrough path, but it is no longer the preferred user-facing workflow.
When `SOLDR_RUSTC_WRAPPER` is set to a non-empty value such as `sccache`, soldr puts that binary in the wrapper slot instead of its managed zccache. If it is set to `none` or an empty string, soldr leaves `RUSTC_WRAPPER` unset for that build.

When soldr manages zccache itself, a caller-provided `ZCCACHE_CACHE_DIR` must match the cache root derived from `SOLDR_CACHE_DIR`; conflicting values are rejected. Custom wrapper modes leave caller-provided wrapper environment alone — when `SOLDR_RUSTC_WRAPPER=sccache` and the caller has set `SCCACHE_DIR` themselves, soldr forwards their value rather than overriding it.

`soldr cargo ...` only starts the managed build cache for compile-like Cargo subcommands such as `build`, `check`, `test`, `run`, `doc`, `clippy`, and `nextest`. Non-build Cargo commands such as `cargo metadata` and `cargo --version` pass through without starting zccache.

Set `SOLDR_CACHE_LIFECYCLE=command` for self-build jobs that use soldr only as
the builder and then run tests against zccache or soldr itself. The command
lifetime mode finalizes zccache session stats first, then runs `zccache stop`
against soldr's scoped `ZCCACHE_CACHE_DIR` and waits until `zccache status`
reports that daemon is gone. It does not kill unrelated zccache daemons rooted
at other cache directories.

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
