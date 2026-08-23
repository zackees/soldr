"""A terminated PEP 517 child must say so (soldr#2742).

A build killed by an external harness surfaced through uv as::

    Call to `soldr.build_editable` failed (exit code: 0xffffffff)

with nothing indicating the process had been *terminated* rather than
having failed to compile. Those need opposite responses from the reader,
so the exit code alone is not a diagnosis.
"""

from __future__ import annotations

import signal
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import load_script_module

BACKEND = Path(__file__).resolve().parents[1] / "src" / "soldr" / "__init__.py"


@pytest.fixture(scope="module")
def backend():
    return load_script_module(BACKEND, "soldr_backend_terminated")


def test_an_ordinary_nonzero_exit_is_unchanged(backend) -> None:
    """A compiler failure must not be relabelled as a termination."""
    for code in (1, 2, 101):
        assert backend._describe_pep517_exit(code) == f"exit code {code}"
        assert not backend._pep517_exit_was_termination(code)


def test_a_posix_signal_is_named(backend) -> None:
    described = backend._describe_pep517_exit(-9)
    assert "terminated" in described
    # Either the symbolic name or the number, depending on the host's
    # signal table -- both are more than the bare "-9" this replaces.
    assert "9" in described or "KILL" in described
    assert backend._pep517_exit_was_termination(-9)


def test_the_reported_windows_status_is_named(backend) -> None:
    """0xffffffff is the exact value soldr#2742 observed through uv."""
    described = backend._describe_pep517_exit(0xFFFFFFFF)
    assert "terminated" in described
    assert "0xffffffff" in described
    assert "not a compiler exit code" in described
    assert backend._pep517_exit_was_termination(0xFFFFFFFF)


def test_windows_fault_statuses_are_treated_as_termination(backend) -> None:
    """0xC0000005 (access violation) is a status, not an exit code."""
    assert backend._pep517_exit_was_termination(0xC0000005)


def test_the_hint_separates_last_known_state_from_cause(backend) -> None:
    """The reported build had plenty of output; none of it was the cause.

    The hint has to say that explicitly, or a reader takes the trailing
    queued-daemon warning as the explanation.
    """
    hint = backend._PEP517_TERMINATED_HINT
    assert "terminated rather than failing on its own" in hint
    assert "not the cause" in hint
    # And it must point at the lever that makes soldr fail first.
    assert "SOLDR_COMPILE_REPLY_TIMEOUT_SECS" in hint


def test_the_hint_names_the_deadline_the_reader_must_beat(backend) -> None:
    """soldr#2742 problem 2: the advice is only actionable with a number.

    "Set SOLDR_COMPILE_REPLY_TIMEOUT_SECS below the caller's timeout" does
    not tell the reader they are fighting a thirty-minute default, which is
    the part that makes a short-budget caller lose the race every time.
    """
    hint = backend._pep517_terminated_hint({})
    assert "SOLDR_COMPILE_REPLY_TIMEOUT_SECS" in hint
    assert "currently 1800s" in hint


def test_the_hint_reports_a_caller_supplied_deadline(backend) -> None:
    hint = backend._pep517_terminated_hint({"SOLDR_COMPILE_REPLY_TIMEOUT_SECS": "90"})
    assert "currently 90s" in hint
    assert "1800" not in hint


def test_an_unusable_deadline_value_claims_nothing(backend) -> None:
    """soldr-cli owns the fallback, so this module must not assert one.

    Naming a number here that the Rust side might not actually use would be
    worse than saying nothing: the reader would tune against a fiction.
    """
    for raw in ("not-a-number", "0", "-5", ""):
        env = {"SOLDR_COMPILE_REPLY_TIMEOUT_SECS": raw}
        hint = backend._pep517_terminated_hint(env)
        assert "SOLDR_COMPILE_REPLY_TIMEOUT_SECS" in hint
        if raw == "":
            # Empty means "unset" -- the documented default applies.
            assert "currently 1800s" in hint
        else:
            assert "currently" not in hint


def test_effective_timeout_prefers_the_callers_value(backend) -> None:
    assert backend._effective_compile_reply_timeout({}) == 1800
    assert (
        backend._effective_compile_reply_timeout(
            {"SOLDR_COMPILE_REPLY_TIMEOUT_SECS": "  120  "}
        )
        == 120
    )
    assert (
        backend._effective_compile_reply_timeout(
            {"SOLDR_COMPILE_REPLY_TIMEOUT_SECS": "junk"}
        )
        is None
    )


# soldr#2742, second half: a terminated *child* was named by #2744, but the
# incident that opened the issue was the *backend* being killed by a hook. uv
# reports only our exit status, so dying silently gives the reader a number.


def test_termination_signals_include_the_ones_a_harness_sends(backend) -> None:
    numbers = backend._backend_termination_signals()
    assert signal.SIGTERM in numbers, "SIGTERM is the POSIX convention"
    assert signal.SIGINT in numbers, "a Ctrl-C may reach us rather than the child"
    if hasattr(signal, "SIGBREAK"):
        assert signal.SIGBREAK in numbers, "SIGBREAK is what Windows harnesses send"
    # Nothing invented: every entry must be a real signal on this platform.
    for number in numbers:
        assert signal.Signals(number)


def test_the_message_names_signal_elapsed_and_command(backend) -> None:
    message = backend._backend_termination_message(
        signal.SIGTERM, ["soldr", "maturin", "pep517", "build-wheel"], {}, 412.7
    )
    assert "SIGTERM" in message
    # The reader needs to know it ran a long time, not that it failed fast.
    assert "413s" in message
    assert "`soldr maturin pep517 build-wheel`" in message


def test_the_message_carries_the_termination_hint(backend) -> None:
    """The diagnosis is the hint; this path must not invent a second one."""
    message = backend._backend_termination_message(
        signal.SIGTERM, ["soldr"], {"SOLDR_COMPILE_REPLY_TIMEOUT_SECS": "90"}, 1.0
    )
    assert "terminated rather than failing on its own" in message
    assert "currently 90s" in message


def test_handlers_are_installed_and_restored(backend) -> None:
    """A diagnostic must not leave the caller's signal disposition changed."""
    before = {n: signal.getsignal(n) for n in backend._backend_termination_signals()}

    with backend._explain_backend_termination(["soldr"], {}, 0.0):
        during = {
            n: signal.getsignal(n) for n in backend._backend_termination_signals()
        }
        assert during != before, "the handler must actually be installed"

    after = {n: signal.getsignal(n) for n in backend._backend_termination_signals()}
    assert after == before, "the previous handlers must be restored"


def test_a_refused_signal_never_breaks_the_build(backend, monkeypatch) -> None:
    """Installing the diagnostic is best-effort, always.

    `signal.signal` raises off the main thread, and a build backend is not
    guaranteed to be on it. A missing diagnostic is acceptable; a build that
    fails because we tried to add one is not.
    """

    def _refuse(_number, _handler):
        raise ValueError("signal only works in main thread")

    monkeypatch.setattr(backend.signal, "signal", _refuse)
    with backend._explain_backend_termination(["soldr"], {}, 0.0):
        pass


# The tests above prove the handler is *installed*. This one proves it
# *fires* -- which is the whole claim -- by signalling a real process and
# reading what it printed on the way out.
#
# POSIX only, and the reason is the same limitation the implementation
# documents: on Windows `os.kill(pid, SIGTERM)` is `TerminateProcess`, which
# no handler can intercept. That is not a gap in the test, it is the platform
# behaviour that makes this a best-effort diagnostic rather than a guarantee.
CHILD = """
import importlib.util, signal, sys, time
spec = importlib.util.spec_from_file_location("soldr_backend_child", sys.argv[1])
backend = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backend)
with backend._explain_backend_termination(["soldr", "maturin", "pep517"], {}, 0.0):
    sys.stdout.write("READY\\n")
    sys.stdout.flush()
    time.sleep(30)
"""


@pytest.mark.skipif(
    sys.platform == "win32",
    reason="os.kill(SIGTERM) is TerminateProcess on Windows and cannot be caught",
)
def test_a_real_sigterm_prints_the_diagnosis_before_exiting(tmp_path) -> None:
    script = tmp_path / "child.py"
    script.write_text(CHILD, encoding="utf-8")

    with subprocess.Popen(
        [sys.executable, str(script), str(BACKEND)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    ) as child:
        try:
            assert child.stdout is not None
            assert child.stdout.readline().strip() == "READY", "child never armed"
            child.send_signal(signal.SIGTERM)
            _, stderr = child.communicate(timeout=30)
        finally:
            if child.poll() is None:  # pragma: no cover - only on a hang
                child.kill()
                child.communicate()

    assert "terminated by SIGTERM" in stderr, stderr
    assert "`soldr maturin pep517`" in stderr, stderr
    assert "terminated rather than failing on its own" in stderr, stderr
    # Explaining the death must not disguise it: the exit status still has to
    # report the signal, or a caller's own error handling changes meaning.
    assert child.returncode == -signal.SIGTERM, child.returncode
