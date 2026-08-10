"""Shared helper functions for the split PTY test modules.

This module exists so the various ``tests/pty/test_pty_*.py`` files can share
the helpers that used to live at the top of ``tests/test_pty_support.py``
without duplicating them. The autouse fixtures live in
``tests/pty/conftest.py`` so pytest picks them up automatically for every
test in this directory.
"""

from __future__ import annotations

import io
import sys
import time

from running_process import RunningProcess


def _read_until_contains(process: object, needle: str, timeout: float = 10) -> str:
    deadline = time.time() + timeout
    chunks: list[str] = []
    while time.time() < deadline:
        try:
            chunk = process.read(timeout=0.2)  # type: ignore[attr-defined]
        except TimeoutError:
            continue
        except EOFError:
            break
        text = chunk.decode("utf-8", errors="replace") if isinstance(chunk, bytes) else chunk
        chunks.append(text)
        if needle in "".join(chunks):
            return "".join(chunks)
    raise AssertionError(f"Did not observe {needle!r} in PTY output: {''.join(chunks)!r}")


def _capture_wait_echo_bytes(process: RunningProcess) -> bytes:
    fake_stdout = io.TextIOWrapper(io.BytesIO(), encoding="utf-8", newline="")
    original_stdout = sys.stdout
    sys.stdout = fake_stdout
    try:
        assert process.wait(timeout=5, echo=True) == 0
        fake_stdout.flush()
        return fake_stdout.buffer.getvalue()
    finally:
        sys.stdout = original_stdout
