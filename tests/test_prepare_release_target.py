from __future__ import annotations

import importlib.util
from pathlib import Path
from types import SimpleNamespace

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "prepare_release_target.py"


def _load_script():
    spec = importlib.util.spec_from_file_location("prepare_release_target", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare = _load_script()


def test_retry_publishes_only_successful_environment(
    monkeypatch, tmp_path: Path
) -> None:
    calls = []
    sleeps = []

    def fake_run(command, *, check):
        calls.append(command)
        attempt_env = Path(command[-1])
        if len(calls) == 1:
            attempt_env.write_text("PARTIAL=discard-me\n", encoding="utf-8")
            return SimpleNamespace(returncode=1)
        attempt_env.write_text("TARGET_CC=managed-cc\n", encoding="utf-8")
        return SimpleNamespace(returncode=0)

    monkeypatch.setattr(prepare.subprocess, "run", fake_run)
    monkeypatch.setattr(prepare.time, "sleep", sleeps.append)
    github_env = tmp_path / "github-env"
    github_env.write_text("EXISTING=yes\n", encoding="utf-8")

    prepare.prepare_target(
        soldr="/pinned/soldr",
        target="x86_64-unknown-linux-gnu",
        github_env=github_env,
    )

    assert len(calls) == 2
    assert calls[0][:4] == [
        "/pinned/soldr",
        "prepare",
        "--target",
        "x86_64-unknown-linux-gnu",
    ]
    assert sleeps == [5]
    assert github_env.read_text(encoding="utf-8") == (
        "EXISTING=yes\nTARGET_CC=managed-cc\n"
    )


def test_retry_exhaustion_is_fatal(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.setattr(
        prepare.subprocess,
        "run",
        lambda _command, *, check: SimpleNamespace(returncode=17),
    )
    monkeypatch.setattr(prepare.time, "sleep", lambda _delay: None)

    with pytest.raises(RuntimeError, match="after 2 attempts.*exit code 17"):
        prepare.prepare_target(
            soldr="soldr",
            target="x86_64-pc-windows-msvc",
            github_env=tmp_path / "github-env",
            attempts=2,
        )


def test_attempt_count_must_be_positive(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="at least one"):
        prepare.prepare_target(
            soldr="soldr",
            target="x86_64-pc-windows-msvc",
            github_env=tmp_path / "github-env",
            attempts=0,
        )
