#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile


def _command_name(command: list[str]) -> str:
    return " ".join(command)


def _check_output(command: list[str]) -> str:
    return _run(command, capture_output=True, text=True).stdout


def _run(
    command: list[str], *, capture_output: bool = False, **kwargs
) -> subprocess.CompletedProcess[str]:
    try:
        if not capture_output:
            return subprocess.run(command, check=True, timeout=30, **kwargs)
        # Regular files avoid pipe-buffer and inherited-pipe EOF waits.
        kwargs.pop("text", None)
        with (
            tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stdout,
            tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stderr,
        ):
            result = subprocess.run(
                command, stdout=stdout, stderr=stderr, check=False, timeout=30, **kwargs
            )
            stdout.seek(0)
            stderr.seek(0)
            completed = subprocess.CompletedProcess(
                command, result.returncode, stdout.read(), stderr.read()
            )
        completed.check_returncode()
        return completed
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"{_command_name(command)} timed out after 30s") from exc


def _is_transient_zccache_status_failure(exc: subprocess.CalledProcessError) -> bool:
    combined = "\n".join(
        part
        for part in (
            exc.stdout,
            exc.stderr,
            exc.output,
        )
        if isinstance(part, str) and part
    ).lower()
    return "zccache status failed" in combined and "daemon not running" in combined


def main() -> None:
    binary = os.environ["SETUP_SOLDR_PATH"]
    output_path = os.environ["GITHUB_OUTPUT"]

    version_json = _check_output([binary, "version", "--json"])
    payload = json.loads(version_json)

    with open(output_path, "a", encoding="utf-8") as fh:
        fh.write(f"soldr_version={payload['soldr_version']}\n")

    _run(["cargo", "--version"])
    _run(["rustc", "--version"])
    try:
        _run(["soldr", "status", "--json"], capture_output=True, text=True)
    except subprocess.CalledProcessError as exc:
        if not _is_transient_zccache_status_failure(exc):
            raise


if __name__ == "__main__":
    try:
        main()
    except RuntimeError as exc:
        sys.exit(str(exc))
    except subprocess.CalledProcessError as exc:
        # Captured probes must replay their diagnostics at the action boundary.
        for diagnostic in (exc.stdout, exc.stderr):
            if diagnostic:
                sys.stderr.write(diagnostic)
        sys.exit(exc.returncode)
