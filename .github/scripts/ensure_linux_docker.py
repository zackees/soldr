#!/usr/bin/env python3
"""Switch Docker Desktop to Linux containers and wait for its engine to return."""

from __future__ import annotations

import argparse
import os
import subprocess
import time
from pathlib import Path


def docker_info_command() -> list[str]:
    """Return the command that reports Docker's active server OS."""
    return ["docker", "info", "--format", "{{.OSType}}"]


def docker_cli_path(program_files: str | None) -> Path:
    """Resolve Docker Desktop's engine-switch executable on Windows."""
    if not program_files:
        raise ValueError("ProgramFiles is not set; cannot locate DockerCli.exe")
    return Path(program_files) / "Docker" / "Docker" / "DockerCli.exe"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout-seconds", type=int, default=120)
    args = parser.parse_args()
    cli = docker_cli_path(os.environ.get("ProgramFiles"))
    if not cli.is_file():
        parser.error(f"Docker Desktop CLI not found: {cli}")
    subprocess.run([str(cli), "-SwitchLinuxEngine"], check=True)
    deadline = time.monotonic() + args.timeout_seconds
    while time.monotonic() < deadline:
        result = subprocess.run(
            docker_info_command(), text=True, capture_output=True, check=False
        )
        if result.returncode == 0 and result.stdout.strip().lower() == "linux":
            print("Docker Linux engine is ready", flush=True)
            return 0
        time.sleep(2)
    parser.error(f"Docker did not report a Linux engine within {args.timeout_seconds}s")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
