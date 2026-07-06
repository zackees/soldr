"""Opt-in Docker regression lock for full nextest archive cacheability."""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]


def _docker_available() -> bool:
    if shutil.which("docker") is None:
        return False
    try:
        result = subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


@pytest.mark.cacheability_integration
def test_full_nextest_archive_warm_cacheability() -> None:
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    script = REPO_ROOT / "ci" / "assert_nextest_archive_cacheability.py"
    result = subprocess.run(
        [sys.executable, str(script)],
        cwd=REPO_ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=3600,
        check=False,
    )
    assert result.returncode == 0, result.stdout
    assert "CACHEABILITY_OK warm run had hits and zero misses" in result.stdout
