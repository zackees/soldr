# CI Script Tests

Python-level pytest suites that exercise the CI helper modules in `ci/`
(packaging, perf-guard parsing, release workflow). These are infrastructure
tests for the CI scripts themselves — not project tests. Per project policy,
all benchmarks and application tests are written in Rust.

Run with:

```bash
uv run pytest ci/tests
```
