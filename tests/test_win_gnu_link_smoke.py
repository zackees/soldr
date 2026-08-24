"""The win-gnu smoke must find the binary the build step just produced.

Two bugs, one after the other, both of which made the lane fail while the build
above it succeeded.

**soldr#2813** — the smoke runs soldr with `cwd` set to a throwaway fixture
crate, and the workflow passed a relative `--soldr ./target/debug/soldr`, so the
path resolved against the temporary directory:

    FileNotFoundError: [Errno 2] No such file or directory: './target/debug/soldr'

**Then the windows-host lane still failed**, because `target/debug/` is not
where cargo wrote it there. `soldr cargo build` injects `CARGO_BUILD_TARGET` on
Windows and nowhere else, so cargo produces
`target/<triple>/debug/soldr.exe` on Windows and `target/debug/soldr` on Linux.
The workflow now reads the path out of cargo's own JSON rather than assuming
either shape.

The lane is scheduled-only, so nothing surfaced any of this on a PR. These tests
pin the resolution and the diagnostics; the cross-build itself needs a Windows
toolchain and is not exercised.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "win_gnu_link_smoke.py"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "win-gnu-smoke.yml"


@pytest.fixture(scope="module")
def smoke():
    return load_script_module(SCRIPT, "win_gnu_link_smoke")


# ------------------------- resolving a relative --soldr -------------------------


def test_a_relative_soldr_resolves_against_the_invocation_cwd(
    smoke, tmp_path, monkeypatch
):
    """soldr#2813, directly: a relative path must mean the checkout's copy.

    `cmd_smoke` hands `cwd=<fixture>` to the child, so resolution has to happen
    while the invocation cwd is still current.
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

    The original failure was a raw traceback ending in `FileNotFoundError`,
    which reads as a broken script rather than a missing build artifact.
    """
    monkeypatch.chdir(tmp_path)
    args = smoke.build_parser().parse_args(["smoke", "--soldr", "./target/debug/soldr"])

    assert smoke.cmd_smoke(args) == 1
    err = capsys.readouterr().err
    assert "soldr binary not found" in err
    assert "./target/debug/soldr" in err, "must echo what was passed"
    assert str(tmp_path) in err, "must say what it was resolved against"


# ---------------------- locating what cargo actually wrote ----------------------


def artifact_record(executable: str) -> str:
    return json.dumps(
        {
            "reason": "compiler-artifact",
            "target": {"name": "soldr"},
            "executable": executable,
        }
    )


def test_locate_reads_the_windows_triple_subdirectory(smoke, tmp_path):
    """The shape that kept the windows-host lane failing after soldr#2813.

    `soldr cargo build` injects `CARGO_BUILD_TARGET` on Windows only, so cargo
    writes `target/<triple>/debug/soldr.exe` there. The workflow hardcoded the
    Linux shape and never found the binary it had just built.
    """
    log = tmp_path / "build.json"
    windows_path = (
        "D:\\a\\soldr\\soldr\\target\\x86_64-pc-windows-msvc\\debug\\soldr.exe"
    )
    log.write_text(artifact_record(windows_path) + "\n", encoding="utf-8")
    assert smoke.locate_executable(str(log), "soldr") == windows_path


def test_locate_reads_the_linux_shape_too(smoke, tmp_path):
    log = tmp_path / "build.json"
    linux_path = "/home/runner/work/soldr/soldr/target/debug/soldr"
    log.write_text(artifact_record(linux_path) + "\n", encoding="utf-8")
    assert smoke.locate_executable(str(log), "soldr") == linux_path


def test_locate_ignores_other_binaries_and_non_artifacts(smoke, tmp_path):
    """A real build log is mostly other records and other binaries."""
    log = tmp_path / "build.json"
    log.write_text(
        "\n".join(
            [
                "   Compiling soldr-cli v0.9.3",
                json.dumps({"reason": "compiler-message", "message": {}}),
                artifact_record("/w/target/debug/soldr-daemon"),
                artifact_record("/w/target/debug/soldr"),
                json.dumps({"reason": "build-finished", "success": True}),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    assert smoke.locate_executable(str(log), "soldr") == "/w/target/debug/soldr"


def test_locate_reports_a_missing_artifact_by_name(smoke, tmp_path, capsys):
    log = tmp_path / "build.json"
    log.write_text(json.dumps({"reason": "build-finished"}) + "\n", encoding="utf-8")
    args = smoke.build_parser().parse_args(
        ["locate", "--build-log", str(log), "--name", "soldr"]
    )

    assert smoke.cmd_locate(args) == 1
    err = capsys.readouterr().err
    assert "no compiler-artifact record" in err
    assert "json-render-diagnostics" in err, "must say how to make it readable"


# ------------------------------- the workflow ---------------------------------


def test_the_workflow_asks_cargo_rather_than_hardcoding_a_path():
    """Both bugs came from the workflow assuming an output path.

    If either assumption returns, this is the test that says so.
    """
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "json-render-diagnostics" in workflow, "build must emit artifact records"
    assert "win_gnu_link_smoke.py locate" in workflow, "path must come from cargo"
    assert (
        "./target/debug/soldr" not in workflow
    ), "the hardcoded path is what broke both lanes"


# --------------------------- reporting the verdict ----------------------------


def test_the_printed_summary_is_ascii_encodable(smoke):
    """soldr#2819: the lane died printing its own success.

    The step-summary file is opened with `encoding="utf-8"`, but stdout is
    encoded with the locale codepage on Windows, so a U+2705 verdict raised

        UnicodeEncodeError: 'charmap' codec can't encode character

    *after* the cross-build had succeeded and the PE check had passed.

    cp1252 is the encoding the runner actually used; ASCII is the stricter
    property and implies it.
    """
    for ok in (True, False):
        printed = smoke.render_summary(
            "x86_64-pc-windows-gnu", "/w/wg_smoke.exe", ok, "PE32+ x86-64", marks=False
        )
        printed.encode("ascii")
        printed.encode("cp1252")


def test_the_file_summary_keeps_the_verdict_marks(smoke):
    """The marks are worth keeping where they render: GitHub's step summary."""
    passed = smoke.render_summary(
        "x86_64-pc-windows-gnu", "/w/wg_smoke.exe", True, "PE32+ x86-64", marks=True
    )
    failed = smoke.render_summary(
        "x86_64-pc-windows-gnu", "/w/wg_smoke.exe", False, "not a PE", marks=True
    )
    assert "\u2705" in passed
    assert "\u274c" in failed
    passed.encode("utf-8")


def test_both_renderings_agree_on_the_verdict(smoke):
    """The ASCII form must not quietly report the opposite outcome."""
    args = ("x86_64-pc-windows-gnu", "/w/wg_smoke.exe")
    ascii_pass = smoke.render_summary(*args, True, "PE32+ x86-64", marks=False)
    ascii_fail = smoke.render_summary(*args, False, "not a PE", marks=False)
    assert "PASS" in ascii_pass and "FAIL" not in ascii_pass
    assert "FAIL" in ascii_fail and "PASS" not in ascii_fail


def test_print_ascii_survives_non_ascii_input(smoke, capsys):
    """The property has to hold however it is called, not only when the call
    site remembers to pass the ASCII rendering.

    My first version of this test asserted `render_summary(marks=False)` was
    ASCII and that no `print(` *line* contained a mark. Flipping the call site
    to `marks=True` passed both -- the line stays ASCII while the runtime output
    does not. So the guarantee moved into the printer, where a call site cannot
    undo it.
    """
    smoke.print_ascii("verdict: \u2705 PE32+ x86-64")
    printed = capsys.readouterr().out
    printed.encode("ascii")
    printed.encode("cp1252")
    assert "PE32+ x86-64" in printed, "the message must survive, only escaped"


def test_print_ascii_leaves_ordinary_text_alone(smoke, capsys):
    smoke.print_ascii("$ soldr --no-cache build --target x86_64-pc-windows-gnu")
    assert (
        capsys.readouterr().out.strip()
        == "$ soldr --no-cache build --target x86_64-pc-windows-gnu"
    )


def test_the_smoke_prints_through_the_ascii_printer(smoke):
    """A bare `print` of the summary is the exact shape that broke the lane."""
    source = SCRIPT.read_text(encoding="utf-8")
    assert "print_ascii(render_summary(" in source, (
        "the verdict must go through print_ascii; a bare print re-opens " "soldr#2819"
    )
