# CI Cache Phase 1 Benchmark

Issue [#122](https://github.com/zackees/soldr/issues/122) requires Linux x64 baseline measurements on GitHub-hosted runners before any broader CI cache work advances.

Use `.github/workflows/cache-benchmark.yml` for that Phase 1 measurement. The workflow dispatch:

- resolves the Phase 1 runner, target, measured command, and mutation labels from [`benchmark.toml`](../benchmark.toml)
- calls the reusable scenario workflow for each selected mutation
- lets each scenario run three reusable child builds: seed, cold, and warm
- measures `cargo build --package soldr-cli --release --locked --target x86_64-unknown-linux-gnu --timings`
- fails unless the warm path is at least the configured ratio faster than the cold control
- writes an issue-comment-ready summary into the workflow summary via `.github/scripts/cache_benchmark_report.py`
- uploads each measured build's `target/cargo-timings` bundle as an artifact

Workflow dispatch inputs:

- `cache_backend=swatinem|zccache`
- `scenario=all|soldr-cli|soldr-core`
- `threshold_ratio=<float>`

Scenarios:

- `soldr-cli`: top-crate edit in `crates/soldr-cli/src/main.rs`
- `soldr-core`: lower-crate edit in `crates/soldr-core/src/lib.rs`

Recommended run:

1. Dispatch `Cache Benchmark`.
2. Leave `cache_backend=swatinem` for the Phase 1 baseline.
3. Leave `scenario=all` unless you only need one mutation case.
4. Leave `threshold_ratio=10` unless you are intentionally tightening or loosening the gate.
5. Open the workflow summary and copy the `Issue Comment Draft` block into issue `#122`.
6. Download any `cache-benchmark-<backend>-<mutation>-<stage>-timings` artifact you want to inspect. Each one contains the `cargo build --timings` output from that child job.

The workflow's Phase 1 defaults currently live in `benchmark.toml` under `[phase1]`. Update that block if the benchmark runner, target, or measured command changes so the workflow, summary text, and docs stay aligned.
