from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "prepare_cook_fixture.py"
WORKFLOW = ROOT / ".github" / "workflows" / "cook-size-gate.yml"


def test_materializes_notify_patch_as_fixture_sibling(tmp_path: Path) -> None:
    fixture = tmp_path / "zccache-fixture"
    source = fixture / "vendor" / "notify"
    source.mkdir(parents=True)
    (source / "Cargo.toml").write_text("[package]\nname='notify'\n", encoding="utf-8")
    (source / "source.rs").write_text("fixture", encoding="utf-8")
    module = load_script_module(SCRIPT, "prepare_cook_fixture")

    destination = module.prepare_fixture(fixture)

    assert destination == tmp_path / "notify"
    assert (destination / "Cargo.toml").is_file()
    assert (destination / "source.rs").read_text(encoding="utf-8") == "fixture"


def test_refuses_missing_or_preexisting_notify_tree(tmp_path: Path) -> None:
    module = load_script_module(SCRIPT, "prepare_cook_fixture_errors")
    fixture = tmp_path / "zccache-fixture"
    fixture.mkdir()
    with pytest.raises(SystemExit, match="missing vendored notify"):
        module.prepare_fixture(fixture)

    source = fixture / "vendor" / "notify"
    source.mkdir(parents=True)
    (source / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
    (tmp_path / "notify").mkdir()
    with pytest.raises(SystemExit, match="already exists"):
        module.prepare_fixture(fixture)


def test_workflow_runs_the_tested_fixture_preparation() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "astral-sh/setup-uv@" in workflow
    assert "soldr/.github/scripts/prepare_cook_fixture.py" in workflow
    assert "--fixture zccache-fixture" in workflow
