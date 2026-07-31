from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "verify_soldr.py"


def _load_module():
    return load_script_module(SCRIPT_PATH, "verify_soldr")


def test_main_tolerates_missing_zccache_daemon_during_status_probe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module = _load_module()
    github_output = tmp_path / "github.output"
    monkeypatch.setenv("SETUP_SOLDR_PATH", "C:/temp/soldr.exe")
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    calls: list[tuple[list[str], dict[str, object]]] = []

    def fake_check_output(cmd: list[str], *, text: bool, timeout: int) -> str:
        assert text is True
        assert timeout == 30
        assert cmd == ["C:/temp/soldr.exe", "version", "--json"]
        return json.dumps({"soldr_version": "0.7.4"})

    def fake_run(cmd: list[str], **kwargs: Any):
        calls.append((cmd, kwargs))
        if cmd == ["soldr", "status", "--json"]:
            raise subprocess.CalledProcessError(
                1,
                cmd,
                output="",
                stderr=(
                    "soldr: zccache status failed: daemon not running at "
                    "\\\\.\\pipe\\zccache-runneradmin"
                ),
            )
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module.subprocess, "check_output", fake_check_output)
    monkeypatch.setattr(module.subprocess, "run", fake_run)

    module.main()

    assert calls == [
        (["cargo", "--version"], {"check": True, "timeout": 30}),
        (["rustc", "--version"], {"check": True, "timeout": 30}),
        (
            ["soldr", "status", "--json"],
            {"check": True, "timeout": 30, "capture_output": True, "text": True},
        ),
    ]
    assert github_output.read_text(encoding="utf-8") == "soldr_version=0.7.4\n"


def test_main_propagates_unexpected_status_failures(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module = _load_module()
    github_output = tmp_path / "github.output"
    monkeypatch.setenv("SETUP_SOLDR_PATH", "C:/temp/soldr.exe")
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    def fake_check_output(cmd: list[str], *, text: bool, timeout: int) -> str:
        assert text is True
        assert timeout == 30
        assert cmd == ["C:/temp/soldr.exe", "version", "--json"]
        return json.dumps({"soldr_version": "0.7.4"})

    def fake_run(cmd: list[str], **kwargs: Any):
        assert kwargs["check"] is True
        assert kwargs["timeout"] == 30
        if cmd == ["soldr", "status", "--json"]:
            raise subprocess.CalledProcessError(1, cmd, stderr="unexpected failure")
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module.subprocess, "check_output", fake_check_output)
    monkeypatch.setattr(module.subprocess, "run", fake_run)

    with pytest.raises(subprocess.CalledProcessError, match="soldr"):
        module.main()


def test_subprocess_helpers_translate_timeouts(monkeypatch: pytest.MonkeyPatch) -> None:
    module = _load_module()

    def fake_check_output(cmd: list[str], *, text: bool, timeout: int) -> str:
        assert text is True
        assert timeout == 30
        raise subprocess.TimeoutExpired(cmd, timeout)

    monkeypatch.setattr(module.subprocess, "check_output", fake_check_output)
    with pytest.raises(RuntimeError, match="version --json timed out after 30s"):
        module._check_output(["soldr", "version", "--json"])

    def fake_run(cmd: list[str], **kwargs: Any):
        assert kwargs["check"] is True
        assert kwargs["timeout"] == 30
        raise subprocess.TimeoutExpired(cmd, kwargs["timeout"])

    monkeypatch.setattr(module.subprocess, "run", fake_run)
    with pytest.raises(RuntimeError, match="status --json timed out after 30s"):
        module._run(["soldr", "status", "--json"])
