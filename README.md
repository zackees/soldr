# soldr

**Instant tools. Instant builds. One command.**

soldr = [crgx](https://crgx.dev/) + [zccache](https://github.com/zackees/zccache) in a single tool.

In soldering, flux removes the oxide so the joint bonds clean. soldr removes the friction between you and your build — no waiting for tool installs, no waiting for recompilation.

- **Tool acquisition** (the crgx half): Need `maturin`, `cargo-dylint`, or any crate binary? soldr fetches a pre-built binary from GitHub Releases in seconds. No `cargo install` from source. Cached locally for instant reuse.

- **Compilation caching** (the zccache half): When your build invokes `rustc` hundreds of times, soldr caches every compilation unit. Second builds finish in milliseconds, not minutes.

```bash
# Fetch and run any Rust tool instantly:
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs

# Transparent compilation caching (invisible to you):
export RUSTC_WRAPPER=soldr
cargo build --release            # soldr caches every rustc call
```

## How it works

```
soldr maturin build --release
  +-- maturin cached? --> run instantly
  +-- not cached?     --> download pre-built binary (2s) --> run

RUSTC_WRAPPER=soldr cargo build
  +-- rustc invocation #1 -> cache miss -> compile -> store artifact
  +-- rustc invocation #2 -> cache hit  -> return cached .rlib (1ms)
  +-- rustc invocation #N -> cache hit  -> return cached .o (1ms)
  +-- Done. (1.6s warm, 9.8s cold)
```

## Design goals

- **Invisible caching**: `RUSTC_WRAPPER` defaults to `zccache` if not set. Daemon auto-starts. No manual setup.
- **One cache**: Tools and compilation artifacts in a single `~/.soldr/` directory.
- **Pre-built first**: Download a pre-built binary before compiling from source. Fall back gracefully.
- **No cargo wrapping**: soldr wraps `rustc`, not `cargo`. You keep all your cargo flags. No flag-forwarding nightmares.
- **Cross-platform**: Linux, macOS, Windows (x86_64 + aarch64).
- **MSVC by default on Windows**: Always targets `x86_64-pc-windows-msvc` (or `aarch64-pc-windows-msvc`) unless `rust-toolchain.toml` explicitly says otherwise. MSVC links against `vcruntime140.dll` which ships with every modern Windows install. The GNU target requires shipping `libgcc_s_seh-1.dll` and `libwinpthread-1.dll` — extra baggage for no benefit. This matches the Rust ecosystem default (rustup, cargo-binstall, and nearly all published release binaries target MSVC). crgx gets this wrong by baking the target at compile time, causing it to look for GNU binaries when compiled under MSYS2.

## Architecture

```
soldr/
├── crates/
│   ├── soldr-core/      # Shared types, config, cache directory layout
│   ├── soldr-fetch/     # Binary resolution + download (the crgx half)
│   ├── soldr-cache/     # Compilation caching (the zccache half)
│   └── soldr-cli/       # CLI entry point + daemon
├── src/soldr/           # Python package (PyO3 bindings)
└── tests/
```

| Crate | Role |
|---|---|
| `soldr-core` | Cache paths, config, version types |
| `soldr-fetch` | Resolve crate binaries from binstall metadata, GitHub Releases, QuickInstall. Download, verify, cache. |
| `soldr-cache` | Wrap rustc, hash inputs, store/retrieve compiled artifacts. The compilation cache daemon. |
| `soldr-cli` | Mode detection, built-in commands (`status`, `clean`, `config`), tool fetch dispatch. |

## Prior art

Built on lessons from:
- [zccache](https://github.com/zackees/zccache) — 2.4x faster warm builds than sccache ([benchmark](https://github.com/zackees/zccache/issues/20))
- [crgx](https://crgx.dev/) — the npx of Rust, instant tool execution
- [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) — pre-built binary resolution
- [sccache](https://github.com/mozilla/sccache) — the original Rust compilation cache

## License

BSD-3-Clause
