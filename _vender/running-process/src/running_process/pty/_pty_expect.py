"""Pseudo-terminal expect-pattern helpers extracted from ``PseudoTerminalProcess``."""

from __future__ import annotations

import time
from dataclasses import replace
from typing import TYPE_CHECKING

from running_process.exit_status import classify_exit_status
from running_process.expect import (
    ExpectAction,
    ExpectMatch,
    ExpectPattern,
    apply_expect_action,
    ensure_text,
    search_expect_pattern,
)
from running_process.pty._types import Expect, WaitCheckpoint, WaitForResult

if TYPE_CHECKING:
    from running_process.pty._pseudo_terminal import PseudoTerminalProcess


_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS = 2.0


def wait_for_expect(
    process: PseudoTerminalProcess,
    next_expect: Expect | None = None,
    *,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
    echo_output: bool = False,
) -> WaitForResult:
    if not process.capture:
        raise NotImplementedError("PTY wait_for_expect() requires capture=True")
    active_expect_conditions = list(process._registered_expect_conditions)
    if not active_expect_conditions:
        if next_expect is None:
            raise ValueError(
                "No registered Expect conditions are configured for this process"
            )
        active_expect_conditions = [next_expect]
        next_expect = None
    result = process.wait_for(
        *active_expect_conditions,
        timeout=timeout,
        raise_on_abnormal_exit=raise_on_abnormal_exit,
        echo_output=echo_output,
    )
    if not result.matched:
        process._registered_expect_conditions = active_expect_conditions
        return result
    if next_expect is None:
        process._registered_expect_conditions = []
        return result
    offset = process.checkpoint().offset
    if result.expect_match is not None:
        offset = result.expect_match.span[1]
    process._registered_expect_conditions = [
        replace(next_expect, after=WaitCheckpoint(offset))
    ]
    return result


def expect(
    process: PseudoTerminalProcess,
    pattern: ExpectPattern,
    *,
    timeout: float | None = None,
    action: ExpectAction = None,
) -> ExpectMatch:
    from running_process.pty._pseudo_terminal import KEYBOARD_INTERRUPT_EXIT_CODES

    if not process.capture:
        raise NotImplementedError("PTY expect() requires capture=True")
    deadline = time.time() + timeout if timeout is not None else None
    buffer, history_bytes = process._snapshot_output_history()

    while True:
        match = search_expect_pattern(buffer, pattern)
        if match is not None:
            apply_expect_action(process, action, match)
            return match

        wait_timeout = 0.1
        if deadline is not None:
            remaining = deadline - time.time()
            if remaining <= 0:
                if process.poll() is not None:
                    raise EOFError(
                        f"Pattern not found before stream closed: {pattern!r}"
                    )
                raise TimeoutError(f"Pattern not found before timeout: {pattern!r}")
            wait_timeout = min(wait_timeout, remaining)

        try:
            chunk = process.read(timeout=wait_timeout)
        except TimeoutError:
            new_output, current_history_bytes = process._snapshot_output_since(
                history_bytes
            )
            if current_history_bytes > history_bytes:
                buffer = f"{buffer}{new_output}"
                history_bytes = current_history_bytes
                continue
            code = process.poll()
            if code is not None:
                process._drain_native_until_eof(
                    timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                )
                process._finalize("exit")
                process._exit_status = classify_exit_status(
                    code, KEYBOARD_INTERRUPT_EXIT_CODES
                )
                new_output, current_history_bytes = process._snapshot_output_since(
                    history_bytes
                )
                if current_history_bytes > history_bytes:
                    buffer = f"{buffer}{new_output}"
                    history_bytes = current_history_bytes
                    continue
                raise EOFError(
                    f"Pattern not found before stream closed: {pattern!r}"
                ) from None
            continue
        except EOFError as exc:
            raise EOFError(
                f"Pattern not found before stream closed: {pattern!r}"
            ) from exc
        buffer = f"{buffer}{ensure_text(chunk, process.encoding, process.errors)}"
        assert process._buffer is not None
        history_bytes = int(process._buffer.history_bytes())
