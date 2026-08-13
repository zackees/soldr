"""Tests for the whole-tree production line ceiling (#2493)."""

import importlib.util
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = REPO_ROOT / ".github" / "scripts" / "loc_ceiling.py"

_spec = importlib.util.spec_from_file_location("loc_ceiling", MODULE_PATH)
assert _spec is not None
assert _spec.loader is not None
_ceiling = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_ceiling)


def test_production_sources_exclude_test_targets_and_test_modules() -> None:
    paths = {p.as_posix() for p in _ceiling.production_sources()}
    assert "crates/soldr-cli/src/main_tests.rs" not in paths
    assert "crates/soldr-cli/src/cargo_front_door/tests.rs" not in paths
    assert "crates/soldr-fetch/src/fetch/segmented_download_tests.rs" not in paths
    assert not any("/tests/" in p for p in paths)
    assert "crates/soldr-platform/src/lib.rs" in paths
    assert "crates/soldr-cli/src/cargo_front_door/mod.rs" in paths


def test_ceiling_is_one_thousand() -> None:
    assert _ceiling.CEILING == 1000


def test_violations_are_sorted_and_measured() -> None:
    over = _ceiling.violations()
    assert over, "the current tree must still contain over-ceiling files"
    counts = [count for _, count in over]
    assert counts == sorted(counts, reverse=True)
    assert all(count > 1000 for count in counts)
