"""Execution-architecture contract for packaged target commands (#2968)."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest


SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "run_target_command.py"
SPEC = importlib.util.spec_from_file_location("run_target_command", SCRIPT)
assert SPEC and SPEC.loader
runner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = runner
SPEC.loader.exec_module(runner)


def test_native_execution_leaves_packaged_target_command_unchanged() -> None:
    assert runner.command_argv("native", ["/artifact/soldr", "--version"]) == [
        "/artifact/soldr",
        "--version",
    ]


def test_rosetta_execution_prefixes_every_packaged_target_command() -> None:
    assert runner.command_argv("x86_64-rosetta", ["/artifact/soldr", "--version"]) == [
        "arch",
        "-x86_64",
        "/artifact/soldr",
        "--version",
    ]


def test_only_argparse_leading_delimiter_is_removed_from_target_command() -> None:
    assert runner.strip_remainder_delimiter(["--", "/artifact/soldr", "--", "--json"]) == [
        "/artifact/soldr",
        "--",
        "--json",
    ]


@pytest.mark.parametrize("execution", ["unknown", "arm64-rosetta"])
def test_unknown_execution_mode_fails_explicitly(execution: str) -> None:
    with pytest.raises(ValueError, match="unsupported target execution mode"):
        runner.command_argv(execution, ["/artifact/soldr"])
