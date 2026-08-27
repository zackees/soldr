# soldr Python tests

`pytest`-driven tests for the Python surfaces that ship alongside the
Rust crates: the `setup-soldr` GitHub Action helpers, the exporter, and
the CI benchmark report. Rust crate tests live under `crates/*/tests/`
and `crates/*/src/**_tests.rs`.

## Running

```bash
uv run pytest tests/
```

Heavy Docker checks are opt-in:

```bash
uv run --no-sync pytest tests/test_nextest_archive_cacheability.py --cacheability-integration
```

That check runs `ci/assert_nextest_archive_cacheability.py`, which builds
the full `soldr cargo nextest archive --workspace` path twice in
Linux Docker. It forces the warm pass with `cargo clean` and a soldr cache
daemon restart, then fails unless the warm zccache report has positive hits
and zero misses.

> **Superseded (soldr#2931):** the invariant this check enforces — the full
> linked test archive must be warm-cacheable — has been inverted: linked test
> products are never cacheable. The check is scheduled for retirement or
> rewrite under soldr#2937; do not extend it.

## Layout

- `test_setup_soldr_action.py` — exercises `resolve_setup.py` end-to-end
  (cache key shapes, target-cache modes, native-cache policy).
- `test_setup_soldr_*.py` — additional unit tests for each
  `.github/actions/setup-soldr/*.py` helper.
- `test_cli.py`, `test_bootstrap_act_image.py`, `test_assert_thin_*.py` —
  Python-side glue for CLI smoke tests and the `nektos/act` smoke image.
- `fixtures/` — golden files (e.g. exporter expected outputs).
- `conftest.py` — shared fixtures and path bootstrap.
