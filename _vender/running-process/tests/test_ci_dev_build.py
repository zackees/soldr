from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from ci import dev_build


def test_ensure_dev_wheel_reuses_cached_wheel(monkeypatch, tmp_path: Path) -> None:
    dist = tmp_path / "dist"
    dist.mkdir()
    wheel = dist / "running_process-3.0.2-cp313-cp313-win_amd64.whl"
    wheel.write_text("wheel", encoding="utf-8")
    state_path = dist / ".running-process-dev-build.json"
    state_path.write_text(
        '{"fingerprint": "abc123", "wheel": "running_process-3.0.2-cp313-cp313-win_amd64.whl"}',
        encoding="utf-8",
    )

    calls: list[list[str]] = []

    def fake_run(command, cwd, check, env):
        calls.append([str(part) for part in command])
        return subprocess.CompletedProcess(command, 0)

    monkeypatch.setattr(dev_build, "build_env", lambda: {})
    monkeypatch.setattr(dev_build, "source_fingerprint", lambda root: "abc123")
    monkeypatch.setattr(dev_build.subprocess, "run", fake_run)

    action = dev_build.ensure_dev_wheel(
        tmp_path / ".venv" / "Scripts" / "python.exe",
        root=tmp_path,
    )

    assert action == "reused"
    assert calls == [
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(tmp_path / ".venv" / "Scripts" / "python.exe"),
            "--reinstall",
            "--no-deps",
            str(wheel),
        ]
    ]


def test_ensure_dev_wheel_builds_and_records_state(monkeypatch, tmp_path: Path) -> None:
    dist = tmp_path / "dist"
    dist.mkdir()
    built_wheel = dist / "running_process-3.0.2-cp313-cp313-win_amd64.whl"

    calls: list[list[str]] = []

    def fake_run(command, cwd, check, env):
        calls.append([str(part) for part in command])
        built_wheel.write_text("wheel", encoding="utf-8")
        return subprocess.CompletedProcess(command, 0)

    monkeypatch.setattr(dev_build, "build_env", lambda: {})
    monkeypatch.setattr(dev_build, "source_fingerprint", lambda root: "new-fingerprint")
    monkeypatch.setattr(dev_build.subprocess, "run", fake_run)

    action = dev_build.ensure_dev_wheel(
        tmp_path / ".venv" / "Scripts" / "python.exe",
        root=tmp_path,
    )

    assert action == "built"
    assert calls == [[str(tmp_path / ".venv" / "Scripts" / "python.exe"), "build.py", "--dev"]]
    state = (dist / ".running-process-dev-build.json").read_text(encoding="utf-8")
    assert '"fingerprint": "new-fingerprint"' in state
    assert built_wheel.name in state


def test_repo_python_ignores_windows_venv_on_non_windows(monkeypatch, tmp_path: Path) -> None:
    windows_python = tmp_path / ".venv" / "Scripts" / "python.exe"
    windows_python.parent.mkdir(parents=True, exist_ok=True)
    windows_python.write_text("placeholder", encoding="utf-8")

    monkeypatch.setattr(dev_build, "os_name", lambda: "posix")

    assert dev_build.repo_python(tmp_path) == Path(sys.executable)


def test_repo_python_prefers_windows_venv_on_windows(monkeypatch, tmp_path: Path) -> None:
    windows_python = tmp_path / ".venv" / "Scripts" / "python.exe"
    posix_python = tmp_path / ".venv" / "bin" / "python"
    windows_python.parent.mkdir(parents=True, exist_ok=True)
    posix_python.parent.mkdir(parents=True, exist_ok=True)
    windows_python.write_text("placeholder", encoding="utf-8")
    posix_python.write_text("placeholder", encoding="utf-8")

    monkeypatch.setattr(dev_build, "os_name", lambda: "nt")

    assert dev_build.repo_python(tmp_path) == windows_python
