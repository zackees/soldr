#!/usr/bin/env python3
"""Run Bosn's workspace tests after handing the cache route to source Soldr.

The published Soldr bootstrap exists only to compile the checkout.  Source
integration tests execute ``target/debug/soldr`` and must not inherit the
bootstrap daemon route: their wire protocol may legitimately differ.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Step:
    argv: list[str]
    env: dict[str, str]


def container_path(path: Path) -> str:
    """Render a Linux container path even when the contract test runs on Windows."""
    return path.as_posix()


def workspace_test_plan(*, target: Path, bootstrap: Path) -> list[Step]:
    """Return the ordered bootstrap-to-source validation handoff."""
    source = target / "debug" / "soldr"
    source_text = container_path(source)
    base_env = {"CARGO_TARGET_DIR": container_path(target)}
    return [
        Step(
            [
                container_path(bootstrap),
                "cargo",
                "build",
                "-p",
                "soldr-cli",
                "--bin",
                "soldr",
            ],
            base_env,
        ),
        Step(
            [
                container_path(bootstrap),
                "cache",
                "shutdown",
                "--shutdown-timeout-seconds",
                "30",
            ],
            {},
        ),
        Step([container_path(bootstrap), "broker", "remove"], {}),
        Step([source_text, "daemon", "start"], {}),
        Step(
            [source_text, "cargo", "test", "--workspace"],
            {**base_env, "SOLDR_RUSTC_WRAPPER": source_text},
        ),
        Step(
            [source_text, "cache", "shutdown", "--shutdown-timeout-seconds", "30"],
            {},
        ),
        Step([source_text, "broker", "remove"], {}),
    ]


def run_step(step: Step, *, repo: Path) -> None:
    env = os.environ.copy()
    env.update(step.env)
    subprocess.run(step.argv, cwd=repo, env=env, check=True)


def cleanup(steps: list[Step], *, repo: Path, preserve_primary: bool) -> None:
    """Run all source-route cleanup steps without hiding a test failure."""
    failures: list[subprocess.CalledProcessError | OSError] = []
    for step in steps:
        try:
            run_step(step, repo=repo)
        except (subprocess.CalledProcessError, OSError) as error:
            failures.append(error)
            print(f"cleanup failed: {' '.join(step.argv)}", file=sys.stderr)
    if failures and not preserve_primary:
        raise failures[0]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("/repo"))
    parser.add_argument("--target", type=Path, default=Path("/target"))
    parser.add_argument(
        "--bootstrap", type=Path, default=Path("/opt/soldr-bootstrap/bin/soldr")
    )
    args = parser.parse_args(argv)

    plan = workspace_test_plan(target=args.target, bootstrap=args.bootstrap)
    setup, source_start, validation, teardown = plan[:3], plan[3], plan[4], plan[5:]
    for step in setup:
        run_step(step, repo=args.repo.resolve())
    try:
        run_step(source_start, repo=args.repo.resolve())
        run_step(validation, repo=args.repo.resolve())
    except BaseException:
        cleanup(teardown, repo=args.repo.resolve(), preserve_primary=True)
        raise
    cleanup(teardown, repo=args.repo.resolve(), preserve_primary=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
