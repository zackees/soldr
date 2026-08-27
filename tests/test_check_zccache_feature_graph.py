from __future__ import annotations

import subprocess
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).resolve().parents[1] / "ci" / "check_zccache_feature_graph.py"


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_zccache_feature_graph")


def completed(returncode: int, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=["soldr", "cargo", "tree"],
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
    )


def test_normal_tree_without_sevenz_passes(guard) -> None:
    with patch.object(
        guard,
        "_run",
        return_value=completed(0, "soldr-cli v0.9.10\n`-- zccache v1.13.13\n"),
    ):
        assert not guard._check_no_normal_sevenz("soldr")


def test_normal_tree_with_sevenz_fails(guard) -> None:
    with patch.object(
        guard,
        "_run",
        return_value=completed(0, "`-- sevenz-rust v0.6.1\n"),
    ):
        failures = guard._check_no_normal_sevenz("soldr")

    assert len(failures) == 1
    assert "normal dependency path" in failures[0]


def test_normal_tree_command_error_fails_closed(guard) -> None:
    with patch.object(
        guard,
        "_run",
        return_value=completed(101, stderr="metadata failed"),
    ):
        failures = guard._check_no_normal_sevenz("soldr")

    assert len(failures) == 1
    assert "could not inspect normal dependency tree" in failures[0]
    assert "metadata failed" in failures[0]
