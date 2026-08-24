"""The win-gnu smoke must find the binary the build step just produced.

Every scheduled run of this lane on record — 2026-08-10, 08-17, 08-24 — died
with:

    FileNotFoundError: [Errno 2] No such file or directory: './target/debug/soldr'

directly after a build step that reported `Finished dev profile in 1m 39s`. The
smoke runs soldr with `cwd` set to a throwaway fixture crate, and the workflow
passes `--soldr ./target/debug/soldr`, so the relative path was resolved against
the temporary directory rather than the checkout.

The lane is scheduled-only, so nothing surfaced it on a PR and three weeks of
failures went unread. These tests pin the resolution and the diagnostic; the
cross-build itself needs a Windows toolchain and is not exercised here.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "win_gnu_link_smoke.py"
)


@pytest.fixture(scope="module")
def smoke():
    return load_script_module(SCRIPT, "win_gnu_link_smoke")


def test_a_relative_soldr_resolves_against_the_invocation_cwd(
    smoke, tmp_path, monkeypatch
):
    """The bug, directly: `./target/debug/soldr` must mean the checkout's copy.

    `cmd_smoke` chdirs nothing itself but hands `cwd=<fixture>` to the child, so
    resolution has to happen while the invocation cwd is still current.
    """
    checkout = tmp_path / "checkout"
    (checkout / "target" / "debug").mkdir(parents=True)
    binary = checkout / "target" / "debug" / "soldr"
    binary.write_text("#!/bin/sh\n", encoding="utf-8")

    monkeypatch.chdir(checkout)
    resolved = smoke.resolve_soldr("./target/debug/soldr")

    assert os.path.isabs(resolved), f"must be absolute, got {resolved!r}"
    assert Path(resolved) == binary
    # And it must still name that file from somewhere else entirely -- which is
    # the property the fixture cwd broke.
    monkeypatch.chdir(tmp_path)
    assert os.path.isfile(resolved)


def test_an_absolute_soldr_is_left_alone(smoke, tmp_path):
    binary = tmp_path / "soldr"
    binary.write_text("", encoding="utf-8")
    assert Path(smoke.resolve_soldr(str(binary))) == binary


def test_a_missing_binary_is_named_not_raised(smoke, tmp_path, monkeypatch, capsys):
    """A scheduled lane's reader is days late and has no context.

    The old failure was a raw Python traceback ending in `FileNotFoundError`,
    which reads as a broken script rather than a missing build artifact.
    """
    monkeypatch.chdir(tmp_path)
    args = smoke.build_parser().parse_args(["smoke", "--soldr", "./target/debug/soldr"])

    assert smoke.cmd_smoke(args) == 1
    err = capsys.readouterr().err
    assert "soldr binary not found" in err
    assert "./target/debug/soldr" in err, "must echo what was passed"
    assert str(tmp_path) in err, "must say what it was resolved against"


def test_the_workflow_still_passes_a_relative_path(smoke):
    """If the workflow ever switches to an absolute path, this test is the
    place that says the resolution is no longer load-bearing -- rather than the
    fix quietly becoming dead code."""
    workflow = (
        Path(__file__).resolve().parents[1]
        / ".github"
        / "workflows"
        / "win-gnu-smoke.yml"
    ).read_text(encoding="utf-8")
    assert "./target/debug/soldr" in workflow
