# CI Cache Phase 1 Benchmark

Issue [#122](https://github.com/zackees/soldr/issues/122) requires Linux x64 baseline measurements on GitHub-hosted runners before any broader CI cache work advances.

Use `.github/workflows/cache-benchmark.yml` for that Phase 1 measurement. The workflow dispatch:

- runs on `ubuntu-24.04` with target `x86_64-unknown-linux-gnu`
- runs both cache backends in parallel: `Swatinem/rust-cache` and `zccache`
- seeds each backend cache in one child job, then measures cold and warm builds for each selected scenario
- measures only the `cargo build --package soldr-cli --release --locked --target <target>` wall time
- uses Python `time.perf_counter()` around the cargo subprocess for the benchmark timing
- uploads one top-level `cache-benchmark-summary` artifact containing `cache-benchmark-summary.json`
- uploads one website-ready `cache-benchmark-www` artifact containing `www/benchmarks/index.html` and `www/benchmarks/latest.json`
- keeps the final workflow summary focused on percent less wall time than bare and the leader's advantage over the next-best cache backend
- fails a compare job unless the warm path is at least the configured ratio faster than the cold control

Scenarios:

- `soldr-cli`: top-crate edit in `crates/soldr-cli/src/main.rs`
- `soldr-core`: lower-crate edit in `crates/soldr-core/src/lib.rs`
- `all`: runs both scenarios and writes both mutation summaries into the same top-level JSON artifact

Recommended Phase 1 run:

1. Dispatch `Cache Benchmark`.
2. Leave `scenario=all`.
3. Leave `threshold_ratio=10`.
4. Open the `Phase 1 summary` job for the high-level percent deltas.
5. Download the `cache-benchmark-summary` artifact if you need the raw wall times and cache-hit details.
6. Download `cache-benchmark-www` if you want the static benchmark page bundle for a `www` site.
