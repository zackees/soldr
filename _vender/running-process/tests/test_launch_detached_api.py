from __future__ import annotations

from pathlib import Path

import pytest

import running_process.launch as launch_module
from running_process import DetachedProcess, launch_detached


def test_launch_detached_returns_typed_metadata(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    calls: dict[str, object] = {}

    def fake_native_launch_detached(
        command: str,
        *,
        cwd: str | None = None,
        env: dict[str, str] | None = None,
        originator: str | None = None,
    ) -> tuple[int, float, str, str | None, str | None, str]:
        calls.update(
            {
                "command": command,
                "cwd": cwd,
                "env": env,
                "originator": originator,
            }
        )
        return (1234, 99.5, command, cwd, originator, "detached")

    monkeypatch.setattr(
        launch_module,
        "_native_launch_detached",
        fake_native_launch_detached,
    )

    result = launch_detached(
        " echo hello ",
        cwd=tmp_path,
        env={"RP_TEST": "ok"},
        originator="test:launch",
    )

    assert result == DetachedProcess(
        pid=1234,
        created_at=99.5,
        command="echo hello",
        cwd=str(tmp_path),
        originator="test:launch",
        containment="detached",
    )
    assert calls == {
        "command": "echo hello",
        "cwd": str(tmp_path),
        "env": {"RP_TEST": "ok"},
        "originator": "test:launch",
    }


def test_launch_detached_rejects_empty_command() -> None:
    with pytest.raises(ValueError, match="command must not be empty"):
        launch_detached("   ")


def test_launch_detached_rejects_non_string_env_value() -> None:
    with pytest.raises(TypeError, match="env must be a mapping of str to str"):
        launch_detached("echo hello", env={"COUNT": 1})  # type: ignore[dict-item]


def test_launch_detached_rejects_non_string_originator() -> None:
    with pytest.raises(TypeError, match="originator must be a string"):
        launch_detached("echo hello", originator=123)  # type: ignore[arg-type]
