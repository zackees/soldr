# soldr

**Instant tools. Instant builds. One command.**

soldr = [crgx](https://crgx.dev/) + [zccache](https://github.com/zackees/zccache) in a single tool.

In soldering, flux removes the oxide so the joint bonds clean. soldr removes the friction between you and your build: no waiting for tool installs, no waiting for recompilation.

The point of soldr is not to invent some brand-new primitive. The point is to combine the pieces that already work into one tool that people can actually rely on every day.

[zccache](https://github.com/zackees/zccache) is already excellent. [crgx](https://crgx.dev/) already proved the value of instant Rust tooling. soldr turns those into one front door:

- get the right Rust tool for the job
- get the right Windows ABI without thinking about it
- get transparent compilation caching without separate setup

That is the same reason [uv](https://github.com/astral-sh/uv) is compelling. uv did not win because it invented packaging, virtual environments, or Python installation. It won because it made the whole workflow feel like one tool instead of a pile of separate ones.

soldr aims for the same outcome in the Rust toolchain world.

Current release line:

- `0.5.x` is the secure front-door and tool-fetch release line
- `1.0.0-rc` remains reserved for the point where the built-in zccache integration and cache command surface are fully rounded out
## Why soldr exists

On Windows, the real problem is not "how do I cache builds?" or "how do I download a tool binary?" in isolation.

The real problem is that the execution path is messy:

- the wrong `cargo` can win on `PATH`
- the wrong Windows target can get selected
- GNU can leak in where MSVC should have been used
- users end up debugging their toolchain instead of shipping code

soldr exists to make that path boring.

When you run `soldr`, the tool should do the obvious thing:

- pick MSVC on Windows by default
- fetch the tool you asked for
- cache it locally
- fetch and manage zccache so Rust builds get transparent caching without manual wrapper setup

If soldr solves that one problem well, it becomes a super tool: the command you reach for first, because it makes the rest of the stack behave.

- **Tool acquisition** (the crgx half): Need `maturin`, `cargo-dylint`, or any crate binary? soldr fetches a pre-built binary from GitHub Releases in seconds. No `cargo install` from source. Cached locally for instant reuse.

- **Compilation caching** (the zccache half): `soldr cargo ...` now fetches and manages a pinned `zccache` release for Rust builds. soldr owns the zccache daemon/session wiring; zccache's artifact store still uses its current default cache root.

```bash
# Build through soldr's front door:
soldr cargo build --release
soldr cargo test
soldr --no-cache cargo test

# Fetch and run any Rust tool instantly:
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
```

## How it works

```text
soldr cargo build --release
  +-- resolve the real cargo binary
  +-- fetch/start managed zccache when cache is enabled
  +-- set zccache as the compiler wrapper for this build
  +-- delegate to cargo with your existing flags

soldr maturin build --release
  +-- maturin cached? --> run instantly
  +-- not cached?     --> download pre-built binary (2s) --> run
```

## Design goals

- **One obvious command**: Fetch tools, pick the right Windows target, and run through managed zccache through the same entry point.
- **Front-door builds**: `soldr cargo ...` is the primary build UX.
- **Invisible caching**: `soldr cargo ...` uses a soldr-managed zccache by default, with `soldr --no-cache cargo ...` as the opt-out.
- **One cache boundary, eventually**: soldr keeps its own tools and zccache session state in `~/.soldr/`. Current zccache artifacts still live in zccache's default cache root until upstream exposes a supported cache-dir override.
- **Pre-built first**: Download a pre-built binary before compiling from source. Fall back gracefully.
- **Cargo-compatible**: soldr preserves normal cargo arguments instead of forcing a separate workflow.
- **Cross-platform**: Linux, macOS, Windows (x86_64 + aarch64).
- **MSVC by default on Windows**: Always targets `x86_64-pc-windows-msvc` (or `aarch64-pc-windows-msvc`) unless the active project explicitly selects another target in `.cargo/config.toml`, `.cargo/config`, or `rust-toolchain.toml`. MSVC links against `vcruntime140.dll` which ships with every modern Windows install. The GNU target requires shipping `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` with every binary, which is extra baggage for no benefit. This matches the Rust ecosystem default: rustup, cargo-binstall, and nearly all published release binaries target MSVC. crgx gets this wrong by baking the target at compile time, causing it to look for GNU binaries when compiled under MSYS2.

## Architecture

```
soldr/
|-- crates/
|   |-- soldr-core/      # Shared types, config, cache directory layout
|   |-- soldr-fetch/     # Binary resolution + download (the crgx half)
|   |-- soldr-cache/     # Compilation caching (the zccache half)
|   `-- soldr-cli/       # CLI entry point + daemon
|-- src/soldr/           # Python package (PyO3 bindings)
`-- tests/
```

| Crate | Role |
|---|---|
| `soldr-core` | Cache paths, config, version types |
| `soldr-fetch` | Resolve crate binaries from binstall metadata, GitHub Releases, QuickInstall. Download, verify, cache. |
| `soldr-cache` | zccache integration helpers, cache policy, session plumbing. |
| `soldr-cli` | Mode detection, cargo front door, built-in commands (`status`, `clean`, `config`, `cache`), tool fetch dispatch. |

## Prior art

Built on lessons from:
- [zccache](https://github.com/zackees/zccache) - 2.4x faster warm builds than sccache ([benchmark](https://github.com/zackees/zccache/issues/20))
- [crgx](https://crgx.dev/) - the npx of Rust, instant tool execution
- [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) - pre-built binary resolution
- [sccache](https://github.com/mozilla/sccache) - the original Rust compilation cache

## Security And Verification

- [SECURITY.md](./SECURITY.md) describes the current hardening posture and release policy.
- [RELEASE.md](./RELEASE.md) documents the intended maximum-security release setup and owner workflow.
- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md) explains how to verify published release artifacts.
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md) inventories the external systems and artifacts `soldr` currently trusts.

## License

BSD-3-Clause
