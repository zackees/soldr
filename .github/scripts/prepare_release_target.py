#!/usr/bin/env python3
"""Retry Soldr release-target preparation after transient asset failures.

setup-soldr installs the pinned Soldr binary before it runs ``soldr prepare``.
If a large catalogue asset is corrupted in transit, the action step can fail
after installation.  This helper reuses that exact binary, isolates each
attempt's GitHub environment records, and publishes only a successful set.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import tempfile
import time
from pathlib import Path


def prepare_target(
    *, soldr: str, target: str, github_env: Path, attempts: int = 3
) -> None:
    if attempts < 1:
        raise ValueError("attempts must be at least one")

    last_returncode = 1
    for attempt in range(1, attempts + 1):
        with tempfile.TemporaryDirectory(prefix="soldr-release-prepare-") as raw:
            attempt_env = Path(raw, "github-env")
            completed = subprocess.run(
                [
                    soldr,
                    "prepare",
                    "--target",
                    target,
                    "--github-env",
                    str(attempt_env),
                ],
                check=False,
            )
            last_returncode = completed.returncode
            if completed.returncode == 0:
                payload = attempt_env.read_bytes() if attempt_env.is_file() else b""
                if payload and not payload.endswith(b"\n"):
                    payload += b"\n"
                with github_env.open("ab") as destination:
                    destination.write(payload)
                print(
                    f"release target preparation succeeded on attempt {attempt}/{attempts}"
                )
                return

        if attempt < attempts:
            delay = 5 * attempt
            print(
                f"release target preparation failed on attempt {attempt}/{attempts}; "
                f"retrying in {delay}s"
            )
            time.sleep(delay)

    raise RuntimeError(
        f"soldr prepare failed for {target} after {attempts} attempts "
        f"(last exit code {last_returncode})"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--github-env", required=True, type=Path)
    parser.add_argument("--attempts", type=int, default=3)
    parser.add_argument("--soldr", default=shutil.which("soldr"))
    args = parser.parse_args()
    if not args.soldr:
        parser.error("the failed setup-soldr step did not leave soldr on PATH")
    prepare_target(
        soldr=args.soldr,
        target=args.target,
        github_env=args.github_env,
        attempts=args.attempts,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
