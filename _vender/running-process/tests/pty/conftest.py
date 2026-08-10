"""Autouse fixtures shared by every split PTY test module.

These mirror the autouse fixtures that previously lived at the top of
``tests/test_pty_support.py``. By placing them in a directory-scoped
``conftest.py``, pytest automatically applies them to every test in
``tests/pty/`` without each test file having to import them explicitly.
"""

from __future__ import annotations

import faulthandler
import sys
from collections.abc import Iterator

import pytest

_PTY_SUPPORT_WATCHDOG_TIMEOUT_SECONDS = 120.0


@pytest.fixture(autouse=True)
def _suppress_pty_text_warning_by_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("RUNNING_PROCESS_NO_PTY_TEXT_WARNING", "1")


@pytest.fixture(scope="module", autouse=True)
def _pty_support_module_watchdog() -> Iterator[None]:
    faulthandler.dump_traceback_later(
        _PTY_SUPPORT_WATCHDOG_TIMEOUT_SECONDS,
        file=sys.__stderr__,
        exit=True,
    )
    try:
        yield
    finally:
        faulthandler.cancel_dump_traceback_later()
