"""Run a packaged target command under the target execution architecture."""

from __future__ import annotations

import argparse
import subprocess
from collections.abc import Sequence


EXECUTION_MODES = frozenset({"native", "x86_64-rosetta"})


def command_argv(execution: str, command: Sequence[str]) -> list[str]:
    """Return the exact process argv for the declared execution contract."""
    if execution not in EXECUTION_MODES:
        raise ValueError(f"unsupported target execution mode: {execution!r}")
    if not command:
        raise ValueError("target command is required")
    if execution == "x86_64-rosetta":
        return ["arch", "-x86_64", *command]
    return list(command)


def strip_remainder_delimiter(command: Sequence[str]) -> list[str]:
    """Remove argparse's one separator without corrupting command arguments."""
    values = list(command)
    return values[1:] if values[:1] == ["--"] else values


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execution", choices=sorted(EXECUTION_MODES), required=True)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = strip_remainder_delimiter(args.command)
    if args.preflight:
        command = ["/usr/bin/true"]
    try:
        argv = command_argv(args.execution, command)
    except ValueError as error:
        parser.error(str(error))
    subprocess.run(argv, check=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
