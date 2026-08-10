from __future__ import annotations

import os
import warnings
from collections.abc import Mapping
from contextlib import suppress

from running_process._native import NativeProcess

_PTY_CLEANUP_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, AttributeError)
_NO_PTY_TEXT_WARNING_ENV = "RUNNING_PROCESS_NO_PTY_TEXT_WARNING"


def _close_native_pty_process(proc: NativeProcess | None) -> None:
    if proc is None:
        return
    # Finalizers must not block indefinitely while the interpreter is collecting.
    # Use best-effort non-blocking termination instead of `close()`.
    with suppress(*_PTY_CLEANUP_ERRORS):
        proc.kill()
    with suppress(*_PTY_CLEANUP_ERRORS):
        proc.terminate()


def _warn_pty_text_mode_ignored(env: Mapping[str, str] | None) -> None:
    effective_env = env if env is not None else os.environ
    if effective_env.get(_NO_PTY_TEXT_WARNING_ENV):
        return
    warnings.warn(
        "PTY mode ignores text/universal_newlines and always uses raw bytes; "
        f"set {_NO_PTY_TEXT_WARNING_ENV}=1 to suppress this warning.",
        RuntimeWarning,
        stacklevel=3,
    )
