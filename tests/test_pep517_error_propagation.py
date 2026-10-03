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
        f"import sys\nsys.stderr.write({named!r} + '\\n')\nsys.exit(3)\n",
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


# soldr#3401: the hook must end with soldr's own summary, not a Python
# traceback. `SystemExit(message)` prints only the message.
def test_a_failing_build_ends_the_hook_with_a_message_not_a_traceback(
    backend, monkeypatch
):
    named = "error: linking with `cc` failed: exit status: 1"

    def failing(cmd, env):
        raise subprocess.CalledProcessError(
            3,
            cmd,
            output=backend._pep517_failure_payload(named, Path("/tmp/b.log"), True),
            stderr=named,
        )

    monkeypatch.setattr(backend, "_prep_env", lambda *a, **k: {})
    monkeypatch.setattr(backend, "_stats_mode", lambda env: "off")
    monkeypatch.setattr(backend, "_run_pep517_streaming", failing)

    with pytest.raises(SystemExit) as excinfo:
        backend._maturin_pep517("build-wheel")

    message = str(excinfo.value.code)
    assert "exit status 3" in message, message
    assert named in message, "the named cause must survive the boundary: " + message
    assert "/tmp/b.log" in message.replace("\\", "/"), message
    assert excinfo.value.__cause__ is None, "no chained CalledProcessError traceback"
    assert excinfo.value.__suppress_context__


def test_a_failure_message_without_a_payload_still_names_the_exit_status(backend):
    exc = subprocess.CalledProcessError(9, ["soldr"], output=None)
    assert backend._pep517_failure_message(exc) == (
        "soldr: the PEP 517 build failed (exit status 9)."
    )


def test_root_owner_refusal_retries_without_the_cache_wrapper(backend, monkeypatch):
    calls = []

    def build(cmd, env):
        calls.append(env.copy())
        if len(calls) == 1:
            raise subprocess.CalledProcessError(
                1,
                cmd,
                output="broker refused the daemon route: soldr root ownership is busy",
            )

    monkeypatch.delenv("RUSTC_WRAPPER", raising=False)
    monkeypatch.setattr(
        backend, "_prep_env", lambda *a, **k: {"RUSTC_WRAPPER": "soldr"}
    )
    monkeypatch.setattr(backend, "_stats_mode", lambda env: "off")
    monkeypatch.setattr(backend, "_run_pep517_streaming", build)

    backend._maturin_pep517("build-wheel")

    assert [env["RUSTC_WRAPPER"] for env in calls] == ["soldr", ""]


def test_other_refusals_keep_the_original_failure(backend, monkeypatch):
    calls = []

    def build(cmd, env):
        calls.append(env.copy())
        raise subprocess.CalledProcessError(
            1,
            cmd,
            output="broker refused the daemon route: permission denied",
        )

    monkeypatch.delenv("RUSTC_WRAPPER", raising=False)
    monkeypatch.setattr(
        backend, "_prep_env", lambda *a, **k: {"RUSTC_WRAPPER": "soldr"}
    )
    monkeypatch.setattr(backend, "_stats_mode", lambda env: "off")
    monkeypatch.setattr(backend, "_run_pep517_streaming", build)

    with pytest.raises(SystemExit):
        backend._maturin_pep517("build-wheel")

    assert len(calls) == 1


def test_explicit_wrapper_is_not_disabled_on_root_conflict(backend, monkeypatch):
    calls = []

    def build(cmd, env):
        calls.append(env.copy())
        raise subprocess.CalledProcessError(
            1,
            cmd,
            output="broker refused the daemon route: soldr root ownership is busy",
        )

    monkeypatch.setenv("RUSTC_WRAPPER", "soldr")
    monkeypatch.setattr(
        backend, "_prep_env", lambda *a, **k: {"RUSTC_WRAPPER": "soldr"}
    )
    monkeypatch.setattr(backend, "_stats_mode", lambda env: "off")
    monkeypatch.setattr(backend, "_run_pep517_streaming", build)

    with pytest.raises(SystemExit):
        backend._maturin_pep517("build-wheel")

    assert len(calls) == 1
