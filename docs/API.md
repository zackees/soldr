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

- Resolve the real `cargo` binary through `rustup`
- Resolve the matching real `rustc` binary through `rustup`
- Fetch a pinned managed `zccache` release when caching is enabled
- Set `RUSTC_WRAPPER` to the current soldr binary
- Pass the managed `zccache` binary path into wrapper mode through the environment
- Start a per-build zccache session on zccache's current default daemon endpoint
- Delegate to Cargo with the exact flags the user passed

Current cache-control behavior:

- caching is enabled by default for `soldr cargo ...`
- `soldr --no-cache cargo ...` disables soldr's compilation-cache path for that invocation
- `soldr cargo --no-cache ...` is rejected; `--no-cache` is a top-level soldr flag only
- zccache integration currently targets Rust builds through the cargo front door
- zccache's current artifact store and daemon endpoint remain on zccache's default paths; soldr currently manages the session lifecycle and logs

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

- Wrapper mode still transparently resolves the real `rustc`
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
  "soldr_version": "0.6.0"
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
```

---

## Environment Variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTC_WRAPPER` | Internal build hook used by `soldr cargo ...` | unset |
| `SOLDR_CACHE_ENABLED` | Internal toggle propagated from `soldr cargo ...` into wrapper mode | `1` |
| `SOLDR_ZCCACHE_BIN` | Managed zccache binary path passed from soldr front door into wrapper mode | unset |
| `SOLDR_CACHE_DIR` | Override cache directory | `~/.soldr` |
| `ZCCACHE_SESSION_ID` | Per-build zccache session identifier set by soldr | unset |
| `SOLDR_LOG` | Log level | `warn` |
| `SOLDR_OFFLINE` | Disable network access for tool fetches | `false` |

`RUSTC_WRAPPER=soldr cargo build` remains a valid low-level passthrough path, but it is no longer the preferred user-facing workflow.

---

## Cache Layout

```text
~/.soldr/
|-- bin/
|   `-- <tool>-<version>/
|-- cache/
|-- config.toml
`-- daemon.*
```

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
