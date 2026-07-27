"""Deterministic CLI smoke tests (#1668).

Replaces a permanently-skipped stub::

    @unittest.skip("TODO")
    def test_imports(self) -> None:
        rtn = os.system(COMMAND)
        self.assertEqual(0, rtn)

which was unhelpful three times over: it never ran, `os.system` is
shell-dependent and cannot be given a timeout, and asserting only on the exit
status proves nothing beyond "a process launched".

These tests exercise the CLI end to end and assert on its actual output. Every
invocation is offline and timeout-bounded, and uses a recognized subcommand —
note that an *unrecognized* argv[1] is the tool-fetch path, so a bogus
subcommand would try to download something and is deliberately not used here.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# Generous relative to the ~90 ms these commands actually take, but bounded so a
# wedged binary fails the suite instead of hanging it.
CLI_TIMEOUT_SECS = 60

# `soldr version` prints exactly `soldr <semver>`.
VERSION_PATTERN = re.compile(r"^soldr \d+\.\d+\.\d+")

# A sample of the frozen built-in commands (CLAUDE.md, "Key Design Rules").
# These are contractually stable, so asserting on them is not brittle the way
# matching on help prose would be.
FROZEN_BUILTINS = ("cargo", "rustup", "rustc", "rustfmt")


def _resolve_soldr_cli() -> Path | None:
    """Locate a soldr binary to exercise.

    `SOLDR_BIN` first so CI can point at a downloaded artifact (the same
    override the Rust integration tests honour), then a repo-local build, then
    PATH.
    """
    override = os.environ.get("SOLDR_BIN")
    if override and Path(override).is_file():
        return Path(override)

    suffix = ".exe" if os.name == "nt" else ""
    for candidate in (
        REPO_ROOT / "target" / "release" / f"soldr{suffix}",
        REPO_ROOT / "target" / "debug" / f"soldr{suffix}",
    ):
        if candidate.is_file():
            return candidate

    found = shutil.which("soldr")
    return Path(found) if found else None


def _run_cli(cli: Path, *args: str) -> "subprocess.CompletedProcess[str]":
    return subprocess.run(
        [str(cli), *args],
        capture_output=True,
        text=True,
        timeout=CLI_TIMEOUT_SECS,
        check=False,
    )


class SoldrCliSmokeTest(unittest.TestCase):
    """Smoke tests over the real CLI surface."""

    def setUp(self) -> None:
        cli = _resolve_soldr_cli()
        if cli is None:
            self.skipTest(
                "no soldr binary found — set SOLDR_BIN, build with "
                "`soldr cargo build -p soldr-cli`, or put soldr on PATH"
            )
        self.cli = cli

    def test_version_reports_a_parseable_semver(self) -> None:
        """`soldr version` must print `soldr <semver>`, not merely exit 0."""
        result = _run_cli(self.cli, "version")

        self.assertEqual(
            0, result.returncode, f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        self.assertRegex(result.stdout.strip(), VERSION_PATTERN)

    def test_help_lists_the_frozen_builtin_commands(self) -> None:
        """`--help` must advertise the frozen built-ins.

        This is the assertion that would actually catch a regression: a
        dispatch table that stopped registering `cargo` would still exit 0 and
        still print a help banner.
        """
        result = _run_cli(self.cli, "--help")

        self.assertEqual(
            0, result.returncode, f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        for command in FROZEN_BUILTINS:
            self.assertIn(
                command,
                result.stdout,
                f"`{command}` missing from --help; frozen built-ins must stay listed",
            )

    def test_version_is_stable_across_invocations(self) -> None:
        """Two runs must agree — catches state leaking into version reporting."""
        first = _run_cli(self.cli, "version")
        second = _run_cli(self.cli, "version")

        self.assertEqual(first.stdout.strip(), second.stdout.strip())


if __name__ == "__main__":
    unittest.main()
