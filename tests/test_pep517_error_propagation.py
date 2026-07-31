"""The PEP 517 layer must not swallow a diagnosis it already holds (soldr#1999).

soldr#1999 rule 2: "No layer may replace a specific error with a generic one.
The PEP 517 boundary turning a named `SOLDR_LINKER` error into `No available
output` is the clearest violation."

The mechanism was narrow: everything useful was written to *our* stderr, then
`subprocess.CalledProcessError` was raised bare. Its `.output` and `.stderr`
were `None`, so a consumer that renders from the exception — pip and uv both
do — had nothing to show and reported the build as having produced no output.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest
from conftest import load_script_module

BACKEND = Path(__file__).resolve().parents[1] / "src" / "soldr" / "__init__.py"


@pytest.fixture(scope="module")
def backend():
    return load_script_module(BACKEND, "soldr_backend_under_test")


def test_a_named_error_travels_with_the_exception(backend):
    """The whole point: the specific cause must survive the boundary."""
    named = (
        "error: invalid SOLDR_LINKER value 'x' (expected one of: auto, fast, system)"
    )
    payload = backend._pep517_failure_payload(named, None, True)
    assert named in payload, (
        "a caller rendering from the exception must see the named cause, not a "
        f"generic one: {payload!r}"
    )


def test_the_log_path_travels_too(backend):
    payload = backend._pep517_failure_payload("error: boom", Path("/tmp/b.log"), True)
    assert "error: boom" in payload
    assert "/tmp/b.log" in payload.replace("\\", "/"), payload
    assert "full" in payload, "a complete relay should say so"


def test_an_incomplete_relay_is_labelled_as_such(backend):
    """Overstating completeness would make a truncated log look authoritative."""
    payload = backend._pep517_failure_payload("error: boom", Path("/tmp/b.log"), False)
    assert "possibly incomplete" in payload, payload


# soldr#1878 is *defined* by this shape: a non-zero exit carrying nothing.
# Saying so beats an exit code alone, which reads as "your code is broken".
def test_no_diagnostics_is_stated_rather_than_left_blank(backend):
    payload = backend._pep517_failure_payload("", None, True)
    assert "no diagnostics" in payload, payload
    assert "1878" in payload, "point the reader at the known issue: " + payload


def test_the_payload_is_never_empty(backend):
    """An empty payload is indistinguishable from the bug being fixed."""
    for excerpt, log, complete in (
        ("", None, True),
        ("", Path("/tmp/x.log"), False),
        ("error: boom", None, True),
    ):
        payload = backend._pep517_failure_payload(excerpt, log, complete)
        assert payload.strip(), f"empty payload for {excerpt!r}/{log}/{complete}"


def test_called_process_error_carries_output_and_stderr(backend, tmp_path, monkeypatch):
    """End to end through the real failure branch, not a mocked one.

    Asserts the exception itself carries the diagnosis — the attribute a
    rendering consumer reads.
    """
    named = "error: linking with `link.exe` failed: exit code: 1181"

    # Drive `_run_pep517_streaming` against a command that fails and prints a
    # named cause, so the excerpt builder has something real to work with.
    script = tmp_path / "boom.py"
    script.write_text(
        "import sys\n" f"sys.stderr.write({named!r} + '\\n')\n" "sys.exit(3)\n",
        encoding="utf-8",
    )
    cmd = [sys.executable, str(script)]

    with pytest.raises(subprocess.CalledProcessError) as excinfo:
        backend._run_pep517_streaming(cmd, env={})

    err = excinfo.value
    assert err.returncode == 3
    assert err.output, "output must not be None -- that is the reported bug"
    assert named in err.output, f"the named cause must survive: {err.output!r}"
    assert err.stderr and named in err.stderr, f"stderr attr too: {err.stderr!r}"
