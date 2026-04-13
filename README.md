# soldr

**Instant tools. Instant builds. One command.**

soldr = [crgx](https://crgx.dev/) + [zccache](https://github.com/zackees/zccache) in a single tool.

In soldering, flux removes the oxide so the joint bonds clean. soldr removes the friction between you and your build — no waiting for tool installs, no waiting for recompilation.

- **Tool acquisition** (the crgx half): Need `maturin`, `cargo-dylint`, or any crate binary? soldr fetches a pre-built binary from GitHub Releases in seconds. No `cargo install` from source. Cached locally for instant reuse.

- **Compilation caching** (the zccache half): When your build invokes `rustc` hundreds of times, soldr caches every compilation unit. Second builds finish in milliseconds, not minutes.

```
# Instead of:
#   cargo binstall maturin        # tool install
#   export RUSTC_WRAPPER=zccache  # cache setup
#   zccache start                 # daemon
#   cargo build                   # build

# Just:
soldr build
```

## How it works

```
soldr build
  |
  +-- Need maturin? -----> check local cache --> fetch pre-built binary (2s)
  +-- Need cargo-dylint? -> check local cache --> fetch pre-built binary (2s)
  |
  +-- rustc invocation #1 -> cache miss -> compile -> store artifact
  +-- rustc invocation #2 -> cache hit  -> return cached .rlib (1ms)
  +-- rustc invocation #N -> cache hit  -> return cached .o (1ms)
  |
  +-- Done. (1.6s warm, 9.8s cold)
```

## Design goals

- **Zero config**: `soldr build` just works. No `RUSTC_WRAPPER`, no daemon management, no PATH hacks.
- **One cache**: Tools and compilation artifacts in a single `~/.soldr/` directory.
- **Pre-built first**: Download a pre-built binary before compiling from source. Fall back gracefully.
- **Drop-in**: Works as `RUSTC_WRAPPER` for existing cargo workflows, or as a standalone CLI.
- **Cross-platform**: Linux, macOS, Windows (x86_64 + aarch64).

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
| `soldr-cli` | `soldr build`, `soldr run <tool>`, `soldr status`, `soldr clean`. Manages the daemon lifecycle. |

## Prior art

Built on lessons from:
- [zccache](https://github.com/zackees/zccache) — 2.4x faster warm builds than sccache ([benchmark](https://github.com/zackees/zccache/issues/20))
- [crgx](https://crgx.dev/) — the npx of Rust, instant tool execution
- [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) — pre-built binary resolution
- [sccache](https://github.com/mozilla/sccache) — the original Rust compilation cache

## License

BSD-3-Clause
