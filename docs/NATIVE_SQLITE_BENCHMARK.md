# Native SQLite Benchmark

The `native-sqlite` job in [`.github/workflows/cache-benchmark.yml`](../.github/workflows/cache-benchmark.yml) exercises the new default-on native C/C++ compiler cache injection (issue [#310](https://github.com/zackees/soldr/issues/310)) on a fixture that compiles bundled SQLite from C sources via `cc-rs`. It is the cross-platform validation requested by issue [#312](https://github.com/zackees/soldr/issues/312) — proof that `zccache` caches the SQLite C build on every platform soldr ships binaries for.

## Fixture

[`benchmarks/sqlite-native/`](../benchmarks/sqlite-native/) is a one-binary Rust crate that depends on `libsqlite3-sys` with the `bundled` feature. The build script compiles ~50 C source files through `cc-rs` on every fresh `target/` — the exact workload [`rusqlite`](https://crates.io/crates/rusqlite) pulls in for downstream consumers, and the workload that drives the [#491](https://github.com/zackees/soldr/issues/491) sqlite-link perf regression.

## Matrix

The job fans out across all eight platforms soldr publishes release binaries for:

| Platform family | Targets |
|---|---|
| Linux glibc | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` |
| Linux musl | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` |
| macOS | `x86_64-apple-darwin`, `aarch64-apple-darwin` |
| Windows MSVC | `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` |

`fail-fast: false` ensures one platform's failure does not mask the others.

## Stages per cell

Each matrix cell runs three timed builds:

1. **Cold control** — `CARGO_INCREMENTAL=0`, fresh `target/`, fresh `zccache` cache, no native wrapper. Baseline wall-clock with **no** cache.
2. **Seed (native cache enabled)** — fresh `target/`, fresh `zccache` cache, `CC="zccache cc"` (or `CC="zccache clang-cl"` on Windows MSVC). Warms the zccache cache by compiling all C sources through the wrapper.
3. **Warm (native cache enabled)** — fresh `target/`, **reused** `zccache` cache. This is the cell that demonstrates the win — C compilations should hit the cache populated by stage 2.

Plus a fourth row for the disabled policy:

4. **Disabled-native-cache warm** — fresh `target/`, no native wrapper. Confirms that opting out keeps the rest of the cargo build healthy (i.e. zccache wrapping is not load-bearing for correctness, only for speed).

## Reading the results

Each cell uploads `benchmark-summary/native-sqlite-<target>.json` and appends a markdown table to `$GITHUB_STEP_SUMMARY`:

| policy | result | cold | seed | warm | speedup |
|---|---|---:|---:|---:|---:|
| `native-zccache-enabled` | `success` | 38.4s | 39.1s | 5.1s | 7.53x |
| `native-zccache-disabled` | `success` | 38.4s | n/a | 37.9s | 1.01x |

- **`speedup`** = `cold_seconds / warm_seconds`. A healthy native-cache cell shows >5x on Linux/macOS and >3x on Windows MSVC (where `clang-cl` ships with VS Build Tools and pays a wrapper-startup penalty).
- **`result: unsupported`** on `*-pc-windows-msvc` means `clang-cl` was not on PATH. The benchmark gracefully no-ops with that string in `cache_hit_detail` and exits 0 — the cell is not failing, the platform tool just isn't available on that image.
- **`zccache_stats.after_seed` / `after_warm`** capture the `zccache status` output so you can confirm hit/miss counts went up between the seed and warm runs.

`mode = "report-only"` in [`benchmark.toml`](../benchmark.toml) means there is no enforced speedup gate today — the job records and reports the cross-platform comparison without blocking a merge. The hard gate lives in the smaller perf cluster (`.github/workflows/perf-matrix.yml`) and tracks the `sqlite-link/cold-tar-untar-warm` cell against a 3x threshold; see [issue #491](https://github.com/zackees/soldr/issues/491).

## Running

```yaml
# .github/workflows/cache-benchmark.yml is push-on-main + workflow_dispatch:
gh workflow run cache-benchmark.yml --ref main
```

The Run Benchmark UI also takes a `threshold_ratio` input that controls the gate for the **config-driven** benchmark below it. The native-sqlite job ignores that input — its mode is fixed in `benchmark.toml`.

To run the same fixture locally, mirroring the `enabled` policy:

```bash
zccache start
export CC_KNOWN_WRAPPER_CUSTOM=zccache
export ZCCACHE_PATH_REMAP=auto
export CC="zccache cc"
export CXX="zccache c++"
export CARGO_INCREMENTAL=0
cargo build --manifest-path benchmarks/sqlite-native/Cargo.toml --release --locked
zccache status
```

Stamping the same env vars by hand reproduces what the `cache-benchmark-zccache` action does in CI. On a `soldr cargo build ...` invocation, the same wrapping happens automatically because of soldr#310 — `CC` is unset, soldr synthesises `CC="zccache cc"` and sets `CC_KNOWN_WRAPPER_CUSTOM=zccache` for you, unless `SOLDR_NATIVE_CACHE=0` opts out.

## Relationship to the other benchmark surfaces

- **`perf-matrix.yml`** (`perf/`) — the smaller, gate-enforcing matrix that pins three regression modes per fixture. The `sqlite-link` fixture there exercises the same C compile path under a different scenario lattice (cold-tar-untar-warm, worktree-share, touch-no-change). When the perf cluster's sqlite-link cell shows the >8x speedup target from [#492](https://github.com/zackees/soldr/issues/492), the umbrella issue closes.
- **`cache-benchmark.yml` config-driven job** — uses [`benchmark.toml`](../benchmark.toml)'s `[[profiles]]` and `[[mutations]]` to compare `soldr` vs `swatinem` for top-of-tree and lower-module edits. Independent of the native-cache work.
