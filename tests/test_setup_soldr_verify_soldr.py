from __future__ import annotations

import importlib.util
import json
import subprocess
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "verify_soldr.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("verify_soldr", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_main_tolerates_missing_zccache_daemon_during_status_probe(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module = _load_module()
    github_output = tmp_path / "github.output"
    monkeypatch.setenv("SETUP_SOLDR_PATH", "C:/temp/soldr.exe")
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    calls: list[list[str]] = []

    def fake_check_output(cmd: list[str], text: bool) -> str:
        assert text is True
        assert cmd == ["C:/temp/soldr.exe", "version", "--json"]
        return json.dumps({"soldr_version": "0.7.4"})

    def fake_run(cmd: list[str], **kwargs):
        calls.append(cmd)
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
        ["cargo", "--version"],
        ["rustc", "--version"],
        ["soldr", "status", "--json"],
    ]
    assert github_output.read_text(encoding="utf-8") == "soldr_version=0.7.4\n"


def test_main_propagates_unexpected_status_failures(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    module = _load_module()
    github_output = tmp_path / "github.output"
    monkeypatch.setenv("SETUP_SOLDR_PATH", "C:/temp/soldr.exe")
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))

    def fake_check_output(cmd: list[str], text: bool) -> str:
        assert text is True
        assert cmd == ["C:/temp/soldr.exe", "version", "--json"]
        return json.dumps({"soldr_version": "0.7.4"})

    def fake_run(cmd: list[str], **kwargs):
        if cmd == ["soldr", "status", "--json"]:
            raise subprocess.CalledProcessError(1, cmd, stderr="unexpected failure")
        return subprocess.CompletedProcess(cmd, 0)

    monkeypatch.setattr(module.subprocess, "check_output", fake_check_output)
    monkeypatch.setattr(module.subprocess, "run", fake_run)

    with pytest.raises(subprocess.CalledProcessError, match="soldr"):
        module.main()
