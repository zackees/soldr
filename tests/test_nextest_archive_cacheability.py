"""Opt-in Docker regression lock for full nextest archive cacheability."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest
from conftest import docker_available

REPO_ROOT = Path(__file__).resolve().parents[1]


@pytest.mark.cacheability_integration
def test_full_nextest_archive_warm_cacheability() -> None:
    if not docker_available():
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
