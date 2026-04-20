# CI Cache Phase 1 Benchmark

Issue [#122](https://github.com/zackees/soldr/issues/122) requires Linux x64 baseline measurements on GitHub-hosted runners before any broader CI cache work advances.

Use `.github/workflows/cache-benchmark.yml` for that Phase 1 measurement. The workflow dispatch:

- runs on `ubuntu-24.04` with target `x86_64-unknown-linux-gnu`
- runs both cache backends in parallel: `Swatinem/rust-cache` and `zccache`
- seeds each backend cache in one child job, then measures cold and warm builds for each selected scenario
- measures only the `cargo build --package soldr-cli --release --locked --target <target>` wall time
- uses Python `time.perf_counter()` around the cargo subprocess for the benchmark timing
- writes the human-readable report into the compare jobs and the final workflow summary
- fails a compare job unless the warm path is at least the configured ratio faster than the cold control

Scenarios:

- `soldr-cli`: top-crate edit in `crates/soldr-cli/src/main.rs`
- `soldr-core`: lower-crate edit in `crates/soldr-core/src/lib.rs`
- `all`: runs both scenarios and emits an issue-comment-ready summary in the workflow summary

Recommended Phase 1 run:

1. Dispatch `Cache Benchmark`.
2. Leave `scenario=all`.
3. Leave `threshold_ratio=10`.
4. Read the side-by-side backend report from the compare jobs and the `Phase 1 summary` job.
5. Copy the generated `Issue Comment Draft` block from the workflow summary into issue `#122`.
