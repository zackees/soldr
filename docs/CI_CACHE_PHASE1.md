# CI Cache Phase 1 Benchmark

Issue [#122](https://github.com/zackees/soldr/issues/122) requires Linux x64 baseline measurements on GitHub-hosted runners before any broader CI cache work advances.

Use `.github/workflows/cache-benchmark.yml` for that Phase 1 measurement. The workflow dispatch:

- runs on `ubuntu-24.04` with target `x86_64-unknown-linux-gnu`
- seeds the cache backend under test in one child job
- measures a cold control build in a second child job
- measures a warm cached build in a third child job after applying a one-line source edit
- fails unless the warm path is at least the configured ratio faster than the cold control
- uploads each child job's `cargo build --timings` HTML output as an artifact

Scenarios:

- `soldr-cli`: top-crate edit in `crates/soldr-cli/src/main.rs`
- `soldr-core`: lower-crate edit in `crates/soldr-core/src/lib.rs`
- `all`: runs both scenarios and emits an issue-comment-ready summary in the workflow summary

Recommended Phase 1 run:

1. Dispatch `Cache Benchmark`.
2. Leave `cache_backend=swatinem`.
3. Leave `scenario=all`.
4. Leave `threshold_ratio=10`.
5. Copy the generated `Issue Comment Draft` block from the workflow summary into issue `#122`.
