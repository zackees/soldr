# ADDENDUM — Bootstrap Toolchain

## Architecture Decision

soldr v0.1 is a **thin fetcher**. Its one real skill: detect the correct target triple (MSVC on Windows) and download a binary. It uses this to bootstrap crgx, then crgx handles the rest.

Over time, crgx's fetch logic gets absorbed into soldr-fetch and zccache's cache logic into soldr-cache. But the MVP ships now with just the fetcher — crgx does the heavy lifting.

**soldr's unique value isn't fetching or caching — it's knowing MSVC is correct on Windows when everything else defaults to GNU.**

## What Was Built

### soldr-core (`crates/soldr-core/src/lib.rs`)
- `TargetTriple::detect()` — runtime platform detection, MSVC on Windows always
- `SoldrPaths` — `~/.soldr/` directory layout (bin, cache, config)
- `SoldrError` — shared error types across crates

### soldr-fetch (`crates/soldr-fetch/src/lib.rs`)
- `fetch_tool(crate_name, version)` — full fetch pipeline
- Resolution chain (Phase 1 MVP):
  1. Local cache (`~/.soldr/bin/<tool>-<version>/`)
  2. crates.io API → get GitHub repository URL
  3. GitHub Releases API → list assets
  4. Asset matching: OS + arch keywords, skip GNU on Windows, prefer MSVC
  5. Download and extract (zip on Windows, tar.gz on Unix)
- Cache: subsequent fetches with same version are instant (no network)

### soldr-cli (`crates/soldr-cli/src/main.rs`)
- RUSTC_WRAPPER detection (pre-clap, checks argv[1] for rustc path)
- Frozen built-in commands: `status`, `clean`, `config`, `cache`, `version`
- Tool dispatch: `soldr <tool>[@version] [args...]` via clap `external_subcommand`
- Removed incorrect `Build` and `Run` subcommands from prior skeleton

### Integration Test (`crates/soldr-fetch/tests/fetch_crgx.rs`)
- `fetch_crgx_and_run`: downloads crgx, verifies binary runs, verifies cache hit
- Run with: `cargo test -p soldr-fetch --test fetch_crgx`

## Bootstrap Chain (Proven)

```
soldr detects x86_64-pc-windows-msvc
  -> queries crates.io for "crgx" -> finds github.com/yfedoseev/crgx
  -> GitHub Releases API -> matches crgx-windows-x86_64-0.1.0.zip
  -> downloads, extracts to ~/.soldr/bin/crgx-0.1.0/crgx.exe
  -> crgx runs OK
  -> second fetch is instant (cached)
```

From here, `soldr crgx <tool>` fetches any Rust tool through crgx.

## New Dependencies

Added to workspace `Cargo.toml`:
- `zip = "2"` — extract .zip archives (Windows binaries)
- `flate2 = "1"` — gzip decompression (Unix binaries)
- `tar = "0.4"` — tar extraction (Unix binaries)

## Running

```bash
# Build
cargo build --workspace

# Unit tests (soldr-core target detection, triple strings, paths)
cargo test --workspace --lib

# Integration test (fetches crgx from GitHub, requires network)
cargo test -p soldr-fetch --test fetch_crgx

# Use the CLI directly
cargo run -p soldr-cli -- crgx --help
cargo run -p soldr-cli -- crgx tokei .
```

## What's Next

- Phase 2: RUSTC_WRAPPER compilation caching (soldr-cache)
- Phase 3: Absorb crgx resolution chain (binstall metadata, QuickInstall)
- Bootstrap guard: detect self-build and pass through to real rustc
