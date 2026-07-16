# PEP 517 local-install benchmarks

`pep517-baseline.json` preserves the pre-migration fbuild measurements from
issue #1725. It is intentionally a raw baseline, not a claim about the
post-migration result.

Run the checked-in harness from the soldr repository with:

```text
uv run --no-project --script ci/bench_pep517.py --project C:/path/to/fbuild3
```

The default is three repetitions for each editable and wheel scenario:

- cold install into a fresh temporary environment;
- warm/no-op `pip install .` into the same environment;
- an install after appending a harmless newline to a copied Rust source file.

The harness creates only temporary project copies and virtual environments.
It inherits Cargo, rustup, soldr, and zccache configuration so the comparison
measures the real local cache state. It does not remove or redirect user
caches. The JSON result contains medians, return codes, phase-line counts, and
per-sample logs under the output directory.

Report percentage improvement for each matched scenario as:

```text
(baseline_seconds - soldr_seconds) / baseline_seconds * 100
```

For a quick smoke test, pass `--repetitions 1`. Use the same machine, source
revision, cache state, and environment for before/after comparisons.

For a literal pip frontend measurement, use `--frontend pip`. Add
`--force-reinstall` only when the benchmark should include uninstalling and
replacing an already-installed package. `--no-build-isolation` plus
`--backend-source <soldr checkout>` is useful for measuring uninstalled soldr
changes locally; the harness seeds setuptools before timing. Downstream
projects that stage native binaries can pass `--touch-staged-artifacts` to
model their already-built warm state without changing the original checkout.
