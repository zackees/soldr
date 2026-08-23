"""A terminated PEP 517 child must say so (soldr#2742).

A build killed by an external harness surfaced through uv as::

    Call to `soldr.build_editable` failed (exit code: 0xffffffff)

with nothing indicating the process had been *terminated* rather than
having failed to compile. Those need opposite responses from the reader,
so the exit code alone is not a diagnosis.
"""

from __future__ import annotations

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
