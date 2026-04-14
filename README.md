# soldr - `UV` but for rust.

**Instant tools. Instant builds. One command.**

The real problem is that the execution path is messy:

- the wrong `cargo` can win on `PATH`
- the wrong Windows target can get selected
- GNU can leak in where MSVC should have been used
- users end up debugging their toolchain instead of shipping code

soldr exists to make that path boring. A rust bootstrapping tool that hydrates your dependency chain.

When you run `soldr`, the tool should do the obvious thing:

- pick MSVC on Windows by default
- fetch the tool you asked for
- cache it locally
- carry `zccache` along for transparent `rustc` caching without manual wrapper setup

If soldr solves that one problem well, it becomes a super tool: the command you reach for first, because it makes the rest of the stack behave.

- **Tool acquisition** (the crgx half): Need `maturin`, `cargo-dylint`, or any crate binary? soldr fetches a pre-built binary from GitHub Releases in seconds. No `cargo install` from source. Cached locally for instant reuse.

# Insanely fast build cache

Powered by [zccache](https://github.com/zackees/zccache) the fastest cpp + rs cache in existance.

**caching not recommended for release builds**

```bash
# Build through soldr's front door:
soldr cargo build --release
soldr cargo test

# Fetch and run any Rust tool instantly:
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
```

## How it works

```text
soldr cargo build --release
  +-- resolve the real cargo binary
  +-- wire soldr's wrapper path internally
  +-- delegate to cargo with your existing flags

soldr maturin build --release
  +-- maturin cached? --> run instantly
  +-- not cached?     --> download pre-built binary (2s) --> run
```

## Design goals

- **One obvious command**: Fetch tools, pick the right Windows target, and enable build caching through the same entry point.
- **Front-door builds**: `soldr cargo ...` is the primary build UX.
- **Invisible caching**: soldr wires its build-assistance internals for you. No manual `RUSTC_WRAPPER` setup in the common case.
- **One cache**: Tools and compilation artifacts in a single `~/.soldr/` directory.
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
| `soldr-cache` | Wrap rustc, hash inputs, store/retrieve compiled artifacts. The compilation cache daemon. |
| `soldr-cli` | Mode detection, cargo front door, built-in commands (`status`, `clean`, `config`), tool fetch dispatch. |

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
