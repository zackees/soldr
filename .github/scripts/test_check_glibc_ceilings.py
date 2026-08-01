"""Tests for check_glibc_ceilings.py (soldr#2145)."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent


@pytest.fixture(scope="module")
def mod():
    spec = importlib.util.spec_from_file_location(
        "check_glibc_ceilings", _HERE / "check_glibc_ceilings.py"
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _workflow(*ceilings: str) -> str:
    """A workflow whose invocations are split across continuation lines,
    which is how they are actually written."""
    steps = []
    for ceiling in ceilings:
        steps.append(
            "      - name: Check glibc baseline\n"
            "        run: |\n"
            "          python3 .github/scripts/verify_glibc_baseline.py \\\n"
            f"            --max-glibc {ceiling} \\\n"
            '            "target/x/release/soldr"\n'
        )
    return "jobs:\n" + "".join(steps)


def _seed(root: Path, first: str, second: str) -> None:
    workflows = root / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "_ci-cross-build-linux.yml").write_text(first, encoding="utf-8")
    (workflows / "cross-compile-all-targets.yml").write_text(second, encoding="utf-8")


def test_agreeing_ceilings_pass(mod, tmp_path):
    _seed(tmp_path, _workflow("2.28"), _workflow("2.28", "2.28"))
    assert mod.main(["--repo-root", str(tmp_path)]) == 0


def test_a_single_drifted_ceiling_fails(mod, tmp_path):
    # The failure this exists for: one lane tightened or loosened alone,
    # leaving the others reporting the old number.
    _seed(tmp_path, _workflow("2.28"), _workflow("2.28", "2.31"))
    assert mod.main(["--repo-root", str(tmp_path)]) == 1


def test_a_removed_ratchet_fails_rather_than_passing_vacuously(mod, tmp_path):
    # Deleting a step would otherwise make this green by checking nothing,
    # which is the worse failure mode.
    _seed(tmp_path, _workflow("2.28"), _workflow("2.28"))
    assert mod.main(["--repo-root", str(tmp_path)]) == 1


def test_release_auto_is_not_consulted(mod, tmp_path):
    # release-auto's ceiling also covers crgx / cargo-chef, which soldr
    # fetches prebuilt at 2.39. It legitimately differs and must not drag
    # the own-binary ceilings with it (soldr#2170).
    _seed(tmp_path, _workflow("2.28"), _workflow("2.28", "2.28"))
    (tmp_path / ".github" / "workflows" / "release-auto.yml").write_text(
        _workflow("2.39", "2.39"), encoding="utf-8"
    )
    assert mod.main(["--repo-root", str(tmp_path)]) == 0


def test_continuation_lines_are_parsed(mod):
    # The flag sits on the line after the script name in every real call
    # site; a same-line-only match would find nothing and pass vacuously.
    assert mod.ceilings_in(_workflow("2.28", "2.39")) == ["2.28", "2.39"]
