from __future__ import annotations

import importlib.util
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = REPO_ROOT / "src" / "soldr" / "setup_soldr_exporter.py"
FIXTURE_DIR = REPO_ROOT / "tests" / "fixtures" / "setup_soldr_exporter"


def _load_module():
    spec = importlib.util.spec_from_file_location("setup_soldr_exporter", MODULE_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_export_bundle_creates_expected_public_repo_layout(tmp_path: Path) -> None:
    module = _load_module()
    destination = tmp_path / "setup-soldr"

    module.export_setup_soldr_bundle(REPO_ROOT, destination)

    assert sorted(
        path.relative_to(destination).as_posix()
        for path in destination.rglob("*")
        if path.is_file()
    ) == [
        ".github/actions/setup-soldr/ensure_rust_toolchain.py",
        ".github/actions/setup-soldr/ensure_soldr.py",
        ".github/actions/setup-soldr/resolve_setup.py",
        ".github/actions/setup-soldr/verify_soldr.py",
        "LICENSE",
        "README.md",
        "action.yml",
    ]

    assert destination.joinpath("action.yml").read_text(encoding="utf-8") == FIXTURE_DIR.joinpath(
        "expected_action.yml"
    ).read_text(encoding="utf-8")
    assert destination.joinpath("README.md").read_text(encoding="utf-8") == FIXTURE_DIR.joinpath(
        "expected_README.md"
    ).read_text(encoding="utf-8")


def test_export_bundle_excludes_internal_repo_override_input(tmp_path: Path) -> None:
    module = _load_module()
    destination = tmp_path / "setup-soldr"

    module.export_setup_soldr_bundle(REPO_ROOT, destination)

    action_yaml = destination.joinpath("action.yml").read_text(encoding="utf-8")
    assert "repo:" not in action_yaml
    assert "INPUT_REPO" not in action_yaml
    assert "Not part of the intended public setup-soldr@v1 contract" not in action_yaml


def test_export_bundle_refuses_repo_root_as_destination() -> None:
    module = _load_module()

    with pytest.raises(ValueError, match="Destination must not be the source repository root"):
        module.export_setup_soldr_bundle(REPO_ROOT, REPO_ROOT)
