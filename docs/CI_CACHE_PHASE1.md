# CI Cache Phase 1 Benchmark

Issue [#136](https://github.com/zackees/soldr/issues/136) moves the benchmark report away from hardcoded scenarios and the oversized command-reference page.

The benchmark pipeline is now driven by [`benchmark.toml`](../benchmark.toml):

- competitors and their internal backend mapping live in the TOML file
- benchmark profiles live in the TOML file
- mutation scenarios live in the TOML file
- the rendered page labels come from the same config

`Cache Benchmark` reads that config and currently benchmarks three profile families on Linux x64:

- `release`: `soldr cargo build --package soldr-cli --release --locked --target <target>`
- `quick`: `soldr cargo check -p soldr-cli --locked --target <target>`
- `lint`: `soldr cargo clippy --workspace --all-targets --locked --target <target> -- -D warnings`

For each selected mutation and visible competitor, the workflow:

- runs one cold control build without the cache backend
- runs one seed build for the configured backend
- applies the mutation and measures the warm build
- writes raw per-run metrics into `cache-benchmark-results.json`
- renders `cache-benchmark-summary.json` and the website bundle from the same TOML config

Presentation changes from the previous version:

- the public page now compares `soldr` vs `swatinem`
- the wide `Result` column is gone
- the giant command-reference table is gone
- the page instead shows one compact comparison table plus a short benchmarked-command list
- the page links to `latest.json` for raw detail

Artifacts:

- `cache-benchmark-raw`: raw benchmark rows in `cache-benchmark-results.json`
- `cache-benchmark-summary`: rendered summary JSON in `cache-benchmark-summary.json`
- `cache-benchmark-www`: static site bundle with `index.html` and `latest.json`

Workflow dispatch inputs:

- `scenario=all|soldr-cli|soldr-core`
- `threshold_ratio=<float>`

Scenarios from `benchmark.toml`:

- `soldr-cli`: top-crate edit in `crates/soldr-cli/src/main.rs`
- `soldr-core`: lower-crate edit in `crates/soldr-core/src/lib.rs`

Recommended run:

1. Dispatch `Cache Benchmark`.
2. Leave `scenario=all` unless you only need one mutation row set.
3. Leave `threshold_ratio=10` unless you are intentionally tightening or loosening the warm-path gate.
4. Open the workflow summary for the compact warm comparison table.
5. Download `cache-benchmark-raw` for the full per-profile JSON rows.
6. Download `cache-benchmark-www` if you want the rendered site bundle directly.
