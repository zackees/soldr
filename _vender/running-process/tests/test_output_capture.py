from __future__ import annotations

import os
import sys

import pytest

from running_process import (
    EOS,
    PIPE,
    EndOfStream,
    RunningProcess,
)

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_basic_stdout_capture() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello')"])
    code = process.wait()

    assert code == 0
    assert process.start_time is not None
    assert process.end_time is not None
    assert process.duration is not None
    assert process.stdout.strip() == "hello"
    assert process.stderr == ""
    assert process.combined_output.strip() == "hello"


def test_default_capture_merges_stderr_into_stdout() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script])
    code = process.wait()

    assert code == 0
    stdout_lines = process.stdout.strip().splitlines()
    combined_lines = process.combined_output.strip().splitlines()
    assert "out" in stdout_lines
    assert "err" in stdout_lines
    assert process.stderr == ""
    assert "out" in combined_lines
    assert "err" in combined_lines


def test_split_stdout_and_stderr_capture() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)
    code = process.wait()

    assert code == 0
    assert process.stdout.strip() == "out"
    assert process.stderr.strip() == "err"
    assert "out" in process.combined_output
    assert "err" in process.combined_output


def test_invalid_utf8_replaced_by_default_for_running_process() -> None:
    script = (
        "import sys; "
        "sys.stdout.buffer.write(b'bad:\\xff\\n'); "
        "sys.stderr.buffer.write(b'err:\\xfe\\n'); "
        "sys.stdout.flush(); sys.stderr.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)
    process.wait()

    assert process.stdout == "bad:�"
    assert process.stderr == "err:�"


def test_crlf_is_normalized_but_bare_cr_is_preserved() -> None:
    script = (
        "import sys; "
        "sys.stdout.buffer.write(b'first\\r\\nsecond\\rthird\\n'); "
        "sys.stdout.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)
    process.wait()

    assert process.stdout == "first\nsecond\rthird"


def test_get_next_line_preserves_combined_stream() -> None:
    script = "import sys; print('out'); print('err', file=sys.stderr)"
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)

    seen = []
    while True:
        item = process.get_next_line(timeout=5)
        if isinstance(item, EndOfStream):
            break
        seen.append(item)

    process.wait()
    assert "out" in seen
    assert "err" in seen


def test_stream_specific_reads_and_availability() -> None:
    script = (
        "import sys, time; "
        "print('out-1'); sys.stdout.flush(); "
        "time.sleep(0.2); "
        "print('err-1', file=sys.stderr); sys.stderr.flush()"
    )
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)

    stdout_line = process.get_next_stdout_line(timeout=2)
    assert stdout_line == "out-1"
    assert process.has_pending_stdout() is False

    stderr_line = process.get_next_stderr_line(timeout=2)
    assert stderr_line == "err-1"
    assert process.has_pending_stderr() is False

    process.wait()


def test_stdout_and_stderr_stream_objects_report_availability() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "print('stdout-ready'); sys.stdout.flush(); "
                "time.sleep(0.2); "
                "print('stderr-ready', file=sys.stderr); sys.stderr.flush()"
            ),
        ],
        stderr=PIPE,
    )

    stdout_line = process.get_next_stdout_line(timeout=2)
    assert stdout_line == "stdout-ready"
    assert process.stdout_stream.available() is False

    stderr_line = process.get_next_stderr_line(timeout=2)
    assert stderr_line == "stderr-ready"
    assert process.stderr_stream.available() is False
    assert process.wait() == 0


def test_running_process_iteration_yields_stdout_stderr_and_terminal_tuple() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ],
        stderr=PIPE,
    )

    seen: list[tuple[object, object, int | None]] = []
    for stdout, stderr, exit_code in process:
        seen.append((stdout, stderr, exit_code))
        finished_and_drained = (
            (stdout is None or stdout is EOS)
            and (stderr is None or stderr is EOS)
            and exit_code is not None
        )
        if finished_and_drained:
            break

    assert any(stdout == "out" for stdout, _stderr, _code in seen)
    assert any(stderr == "err" for _stdout, stderr, _code in seen)
    assert seen[-1] == (EOS, EOS, 0)


def test_line_iter_uses_combined_stream() -> None:
    process = RunningProcess([sys.executable, "-c", "print('a'); print('b')"])
    with process.line_iter(timeout=5) as lines:
        collected = list(lines)
    process.wait()
    assert collected == ["a", "b"]


def test_get_next_line_non_blocking_returns_none_without_output() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(0.2); print('late')"]
    )
    assert process.get_next_line_non_blocking() is None
    process.wait()


def test_drain_combined_includes_stream_names() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ],
        stderr=PIPE,
    )
    process.wait()
    drained = process.drain_combined()
    assert ("stdout", "out") in drained
    assert ("stderr", "err") in drained


def test_stream_values_are_plain_text_and_streams_are_separate() -> None:
    process = RunningProcess([sys.executable, "-c", "print('hello world')"])
    process.wait()
    assert process.stdout.strip() == "hello world"
    assert "hello" in process.stdout
    assert str(process.stdout) == "hello world"
    assert process.stdout_stream.read() == "hello world"


def test_discard_captured_output_releases_pipe_history_bytes() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('alpha'); print('beta', file=sys.stderr)",
        ],
        stderr=PIPE,
    )
    process.wait()

    assert process.captured_output_bytes("stdout") == len("alpha")
    assert process.captured_output_bytes("stderr") == len("beta")
    assert process.captured_output_bytes("combined") == len("alpha") + len("beta")
    assert process.discard_captured_output("stdout") == len("alpha")
    assert process.stdout == ""
    assert process.captured_output_bytes("stdout") == 0
    assert process.stderr == "beta"
    assert process.discard_captured_output("combined") == len("alpha") + len("beta")
    assert process.combined_output == ""
    assert process.captured_output_bytes("combined") == 0


def test_running_process_binary_mode_returns_bytes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import sys; sys.stdout.buffer.write(b'abc\\xff')"],
        text=False,
    )
    process.wait()
    assert process.stdout == b"abc\xff"
