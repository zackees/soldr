from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "prepare_cook_fixture.py"
WORKFLOW = ROOT / ".github" / "workflows" / "cook-size-gate.yml"


def _fixture_with_patch(tmp_path: Path, patch_path: str | None) -> Path:
    """Build a zccache-shaped checkout whose notify patch points wherever asked."""
    fixture = tmp_path / "zccache-fixture"
    fixture.mkdir()
    manifest = "[package]\nname='zccache'\n"
    if patch_path is not None:
        manifest += f'\n[patch.crates-io]\nnotify = {{ path = "{patch_path}" }}\n'
    (fixture / "Cargo.toml").write_text(manifest, encoding="utf-8")
    return fixture


def _vendored_notify(fixture: Path) -> Path:
    source = fixture / "vendor" / "notify"
    source.mkdir(parents=True)
    (source / "Cargo.toml").write_text("[package]\nname='notify'\n", encoding="utf-8")
    (source / "source.rs").write_text("fixture", encoding="utf-8")
    return source


def test_materializes_notify_patch_as_fixture_sibling(tmp_path: Path) -> None:
    """A `../notify` patch has no sibling in a standalone checkout, so copy one."""
    fixture = _fixture_with_patch(tmp_path, "../notify")
    _vendored_notify(fixture)
    module = load_script_module(SCRIPT, "prepare_cook_fixture")

    destination = module.prepare_fixture(fixture)

    assert destination == tmp_path / "notify"
    assert (destination / "Cargo.toml").is_file()
    assert (destination / "source.rs").read_text(encoding="utf-8") == "fixture"


def test_in_repo_notify_patch_needs_no_preparation(tmp_path: Path) -> None:
    """soldr#3301: zccache 1.14.0 patches `vendor/notify`, already in the checkout.

    The previous script assumed the sibling layout unconditionally and failed
    the whole gate once zccache stopped using it.
    """
    fixture = _fixture_with_patch(tmp_path, "vendor/notify")
    _vendored_notify(fixture)
    module = load_script_module(SCRIPT, "prepare_cook_fixture_in_repo")

    assert module.prepare_fixture(fixture) is None
    assert not (tmp_path / "notify").exists()


def test_absent_notify_patch_needs_no_preparation(tmp_path: Path) -> None:
    """A zccache revision that dropped the patch entirely is not an error."""
    fixture = _fixture_with_patch(tmp_path, None)
    module = load_script_module(SCRIPT, "prepare_cook_fixture_unpatched")

    assert module.prepare_fixture(fixture) is None


def test_refuses_missing_or_preexisting_notify_tree(tmp_path: Path) -> None:
    module = load_script_module(SCRIPT, "prepare_cook_fixture_errors")

    fixture = _fixture_with_patch(tmp_path, "../notify")
    with pytest.raises(SystemExit, match="missing vendored notify"):
        module.prepare_fixture(fixture)

    _vendored_notify(fixture)
    (tmp_path / "notify").mkdir()
    with pytest.raises(SystemExit, match="already exists"):
        module.prepare_fixture(fixture)


def test_refuses_an_in_repo_patch_whose_tree_is_missing(tmp_path: Path) -> None:
    """A broken pin must fail loudly rather than be papered over as a no-op."""
    fixture = _fixture_with_patch(tmp_path, "vendor/notify")
    module = load_script_module(SCRIPT, "prepare_cook_fixture_broken_pin")

    with pytest.raises(SystemExit, match="that tree is missing"):
        module.prepare_fixture(fixture)


def test_workflow_runs_the_tested_fixture_preparation() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "astral-sh/setup-uv@" in workflow
    assert "soldr/.github/scripts/prepare_cook_fixture.py" in workflow
    assert "--fixture zccache-fixture" in workflow
