"""Pseudo-terminal and interactive process facade.

This package re-exports the public ``running_process.pty`` surface from
focused sub-modules. The underscore-prefixed modules (``_pseudo_terminal``,
``_idle_helpers``, ...) are private implementation details — import only
from ``running_process.pty`` itself.
"""

# ruff: noqa: F401
# This file is a re-export shim. Private underscore-prefixed symbols are
# intentionally re-surfaced so tests and legacy callers can reach them via
# ``running_process.pty.<name>`` (and monkeypatch them) without depending on
# the internal sub-module layout.

from __future__ import annotations

# The ``sys``/``os``/``time``/``signal`` modules are imported here so that
# legacy callers that reach in via ``running_process.pty.sys`` (or similar
# attribute access used by tests for monkeypatching the corresponding
# global modules) continue to work after the split.
import os
import signal
import sys
import time

from running_process.pty._command import (
    _apply_process_nice,
    _contains_shell_metacharacters,
    _normalize_command,
    _posix_pty_command,
    _pty_command,
    _split_command,
    _strip_wrapping_quotes,
    _windows_pty_command,
    _wrap_posix_pty_command_with_nice,
    interactive_launch_spec,
)
from running_process.pty._console_io import (
    _enable_windows_vt_output_handle,
    _ensure_windows_vt_output,
    _safe_console_write,
    _safe_console_write_chunk,
    _windows_console_output_handle,
)
from running_process.pty._errors import PtyNotAvailableError, SignalBool
from running_process.pty._idle_helpers import (
    _build_default_idle_reset,
    _callable_arity,
    _compile_idle_detector,
    _condition_callback_arity,
    _control_churn_bytes,
    _default_idle_reset,
    _flush_wait_input,
    _input_contains_newline,
    _invoke_condition_callback,
    _invoke_wait_callback,
    _merge_idle_diff,
    _normalize_wait_conditions,
    _resolve_expect_offset,
    _start_event_count,
    _wait_callback_arity,
)
from running_process.pty._idle_state import (
    _ExpectRuntimeState,
    _IdleCallbackThreadState,
    _IdleRuntimeState,
    _IdleSample,
    _WaitCallbackState,
)
from running_process.pty._interactive import InteractiveProcess
from running_process.pty._process_helpers import (
    _NO_PTY_TEXT_WARNING_ENV,
    _PTY_CLEANUP_ERRORS,
    _close_native_pty_process,
    _warn_pty_text_mode_ignored,
)
from running_process.pty._pseudo_terminal import (
    _PTY_POLL_INTERVAL_SECONDS,
    _PTY_READ_CHUNK_TIMEOUT_SECONDS,
    _PTY_READER_NATIVE_CLOSE_WAIT_SECONDS,
    _SUPPORTED_PTY_PLATFORMS,
    KEYBOARD_INTERRUPT_EXIT_CODES,
    PseudoTerminalProcess,
    Pty,
)
from running_process.pty._terminal_strip import (
    _collapse_duplicate_carriage_returns,
    _find_csi_end,
    _normalize_csi_sequence,
    _strip_terminal_fragments,
    _TerminalControlStripper,
)
from running_process.pty._types import (
    Callback,
    Expect,
    Idle,
    IdleContext,
    IdleDecision,
    IdleDetection,
    IdleDetector,
    IdleDiff,
    IdleInfoDiff,
    IdleReachedCallback,
    IdleResetPredicate,
    IdleStartTrigger,
    IdleTiming,
    IdleWaitResult,
    InteractiveLaunchSpec,
    InteractiveMode,
    InterruptResult,
    ProcessIdleDetection,
    PtyIdleDetection,
    WaitCallbackResult,
    WaitCheckpoint,
    WaitCondition,
    WaitForResult,
)
from running_process.pty._wait_input import WaitInputBuffer, _BufferedInput

__all__ = [
    "KEYBOARD_INTERRUPT_EXIT_CODES",
    "Callback",
    "Expect",
    "Idle",
    "IdleContext",
    "IdleDecision",
    "IdleDetection",
    "IdleDetector",
    "IdleDiff",
    "IdleInfoDiff",
    "IdleReachedCallback",
    "IdleResetPredicate",
    "IdleStartTrigger",
    "IdleTiming",
    "IdleWaitResult",
    "InteractiveLaunchSpec",
    "InteractiveMode",
    "InteractiveProcess",
    "InterruptResult",
    "ProcessIdleDetection",
    "PseudoTerminalProcess",
    "Pty",
    "PtyIdleDetection",
    "PtyNotAvailableError",
    "SignalBool",
    "WaitCallbackResult",
    "WaitCheckpoint",
    "WaitCondition",
    "WaitForResult",
    "WaitInputBuffer",
    "interactive_launch_spec",
]
