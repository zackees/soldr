"""Per-line elapsed-second stamping in the PEP 517 relay (soldr#1802 §4).

The Rust front door (`cargo_front_door/timestamp_tee.rs`) already stamps
cargo's output; these tests pin the Python port to the *same* behavior so a
pip/uv build log and a `soldr cargo` log read identically:

- one `  12.34 ` prefix at each line start, plain text at column 0;
- both ``\n`` and ``\r`` start a line (so cargo's ``\r`` progress redraws are
  stamped), but a CRLF pair stamps once;
- ANSI color inside a line passes through untouched;
- the archived log and the failure-tail buffer see UNSTAMPED bytes;
- the child soldr is told not to stamp, so nothing is double-prefixed.
"""

from __future__ import annotations

import contextlib
import io
import os
import re
import subprocess
import sys
from pathlib import Path

import pytest
from conftest import load_script_module

BACKEND = Path(__file__).resolve().parents[1] / "src" / "soldr" / "__init__.py"

# The prefix the stamper inserts: right-aligned seconds, 2 decimals, one space.
_PREFIX_RE = re.compile(r"[ ]*\d+\.\d{2} ")


@pytest.fixture(scope="module")
def backend():
    return load_script_module(BACKEND, "soldr_backend_ts_under_test")


def _shape(text: str) -> str:
    """Replace each real prefix with a literal ``TS `` so structure is
    assertable without depending on the elapsed value."""
    return _PREFIX_RE.sub("TS ", text)


def test_each_line_gets_one_prefix(backend):
    stamper = backend._LineStamper(0.0)
    out = stamper.stamp("first\nsecond\n")
    assert _shape(out) == "TS first\nTS second\n"


def test_a_chunk_split_mid_line_does_not_double_stamp(backend):
    # The relay hands the stamper arbitrary chunk boundaries; a line split
    # across two stamp() calls must still carry exactly one prefix.
    stamper = backend._LineStamper(0.0)
    out = stamper.stamp("Compil") + stamper.stamp("ing foo\n")
    assert _shape(out) == "TS Compiling foo\n"


def test_carriage_return_starts_a_line_so_redraws_are_stamped(backend):
    stamper = backend._LineStamper(0.0)
    out = stamper.stamp("Downloading 1/9\rDownloading 2/9\r")
    assert _shape(out) == "TS Downloading 1/9\rTS Downloading 2/9\r"


def test_crlf_stamps_once_not_twice(backend):
    stamper = backend._LineStamper(0.0)
    out = stamper.stamp("line\r\nnext\r\n")
    assert _shape(out) == "TS line\r\nTS next\r\n"


def test_ansi_escapes_pass_through_untouched(backend):
    # Color lives inside the line; the prefix is plain text at column 0, so
    # the escape survives verbatim -- color preservation for free.
    green = "\x1b[32mCompiling\x1b[0m foo\n"
    stamper = backend._LineStamper(0.0)
    out = stamper.stamp(green)
    assert out.endswith(green)
    assert _shape(out) == "TS " + green


def test_a_partial_line_is_stamped_immediately(backend):
    # Streaming, not buffering: a line with no terminator yet (cargo printing
    # "Compiling foo" while rustc chews) must appear stamped now, not be held
    # back until the newline arrives.
    stamper = backend._LineStamper(0.0)
    out = stamper.stamp("Compiling foo v1.2.3")
    assert _shape(out) == "TS Compiling foo v1.2.3"
    # ...and the newline, when it comes, must not add a second prefix.
    rest = stamper.stamp("\n")
    assert rest == "\n"


def test_empty_input_is_a_noop(backend):
    stamper = backend._LineStamper(0.0)
    assert stamper.stamp("") == ""


def test_should_timestamp_defaults_on_for_ci_and_off_for_a_terminal(backend):
    assert backend._should_timestamp_pep517(None, is_terminal=False) is True
    assert backend._should_timestamp_pep517(None, is_terminal=True) is False


def test_env_override_wins_in_both_directions(backend):
    # Force ON even on a terminal...
    for on in ("1", "true", "on", "TRUE", " On "):
        assert backend._should_timestamp_pep517(on, is_terminal=True) is True
    # ...and OFF even in CI.
    for off in ("0", "false", "off", "OFF", " 0 "):
        assert backend._should_timestamp_pep517(off, is_terminal=False) is False


def test_unrecognised_env_value_falls_back_to_the_default(backend):
    assert backend._should_timestamp_pep517("yes-please", is_terminal=False) is True
    assert backend._should_timestamp_pep517("yes-please", is_terminal=True) is False


def test_anchor_line_carries_the_absolute_epoch(backend):
    # Byte-identical to Rust `epoch_anchor_line`.
    assert (
        backend._pep517_epoch_anchor_line(1_784_950_000_123) == "# t0=1784950000.123\n"
    )
    assert backend._pep517_epoch_anchor_line(1_000) == "# t0=1.000\n"
    # Sub-second millis are zero-padded so the offset math is unambiguous.
    assert (
        backend._pep517_epoch_anchor_line(1_784_950_000_007) == "# t0=1784950000.007\n"
    )


# --- end-to-end through the real relay --------------------------------------


def _relay_env(backend, tmp_path: Path, timestamps: str) -> "dict[str, str]":
    env = dict(os.environ)
    env[backend._PEP517_IDLE_TIMEOUT_ENV] = "5"
    env[backend._TIMESTAMP_LINES_ENV_VAR] = timestamps
    env["SOLDR_CACHE_DIR"] = str(tmp_path)
    return env


def test_relay_stamps_both_streams_and_anchors_stderr(backend, tmp_path):
    # End-to-end: relayed stdout AND stderr carry prefixes and stderr gets the
    # `# t0=` anchor once, when stamping is on.
    code = (
        "import sys\n"
        "sys.stderr.write('Compiling foo v1.2.3\\n'); sys.stderr.flush()\n"
        "sys.stdout.write('Building wheel\\n'); sys.stdout.flush()\n"
    )
    stdout, stderr = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        backend._run_pep517_streaming(
            [sys.executable, "-u", "-c", code], env=_relay_env(backend, tmp_path, "1")
        )

    assert "# t0=" in stderr.getvalue(), stderr.getvalue()
    assert _PREFIX_RE.search(
        stderr.getvalue()
    ), f"stderr unstamped: {stderr.getvalue()!r}"
    assert _PREFIX_RE.search(
        stdout.getvalue()
    ), f"stdout unstamped: {stdout.getvalue()!r}"


def test_the_retained_failure_log_is_never_stamped(backend, tmp_path):
    # The build log is kept only on failure and feeds the diagnostic scanner,
    # so it must hold the child's raw bytes -- no prefixes, no `# t0=` anchor --
    # even when the terminal relay is being stamped.
    code = (
        "import sys\n"
        "sys.stderr.write('error[E0277]: not Send\\n'); sys.stderr.flush()\n"
        "sys.exit(1)\n"
    )
    with (
        contextlib.redirect_stdout(io.StringIO()),
        contextlib.redirect_stderr(io.StringIO()),
    ):
        with pytest.raises(subprocess.CalledProcessError):
            backend._run_pep517_streaming(
                [sys.executable, "-u", "-c", code],
                env=_relay_env(backend, tmp_path, "1"),
            )

    logs = list((tmp_path / "logs" / "pep517").glob("*.log"))
    assert logs, "a failing build must retain its log"
    log_text = logs[0].read_text(encoding="utf-8")
    assert "error[E0277]: not Send" in log_text, log_text
    assert "# t0=" not in log_text, "the log must not carry the terminal anchor"
    assert not _PREFIX_RE.search(log_text), f"log line is prefixed: {log_text!r}"


def test_stamping_off_relays_without_a_prefix(backend, tmp_path):
    code = "import sys; sys.stdout.write('plain line\\n'); sys.stdout.flush()\n"
    stdout = io.StringIO()
    with contextlib.redirect_stdout(stdout):
        backend._run_pep517_streaming(
            [sys.executable, "-u", "-c", code], env=_relay_env(backend, tmp_path, "0")
        )
    # Newline translation (\n -> \r\n) is the child/OS, not the relay; what the
    # relay must not do is add a timestamp prefix.
    assert stdout.getvalue().replace("\r\n", "\n") == "plain line\n"
    assert not _PREFIX_RE.search(stdout.getvalue())


def test_a_broken_sink_still_fails_gracefully_with_stamping_on(backend, tmp_path):
    # The anchor line is written on the main thread; a sink that raises must
    # not turn the graceful "output relay failed" into a raw BrokenPipeError.
    class BrokenSink(io.StringIO):
        def write(self, _value: str) -> int:
            raise BrokenPipeError("capture pipe closed")

    code = (
        "import sys, time\n"
        "sys.stderr.write('diag\\n'); sys.stderr.flush()\n"
        "time.sleep(30)\n"
    )
    with contextlib.redirect_stderr(BrokenSink()):
        with pytest.raises(RuntimeError, match="output relay failed"):
            backend._run_pep517_streaming(
                [sys.executable, "-u", "-c", code],
                env=_relay_env(backend, tmp_path, "1"),
            )


def test_anchor_is_skipped_where_the_runner_already_stamps_lines(backend) -> None:
    assert backend._pep517_epoch_anchor_wanted(None)
    assert backend._pep517_epoch_anchor_wanted("")
    assert not backend._pep517_epoch_anchor_wanted("true")
