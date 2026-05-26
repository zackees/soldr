# CI/CD Workflows

GitHub Actions workflow definitions.

- **ci.yml** - Main lint/build/test orchestration for soldr.
- **_build-and-test.yml** - Reusable per-platform workspace build and test template.
- **_bootstrap-e2e.yml** - Reusable bootstrap test that builds soldr, then uses that binary to build a third-party fixture.
- **setup-soldr-action.yml** - Dogfood smoke test for setup-soldr against the soldr workspace.

Normal build/test workflows use `zackees/setup-soldr` for Rust build acceleration, excluding `release-auto.yml`. Jobs that build soldr before running soldr self-tests or bootstrap tests stop the setup-soldr builder daemon before the test phase, run the test phase with a fresh `SOLDR_CACHE_DIR` / `ZCCACHE_CACHE_DIR`, and request `SOLDR_CACHE_LIFECYCLE=command` for the isolated test cache when supported by soldr.

Exceptions:

- **release-auto.yml** remains conservative and keeps its existing release artifact build path.
- **cache-benchmark.yml**, **cache-benchmark-child-branch.yml**, **parent-cache-bench.yml**, **perf-cold-warm.yml**, **perf-matrix.yml**, **cache-delta-experiment.yml**, and the reusable cache benchmark workflows intentionally compare cache strategies or preserve experiment topology, so setup-soldr is not forced onto the control rows.
- **thin-v2-verify.yml** intentionally uses direct Cargo until its existing Phase 4 TODO wires the verifier through `soldr cargo build` and asserts a produced thin-v2 bundle manifest.
