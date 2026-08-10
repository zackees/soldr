from __future__ import annotations

import dataclasses
import os
from collections.abc import Mapping
from pathlib import Path

from running_process._native import native_launch_detached as _native_launch_detached


@dataclasses.dataclass(frozen=True)
class DetachedProcess:
    """Metadata for a daemon-tracked detached process."""

    pid: int
    created_at: float
    command: str
    cwd: str | None
    originator: str | None
    containment: str


def _normalize_env(env: Mapping[str, str] | None) -> dict[str, str] | None:
    if env is None:
        return None
    if not isinstance(env, Mapping):
        raise TypeError("env must be a mapping of str to str")

    normalized: dict[str, str] = {}
    for key, value in env.items():
        if not isinstance(key, str) or not isinstance(value, str):
            raise TypeError("env must be a mapping of str to str")
        normalized[key] = value
    return normalized


def launch_detached(
    command: str,
    *,
    cwd: str | Path | None = None,
    env: Mapping[str, str] | None = None,
    originator: str | None = None,
) -> DetachedProcess:
    """Launch a daemon-tracked detached shell command and return its metadata."""
    if not isinstance(command, str):
        raise TypeError("command must be a string")

    command = command.strip()
    if not command:
        raise ValueError("command must not be empty")

    if originator is not None and not isinstance(originator, str):
        raise TypeError("originator must be a string")

    cwd_text = os.fspath(cwd) if cwd is not None else None
    pid, created_at, launched_command, launched_cwd, launched_originator, containment = (
        _native_launch_detached(
            command,
            cwd=cwd_text,
            env=_normalize_env(env),
            originator=originator,
        )
    )
    return DetachedProcess(
        pid=pid,
        created_at=created_at,
        command=launched_command,
        cwd=launched_cwd,
        originator=launched_originator,
        containment=containment,
    )


__all__ = ["DetachedProcess", "launch_detached"]
