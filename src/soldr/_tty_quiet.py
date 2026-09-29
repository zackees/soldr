"""Keep terminal input reports from being echoed over a PEP 517 build.

A TUI that exits uncleanly (killed at a "Press Ctrl-C again" prompt, say) can
leave focus reporting (``CSI ?1004h``) or alternate-scroll mode switched on.
The terminal then *types* ``ESC [ I`` / ``ESC [ O`` on every focus change and
``ESC [ B`` per mouse-wheel notch. Nothing reads stdin during a build, so the
kernel's line discipline echoes those bytes straight into pip's progress line
as ``-^[[O^[[I^[[B^[[B...``. soldr emits none of those modes itself; it just
runs long enough for the echo to be very visible.

While the backend child runs we clear ``ECHO`` on the controlling terminal
(``ISIG`` is untouched, so Ctrl-C still interrupts), then restore the exact
prior attributes and discard the queued reports so they do not land on the
shell prompt either. Anything that is not an interactive POSIX tty is a no-op.
"""

from __future__ import annotations

import os
import sys
from contextlib import contextmanager
from typing import Iterator

_DISABLE_ENV = "SOLDR_PEP517_TTY_ECHO"


@contextmanager
def quiet_tty_echo() -> Iterator[None]:
    """Suppress tty echo for the duration of the block; restore on exit."""
    if os.environ.get(_DISABLE_ENV, "").strip().lower() in {"1", "on", "keep"}:
        yield
        return
    try:
        import termios  # pylint: disable=import-outside-toplevel

        fd = sys.stdin.fileno()
        if not os.isatty(fd):
            raise OSError("stdin is not a tty")
        saved = termios.tcgetattr(fd)
        quiet = list(saved)
        quiet[3] &= ~(termios.ECHO | termios.ECHONL)
        termios.tcsetattr(fd, termios.TCSANOW, quiet)
    except (ImportError, AttributeError, ValueError, OSError):
        # No termios (Windows), stdin closed/redirected, or not our terminal.
        yield
        return
    try:
        yield
    finally:
        try:
            termios.tcflush(fd, termios.TCIFLUSH)
            termios.tcsetattr(fd, termios.TCSANOW, saved)
        except OSError:
            pass
