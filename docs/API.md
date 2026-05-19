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

For cross-target builds (`soldr cargo --target ...`), the target's Rust standard library must be provisioned separately — see the [native vs cross targets](../README.md#native-vs-cross-targets) section of the README.

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

First slice of issue #316. Prune stale per-prefix cargo build artifacts
inside a given `target/` directory, keeping only the newest entry per
`(parent_dir, prefix)` bucket. Scanned subdirectories under each
profile (`debug/`, `release/`, …) are `deps/`, `.fingerprint/`,
`incremental/`, and `build/`.

```bash
soldr cache prune-target ./target                  # dry-run report
soldr cache prune-target ./target --force          # actually delete
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
  "scanned": 3,
  "kept": 1,
  "deleted": 2,
  "reclaimed_bytes": 0,
  "reclaimed_human": "0 B",
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

Automatic pruning via `RUSTC_WRAPPER` pre/post-compile hooks is
deferred to a follow-up — the manual subcommand is intentionally
opt-in until the behaviour is trusted on real `target/` directories.

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
```

`prepare` runs, in order:

1. `rustup toolchain install <channel> --profile minimal --no-self-update`
2. `rustup component add --toolchain <channel> <component>` for every entry in `[toolchain].components`
3. `rustup target add --toolchain <channel> <target>` for every entry in `[toolchain].targets`
4. `cargo install <name> [--version V] [--locked] [--features ...] [--no-default-features]` for every entry in `[soldr.plugins]`

The first non-zero exit short-circuits the chain.

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
```

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
  "missing_targets": []
}
```

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
| `SOLDR_LINKER` | Pick the linker injected for `soldr cargo ...` builds (issue #285). Accepted values: `default` (no injection — keep the rust-toolchain default), `ld` (system linker — also no injection on every supported platform), `mold` (Linux only; hard error elsewhere), `rust-lld` (cross-platform via rustup), `fast` (mold on Linux when present on `PATH`, otherwise rust-lld; rust-lld on macOS and Windows). The choice resolves to `CARGO_TARGET_<TRIPLE>_LINKER` and `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` injected into the spawned cargo process; the active target is the same one Cargo would pick (`--target` flag, `CARGO_BUILD_TARGET`, or the host triple). A `linker = "..."` field in `~/.soldr/config.toml` is honored when the env var is unset. | unset |
| `SOLDR_QUIET_DEFENDER` | Suppress the once-per-day pre-build warning emitted by the cargo front door when Defender is actively scanning the soldr cache directory (issue #358). Truthy values silence the warning; the warning is also automatically suppressed in CI environments. | unset |
| `SOLDR_OPTIMIZE_HELPER_OUTPUT` | Internal: set by the parent soldr process when it re-launches itself elevated via `--as-elevated-helper`. The elevated child writes its JSON status to this path so the parent can read and propagate it. | unset |

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
