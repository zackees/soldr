# CI/CD Workflows

GitHub Actions workflow definitions.

- **ci.yml** - Main lint/build/test orchestration for soldr.
- **_build-and-test.yml** - Reusable per-platform workspace build and test template.
- **_bootstrap-e2e.yml** - Reusable bootstrap test that builds soldr, then uses that binary to build a third-party fixture.
- **setup-soldr-action.yml** - Dogfood smoke test for setup-soldr against the soldr workspace.

Normal build/test workflows use `zackees/setup-soldr` for Rust build acceleration, excluding `release-auto.yml`. Jobs that build soldr before running soldr self-tests or bootstrap tests stop the setup-soldr builder daemon before the test phase, run the test phase with a fresh `SOLDR_CACHE_DIR` / `ZCCACHE_CACHE_DIR`, and request `SOLDR_CACHE_LIFECYCLE=command` for the isolated test cache when supported by soldr.

Docs-only changes should not trigger expensive build, lint, benchmark, or setup-soldr dogfood workflows. When a workflow needs to ignore Markdown, include both `*.md` and `**/*.md`; the root-only pattern does not cover files under `docs/`, `perf/`, or other subdirectories.

Exceptions:

- **release-auto.yml** remains conservative and keeps its existing release artifact build path.
- **parent-cache-bench.yml**, **perf-cold-warm.yml**, **perf-matrix.yml**, **cache-delta-experiment.yml** intentionally compare cache strategies or preserve experiment topology, so setup-soldr is not forced onto the control rows. The third-party comparison surface (`cache-benchmark.yml` and friends) lives in `zackees/setup-soldr` now — see soldr#674.
- **thin-v2-verify.yml** intentionally uses direct Cargo until its existing Phase 4 TODO wires the verifier through `soldr cargo build` and asserts a produced thin-v2 bundle manifest.
