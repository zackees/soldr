"""Cross-thread KeyboardInterrupt propagation.

Worker threads that catch KeyboardInterrupt must notify the main thread
so the interrupt is not silently swallowed.
"""

from __future__ import annotations

import _thread
import threading


def is_main_thread() -> bool:
    """Return True if the calling thread is the main thread."""
    return threading.current_thread() is threading.main_thread()


def handle_keyboard_interrupt(exc: KeyboardInterrupt) -> None:
    """Handle KeyboardInterrupt properly across main and worker threads.

    In the main thread this re-raises the exception.  In worker threads
    it calls ``_thread.interrupt_main()`` to notify the main thread and
    then re-raises locally so the worker can clean up.
    """
    if is_main_thread():
        raise exc
    _thread.interrupt_main()
    raise exc
