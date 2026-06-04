# soldr Python tests

`pytest`-driven tests for the Python surfaces that ship alongside the
Rust crates: the `setup-soldr` GitHub Action helpers, the exporter, and
the CI benchmark report. Rust crate tests live under `crates/*/tests/`
and `crates/*/src/**_tests.rs`.

## Running

```bash
uv run pytest tests/
```

## Layout

- `test_setup_soldr_action.py` — exercises `resolve_setup.py` end-to-end
  (cache key shapes, target-cache modes, native-cache policy).
- `test_setup_soldr_*.py` — additional unit tests for each
  `.github/actions/setup-soldr/*.py` helper.
- `test_cli.py`, `test_bootstrap_act_image.py`, `test_assert_thin_*.py` —
  Python-side glue for CLI smoke tests and the `nektos/act` smoke image.
- `fixtures/` — golden files (e.g. exporter expected outputs).
- `conftest.py` — shared fixtures and path bootstrap.
