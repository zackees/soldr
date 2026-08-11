from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
publish_npm = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "publish_npm_package.py"
)


def _package(tmp_path: Path) -> Path:
    (tmp_path / "package.json").write_text(
        json.dumps({"name": "@zackees/soldr", "version": "0.9.0"}),
        encoding="utf-8",
    )
    return tmp_path


def test_skips_version_that_is_already_published(monkeypatch, tmp_path: Path) -> None:
    calls = []

    def run(arguments, source_dir):
        calls.append((arguments, source_dir))
        return SimpleNamespace(returncode=0, stdout='"0.9.0"\n', stderr="")

    monkeypatch.setattr(publish_npm, "run_npm", run)

    assert publish_npm.publish(_package(tmp_path)) is False
    assert calls == [(["view", "@zackees/soldr@0.9.0", "version", "--json"], tmp_path)]


def test_publishes_version_missing_from_registry(monkeypatch, tmp_path: Path) -> None:
    calls = []

    def run(arguments, source_dir):
        calls.append((arguments, source_dir))
        if arguments[0] == "view":
            return SimpleNamespace(returncode=1, stdout="", stderr="not found")
        return SimpleNamespace(returncode=0, stdout="published\n", stderr="")

    monkeypatch.setattr(publish_npm, "run_npm", run)

    assert publish_npm.publish(_package(tmp_path)) is True
    assert calls[-1][0] == ["publish"]


def test_publish_failure_is_fatal(monkeypatch, tmp_path: Path) -> None:
    def run(arguments, _source_dir):
        if arguments[0] == "view":
            return SimpleNamespace(returncode=1, stdout="", stderr="not found")
        return SimpleNamespace(returncode=17, stdout="", stderr="oidc denied")

    monkeypatch.setattr(publish_npm, "run_npm", run)

    with pytest.raises(RuntimeError, match="oidc denied"):
        publish_npm.publish(_package(tmp_path))
