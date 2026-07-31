"""The PEP 517 failure excerpt must show the error, not the command line.

soldr#1878, separable defect: Cargo prints the *entire* compiler invocation on
its ``process didn't exit successfully`` line -- several thousand characters
that bury the actual error, and that get sliced mid-flag
(``--crate-type lib --emit=dep-inf``) when the byte-bounded failure tail cuts
through them. The excerpt is what a `pip install .` user sees, so it must spend
its budget on the diagnostic, not echo the invocation.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import load_script_module

BACKEND = Path(__file__).resolve().parents[1] / "src" / "soldr" / "__init__.py"


@pytest.fixture(scope="module")
def backend():
    return load_script_module(BACKEND, "soldr_backend_excerpt")


def _huge_command() -> str:
    flags = " ".join(f"--flag{i}=value{i}" for i in range(200))
    return (
        "soldr 'C:\\rustc.exe' --crate-name windows_link --edition=2021 "
        f"{flags} --crate-type lib --emit=dep-info,metadata,link"
    )


def test_collapse_keeps_program_crate_and_exit_code(backend):
    line = f"  process didn't exit successfully: `{_huge_command()}` (exit code: 1)"
    out = backend._collapse_process_command(line)
    assert "--flag100=value100" not in out, "flags must be elided"
    assert "--crate-name windows_link" in out, "crate name identifies the unit"
    assert "(exit code: 1)" in out, "the exit code is the actionable part"
    assert "args elided" in out
    assert len(out) < 200, f"still too long: {out!r}"


def test_collapse_handles_crate_name_equals_form(backend):
    cmd = "soldr rustc --crate-name=foo " + " ".join(f"-C opt{i}" for i in range(200))
    line = f"process didn't exit successfully: `{cmd}` (exit code: 101)"
    out = backend._collapse_process_command(line)
    assert "--crate-name=foo" in out
    assert "(exit code: 101)" in out
    assert "opt150" not in out


def test_collapse_leaves_a_short_command_untouched(backend):
    line = "process didn't exit successfully: `soldr rustc --version` (exit code: 1)"
    assert backend._collapse_process_command(line) == line


def test_collapse_ignores_unrelated_lines(backend):
    for line in (
        "error[E0308]: mismatched types",
        "Caused by:",
        "   Compiling foo v1.0",
    ):
        assert backend._collapse_process_command(line) == line


def test_cap_truncates_only_overlong_lines(backend):
    short = "error: something went wrong"
    assert backend._cap_excerpt_line(short) == short
    long = "x" * (backend._EXCERPT_LINE_CAP + 50)
    capped = backend._cap_excerpt_line(long)
    assert len(capped) <= backend._EXCERPT_LINE_CAP + len(" … (line truncated)")
    assert capped.endswith("(line truncated)")


def test_excerpt_shows_the_error_not_the_invocation(backend):
    stderr = (
        "   Compiling windows-link v0.1.3\n"
        "error: could not compile `windows-link` (lib)\n\n"
        "Caused by:\n"
        f"  process didn't exit successfully: `{_huge_command()}` (exit code: 1)\n"
    )
    excerpt = backend._pep517_failure_excerpt("", stderr)
    assert "could not compile `windows-link`" in excerpt
    assert "(exit code: 1)" in excerpt
    assert "--flag100=value100" not in excerpt
    # The whole point: the excerpt is now compact, not a command-line dump.
    assert len(excerpt) < 400, f"excerpt is still bloated ({len(excerpt)} chars)"


def test_excerpt_preserves_real_compiler_diagnostics(backend):
    # A rendered compiler-message must pass through verbatim -- the collapse
    # only touches Cargo's own bookkeeping lines, never the diagnostic.
    diag = json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "rendered": "error[E0308]: mismatched types\n  --> src/lib.rs:3:5\n"
            },
        }
    )
    excerpt = backend._pep517_failure_excerpt(
        "", diag + "\nerror: could not compile `foo`\n"
    )
    assert "error[E0308]: mismatched types" in excerpt
    assert "--> src/lib.rs:3:5" in excerpt


def test_excerpt_caps_a_mid_command_tail_fragment(backend):
    # When the byte-bounded tail starts mid-invocation, the first "line" is a
    # bare flag fragment with no diagnostic. It must not swamp the excerpt.
    fragment = "--crate-type lib --emit=dep-info " + " ".join(
        f"--flag{i}" for i in range(300)
    )
    stderr = f"{fragment}\nerror: could not compile `foo` (lib)\n"
    excerpt = backend._pep517_failure_excerpt("", stderr)
    assert "could not compile `foo`" in excerpt
    for out_line in excerpt.splitlines():
        assert len(out_line) <= backend._EXCERPT_LINE_CAP + len(" … (line truncated)")
