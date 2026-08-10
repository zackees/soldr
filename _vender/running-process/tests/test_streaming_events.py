from __future__ import annotations

import os
import sys

import pytest

from running_process import (
    EOS,
    PIPE,
    ProcessOutputEvent,
    RunningProcess,
)

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_stream_iter_merges_stderr_into_stdout_by_default() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('out'); print('err', file=sys.stderr)",
        ]
    )

    events = list(process.stream_iter(timeout=5))

    assert any(event.stdout == "out" for event in events)
    assert any(event.stdout == "err" for event in events)
    assert all(event.stderr in (None, EOS) for event in events)
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 0)


def test_stream_iter_latches_exit_code_while_stderr_keeps_draining() -> None:
    child_code = (
        "import sys, time; "
        "time.sleep(0.2); "
        "print('late-stderr', file=sys.stderr, flush=True)"
    )
    script = (
        "import subprocess, sys; "
        f"subprocess.Popen([sys.executable, '-c', {child_code!r}]); "
        "sys.exit(1)"
    )
    process = RunningProcess([sys.executable, "-c", script], stderr=PIPE)

    events = list(process.stream_iter(timeout=5))

    assert any(
        event == ProcessOutputEvent(None, "late-stderr", 1) for event in events[:-1]
    )
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 1)
    assert events[-1].finished_and_drained is True


def test_stream_iter_reports_drained_streams_before_exit_code_when_needed() -> None:
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            (
                "import sys, time; "
                "sys.stdout.close(); "
                "sys.stderr.close(); "
                "time.sleep(0.2); "
                "sys.exit(7)"
            ),
        ],
        stderr=PIPE,
    )

    events = list(process.stream_iter(timeout=5))

    assert events[0].streams_drained is True
    if events[0].exit_code is None:
        assert events[0] == ProcessOutputEvent(EOS, EOS, None)
        assert events[0].finished_and_drained is False
    else:
        assert events[0] == ProcessOutputEvent(EOS, EOS, 7)
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 7)
    assert events[-1].finished_and_drained is True


def test_stream_iter_single_terminal_event_when_process_exits_without_output() -> None:
    process = RunningProcess([sys.executable, "-c", "import sys; sys.exit(4)"])

    events = list(process.stream_iter(timeout=5))

    assert events == [ProcessOutputEvent(EOS, EOS, 4)]
    assert events[0].finished_and_drained is True


def test_stream_iter_can_surface_exit_code_on_last_payload_before_terminal_event() -> (
    None
):
    process = RunningProcess(
        [
            sys.executable,
            "-c",
            "import sys; print('done', file=sys.stderr, flush=True); sys.exit(3)",
        ],
        stderr=PIPE,
    )

    events = list(process.stream_iter(timeout=5))

    assert (
        ProcessOutputEvent(None, "done", None) in events
        or ProcessOutputEvent(None, "done", 3) in events
    )
    assert events[-1] == ProcessOutputEvent(EOS, EOS, 3)


def test_stream_iter_times_out_when_no_output_and_process_has_not_exited() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "import time; time.sleep(0.5)"],
    )

    iterator = process.stream_iter(timeout=0.05)
    with pytest.raises(
        TimeoutError, match="No stdout or stderr available before timeout"
    ):
        next(iterator)

    process.wait(timeout=5)


def test_stream_iter_requires_capture_enabled() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('hello')"],
        capture=False,
    )

    with pytest.raises(NotImplementedError, match="requires capture=True"):
        next(iter(process))

    process.wait(timeout=5)


def test_stream_iter_is_not_available_for_pty_backed_processes() -> None:
    process = RunningProcess(
        [sys.executable, "-c", "print('hello from pty')"],
        use_pty=True,
    )

    with pytest.raises(NotImplementedError, match="only available for pipe-backed"):
        next(iter(process))

    process.wait(timeout=5)
