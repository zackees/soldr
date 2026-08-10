from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from ci.dev_build import repo_python
from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
# Local default. A cold `cargo fmt --all` or `clippy --workspace` does not
# finish in 10s, so the old value made `./lint` fail on the developer's own
# machine while CI (which sets RUNNING_PROCESS_LINT_COMMAND_TIMEOUT_SECONDS
# to 300 in ci-preflight.yml) passed. Set the env var to empty or 0 to
# disable supervision entirely.
DEFAULT_COMMAND_TIMEOUT_SECONDS = 300.0
COMMAND_TIMEOUT_ENV = "RUNNING_PROCESS_LINT_COMMAND_TIMEOUT_SECONDS"


def run(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    return subprocess.run(cmd, cwd=ROOT, env=clean_env()).returncode


def load_env_helpers():
    from ci.env import activate, clean_env

    return activate, clean_env


def command_timeout_seconds() -> float | None:
    configured = os.environ.get(COMMAND_TIMEOUT_ENV)
    if configured is None:
        return DEFAULT_COMMAND_TIMEOUT_SECONDS
    configured = configured.strip()
    if not configured:
        return None
    timeout = float(configured)
    if timeout <= 0:
        return None
    return timeout


def supervised_command(python: Path, *command: str) -> list[str]:
    timeout = command_timeout_seconds()
    if timeout is None:
        return list(command)
    return [
        str(python),
        "-m",
        "running_process.cli",
        "--timeout",
        str(timeout),
        "--",
        *command,
    ]


def main() -> int:
    activate, _ = load_env_helpers()
    activate()
    python = repo_python()
    if run(supervised_command(python, str(python), "-m", "ci.version_check")) != 0:
        return 1
    if run(supervised_command(python, str(python), "-m", "ci.spawn_path_guard")) != 0:
        return 1
    if (
        run(supervised_command(python, str(python), "-m", "ci.docker_manifest_guard"))
        != 0
    ):
        return 1
    if run(supervised_command(python, str(python), "-m", "ci.jemalloc_guard")) != 0:
        return 1
    if (
        run(supervised_command(python, str(python), "-m", "ci.cross_compiler_guard"))
        != 0
    ):
        return 1
    # `--check`, not write mode. Plain `cargo fmt --all` reformats the tree
    # and exits 0 whether or not it changed anything, so it can essentially
    # never fail: CI would reformat its checkout, throw it away, and report
    # green while the committed tree drifted. It also silently rewrote
    # contributors' working trees, sweeping unrelated files into their
    # commits. Verifying instead of mutating fixes both. See #694.
    if (
        run(supervised_command(python, *cargo_command("fmt", "--all", "--", "--check")))
        != 0
    ):
        print(
            "lint: formatting drift detected. Run `soldr cargo fmt --all` and "
            "commit the result.",
            file=sys.stderr,
            flush=True,
        )
        return 1
    if (
        run(
            supervised_command(
                python,
                *cargo_command(
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ),
            )
        )
        != 0
    ):
        return 1
    if (
        run(
            supervised_command(
                python,
                str(python),
                "-m",
                "ruff",
                "check",
                "--fix",
                "src",
                "tests",
                "ci",
            )
        )
        != 0
    ):
        return 1
    # KeyboardInterrupt discipline (KBI001/KBI002). Scoped to `src`: the rule
    # is about library code, where an interrupt on a worker thread has to
    # reach the main thread to be seen at all. Test helpers that swallow
    # exceptions in an availability probe are not that.
    if (
        run(
            supervised_command(
                python,
                str(python),
                "-m",
                "ci.lint_python.keyboard_interrupt_checker",
                "src",
                "--exclude",
                ".venv",
                "venv",
                "dist",
                ".build",
            )
        )
        != 0
    ):
        print(
            "lint: KeyboardInterrupt handling violations. See CLAUDE.md "
            "'Keyboard interrupts'; suppress a deliberate exception with "
            "`# noqa: KBI002` and a comment saying why.",
            file=sys.stderr,
            flush=True,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
