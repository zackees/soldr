#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import atexit
import faulthandler
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time
from collections import deque
from collections.abc import Callable
from pathlib import Path

from ci.env import clean_env

ROOT = Path(__file__).resolve().parent.parent

_STACK_DUMP_TIMEOUT_ENV = "RUN_LOGGED_STACK_DUMP_SECONDS"
_DEFAULT_STACK_DUMP_TIMEOUT_SECONDS = 180.0
_TAIL_LINE_LIMIT = 80
_SUMMARY_TAIL_LINE_LIMIT = 40
_PYTEST_FAILURE_PRE_CONTEXT_LINE_LIMIT = 8
_PYTEST_FAILURE_LINE_LIMIT = 80
_ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
_PYTEST_NODEID = re.compile(r"^(tests[\\/][^\s]+::[^\s]+)")
_PYTEST_FAILURE_HEADER = re.compile(r"=+\s+FAILURES\s+=+")
_PYTEST_SUMMARY_HEADER = re.compile(r"=+\s+short test summary info\s+=+")
_PYTEST_ERROR_LINE = re.compile(r"^E\s{2,}")
_PYTEST_FAILED_LINE = re.compile(r"^(FAILED\s+tests[\\/]|tests[\\/].+\s+FAILED\s+\[)")
_FAULT_PATTERNS = (
    re.compile(r"Traceback \(most recent call last\):"),
    re.compile(r"\bFAILED\b"),
    re.compile(r"\bERROR\b"),
    re.compile(r"^E\s{2,}"),
    re.compile(r"\bpanic!?\b", re.IGNORECASE),
    re.compile(r"\bfatal\b", re.IGNORECASE),
    re.compile(r"segmentation fault", re.IGNORECASE),
    re.compile(r"abnormal-exit diagnostics", re.IGNORECASE),
    re.compile(r"timeout diagnostics", re.IGNORECASE),
    re.compile(r"spawn-path guard failed", re.IGNORECASE),
    re.compile(r"requires review and allowlisting", re.IGNORECASE),
)


def _write_console_line(line: str) -> None:
    try:
        sys.stdout.write(line)
    except UnicodeEncodeError:
        encoding = sys.stdout.encoding or "utf-8"
        rendered = line.encode(encoding, errors="replace")
        if hasattr(sys.stdout, "buffer"):
            sys.stdout.buffer.write(rendered)
        else:
            sys.stdout.write(rendered.decode(encoding, errors="replace"))
    sys.stdout.flush()


def _stack_dump_timeout_seconds() -> float:
    value = os.environ.get(_STACK_DUMP_TIMEOUT_ENV)
    if value is None:
        return _DEFAULT_STACK_DUMP_TIMEOUT_SECONDS
    try:
        parsed = float(value)
    except ValueError:
        return _DEFAULT_STACK_DUMP_TIMEOUT_SECONDS
    return max(5.0, parsed)


def _child_env() -> dict[str, str]:
    env = clean_env()
    env.setdefault("PYTHONFAULTHANDLER", "1")
    return env


class RunAnalytics:
    def __init__(self, *, command: list[str], pid: int | None) -> None:
        self.command = list(command)
        self.pid = pid
        self.started_at = time.time()
        self.line_count = 0
        self.byte_count = 0
        self.tail_lines: deque[str] = deque(maxlen=_TAIL_LINE_LIMIT)
        self._recent_lines: deque[str] = deque(maxlen=_PYTEST_FAILURE_PRE_CONTEXT_LINE_LIMIT)
        self.pytest_failure_excerpt: deque[str] = deque(maxlen=_PYTEST_FAILURE_LINE_LIMIT)
        self._capturing_pytest_failure = False
        self._failure_capture_remaining = 0
        self.fault_lines: deque[str] = deque(maxlen=20)
        self.last_test_nodeid: str | None = None
        self.last_nonempty_line: str | None = None

    def record_line(self, line: str) -> None:
        self.line_count += 1
        self.byte_count += len(line.encode("utf-8", errors="replace"))
        cleaned = _strip_ansi(line).rstrip("\r\n")
        if not cleaned.strip():
            return
        self.last_nonempty_line = cleaned
        self.tail_lines.append(cleaned)
        nodeid = _extract_pytest_nodeid(cleaned)
        if nodeid is not None:
            self.last_test_nodeid = nodeid
        _track_pytest_failure_excerpt(self, cleaned)
        if _looks_like_fault_line(cleaned):
            self.fault_lines.append(cleaned)

    def as_dict(self, *, log_path: Path, returncode: int) -> dict[str, object]:
        return {
            "command": self.command,
            "pid": self.pid,
            "returncode": returncode,
            "log_path": str(log_path),
            "started_at_epoch": self.started_at,
            "completed_at_epoch": time.time(),
            "line_count": self.line_count,
            "byte_count": self.byte_count,
            "last_test_nodeid": self.last_test_nodeid,
            "last_nonempty_line": self.last_nonempty_line,
            "pytest_failure_excerpt": list(self.pytest_failure_excerpt),
            "tail_lines": list(self.tail_lines),
            "fault_lines": list(self.fault_lines),
        }


def _strip_ansi(text: str) -> str:
    return _ANSI_ESCAPE.sub("", text)


def _extract_pytest_nodeid(line: str) -> str | None:
    match = _PYTEST_NODEID.match(line.strip())
    if match is None:
        return None
    return match.group(1)


def _looks_like_pytest_failure_start(line: str) -> bool:
    return any(
        pattern.search(line) is not None
        for pattern in (
            _PYTEST_FAILURE_HEADER,
            _PYTEST_SUMMARY_HEADER,
            _PYTEST_ERROR_LINE,
            _PYTEST_FAILED_LINE,
        )
    )


def _track_pytest_failure_excerpt(analytics: RunAnalytics, line: str) -> None:
    analytics._recent_lines.append(line)
    if _looks_like_pytest_failure_start(line):
        if not analytics.pytest_failure_excerpt:
            analytics.pytest_failure_excerpt.extend(analytics._recent_lines)
        elif analytics.pytest_failure_excerpt[-1] != line:
            analytics.pytest_failure_excerpt.append(line)
        analytics._capturing_pytest_failure = True
        analytics._failure_capture_remaining = (
            _PYTEST_FAILURE_LINE_LIMIT - len(analytics.pytest_failure_excerpt)
        )
        return
    if analytics._capturing_pytest_failure and analytics._failure_capture_remaining > 0:
        analytics.pytest_failure_excerpt.append(line)
        analytics._failure_capture_remaining -= 1


def _looks_like_fault_line(line: str) -> bool:
    return any(pattern.search(line) is not None for pattern in _FAULT_PATTERNS)


def _analytics_path(log_path: Path) -> Path:
    return log_path.with_name(f"{log_path.name}.analytics.json")


def _write_analytics(log_path: Path, analytics: RunAnalytics, *, returncode: int) -> Path:
    analytics_path = _analytics_path(log_path)
    analytics_path.write_text(
        json.dumps(
            analytics.as_dict(log_path=log_path, returncode=returncode),
            indent=2,
            sort_keys=True,
        ),
        encoding="utf-8",
    )
    return analytics_path


def _emit_failure_summary(
    *,
    log_path: Path,
    analytics_path: Path,
    analytics: RunAnalytics,
    returncode: int,
) -> None:
    _write_console_line(
        f"[run_logged] failure analytics written to {analytics_path} "
        f"(exit_code={returncode})\n"
    )
    if analytics.last_test_nodeid is not None:
        _write_console_line(
            "[run_logged] last pytest nodeid before failure: "
            f"{analytics.last_test_nodeid}\n"
        )
    if analytics.last_nonempty_line is not None:
        _write_console_line(
            "[run_logged] last non-empty output line: "
            f"{analytics.last_nonempty_line}\n"
        )
    if analytics.fault_lines:
        _write_console_line("[run_logged] fault-marked output lines:\n")
        for line in list(analytics.fault_lines)[-_SUMMARY_TAIL_LINE_LIMIT:]:
            _write_console_line(f"{line}\n")
    if analytics.pytest_failure_excerpt:
        _write_console_line("[run_logged] pytest failure excerpt:\n")
        for line in list(analytics.pytest_failure_excerpt)[-_SUMMARY_TAIL_LINE_LIMIT:]:
            _write_console_line(f"{line}\n")
    _write_console_line(f"[run_logged] tail of {log_path}:\n")
    for line in list(analytics.tail_lines)[-_SUMMARY_TAIL_LINE_LIMIT:]:
        _write_console_line(f"{line}\n")


def _dump_thread_stacks(handle, *, command: list[str], pid: int | None, idle_for: float) -> None:
    header = (
        f"\n[run_logged] no output for {idle_for:.1f}s; "
        f"dumping thread stacks for pid={pid} command={command!r}\n"
    )
    for stream in (sys.stderr, handle):
        stream.write(header)
        stream.flush()
        faulthandler.dump_traceback(file=stream, all_threads=True)
        stream.flush()


def _stdout_reader(stdout, lines: queue.Queue[str | None]) -> None:
    try:
        for line in stdout:
            lines.put(line)
    finally:
        stdout.close()
        lines.put(None)


def _watchdog(
    *,
    stop_event: threading.Event,
    handle,
    command: list[str],
    pid: int | None,
    stack_dump_timeout: float,
    get_last_output: Callable[[], float],
) -> None:
    last_dump_at: float | None = None
    while not stop_event.wait(1.0):
        idle_for = time.monotonic() - get_last_output()
        if idle_for < stack_dump_timeout:
            last_dump_at = None
            continue
        if last_dump_at is not None and (time.monotonic() - last_dump_at) < stack_dump_timeout:
            continue
        _dump_thread_stacks(handle, command=command, pid=pid, idle_for=idle_for)
        last_dump_at = time.monotonic()


def main() -> int:
    if len(sys.argv) < 4 or sys.argv[2] != "--":
        print("usage: run_logged.py <log-path> -- <command...>", file=sys.stderr)
        return 2

    log_path = Path(sys.argv[1])
    command = sys.argv[3:]
    log_path.parent.mkdir(parents=True, exist_ok=True)
    stack_dump_timeout = _stack_dump_timeout_seconds()

    with log_path.open("w", encoding="utf-8", errors="replace") as handle:
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=_child_env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        assert process.stdout is not None
        analytics = RunAnalytics(command=command, pid=process.pid)
        lines: queue.Queue[str | None] = queue.Queue()
        stop_watchdog = threading.Event()
        reader = threading.Thread(
            target=_stdout_reader,
            args=(process.stdout, lines),
            name="run-logged-stdout-reader",
            daemon=True,
        )
        last_output = time.monotonic()

        def _get_last_output() -> float:
            return last_output

        watchdog = threading.Thread(
            target=_watchdog,
            kwargs={
                "stop_event": stop_watchdog,
                "handle": handle,
                "command": command,
                "pid": process.pid,
                "stack_dump_timeout": stack_dump_timeout,
                "get_last_output": _get_last_output,
            },
            name="run-logged-watchdog",
            daemon=True,
        )

        def _disarm_watchdog() -> None:
            stop_watchdog.set()

        atexit.register(_disarm_watchdog)
        reader.start()
        watchdog.start()

        try:
            while True:
                try:
                    line = lines.get(timeout=1.0)
                except queue.Empty:
                    if process.poll() is not None and not reader.is_alive():
                        break
                    continue

                if line is None:
                    break

                _write_console_line(line)
                handle.write(line)
                handle.flush()
                analytics.record_line(line)
                last_output = time.monotonic()
            reader.join(timeout=1.0)
            returncode = process.wait()
            if returncode != 0:
                analytics_path = _write_analytics(log_path, analytics, returncode=returncode)
                _emit_failure_summary(
                    log_path=log_path,
                    analytics_path=analytics_path,
                    analytics=analytics,
                    returncode=returncode,
                )
            return returncode
        finally:
            _disarm_watchdog()
            atexit.unregister(_disarm_watchdog)


if __name__ == "__main__":
    sys.exit(main())
