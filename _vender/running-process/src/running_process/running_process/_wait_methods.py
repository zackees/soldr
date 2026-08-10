"""Wait-related method implementations extracted from RunningProcess.

These free functions take the RunningProcess instance as the first argument
and contain the original method bodies. The class in :mod:`_core` keeps thin
delegators with identical signatures so the public API is unchanged.
"""

from __future__ import annotations

import sys
import time
from collections.abc import Callable
from contextlib import suppress
from typing import TYPE_CHECKING

from running_process.exit_status import ProcessAbnormalExit, classify_exit_status
from running_process.pty import (
    Expect,
    IdleDetector,
    IdleWaitResult,
    WaitCheckpoint,
    WaitCondition,
    WaitForResult,
)
from running_process.running_process._helpers import (
    _make_timestamped_callback,
    _safe_console_write,
    _validate_echo_flag,
    _validate_echo_timestamps,
)
from running_process.running_process._types import EchoCallback
from running_process.running_process_manager import RunningProcessManagerSingleton

if TYPE_CHECKING:
    from running_process.running_process._core import RunningProcess


def echo_streams(
    process: RunningProcess, echo_callback: EchoCallback | None = None
) -> None:
    for stream, line in process.drain_combined():
        if echo_callback is not None:
            text = (
                line.decode("utf-8", errors="replace")
                if isinstance(line, bytes)
                else line
            )
            echo_callback(text)
        else:
            target = sys.stdout if stream == "stdout" else sys.stderr
            _safe_console_write(target, line)


def finalize_wait(process: RunningProcess) -> None:
    process._output_formatter.end()
    if process._on_complete is not None:
        process._on_complete()


def resolve_echo_callback(
    process: RunningProcess,
    echo: bool | EchoCallback,
    echo_timestamps: str | None,
) -> EchoCallback | None:
    """Resolve echo + echo_timestamps into a single callback (or None)."""
    callback: EchoCallback | None = echo if callable(echo) else None
    if echo_timestamps is not None and bool(echo):
        base = callback if callback is not None else print
        start = process._start_time if process._start_time is not None else time.time()
        callback = _make_timestamped_callback(base, echo_timestamps, start)
    return callback


def wait(
    process: RunningProcess,
    echo: bool | EchoCallback = False,
    timeout: float | None = None,
    *,
    echo_timestamps: str | None = None,
    raise_on_abnormal_exit: bool = False,
    idle_detector: IdleDetector = None,
) -> int | IdleWaitResult:
    try:
        return _wait_impl(
            process,
            echo=echo,
            timeout=timeout,
            echo_timestamps=echo_timestamps,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            idle_detector=idle_detector,
        )
    except KeyboardInterrupt:
        if not process._allows_child_ctrl_c_interruption:
            with suppress(Exception):
                process.kill()
        raise


def _wait_impl(
    process: RunningProcess,
    echo: bool | EchoCallback = False,
    timeout: float | None = None,
    *,
    echo_timestamps: str | None = None,
    raise_on_abnormal_exit: bool = False,
    idle_detector: IdleDetector = None,
) -> int | IdleWaitResult:
    _validate_echo_flag(echo)
    _validate_echo_timestamps(echo_timestamps)
    echo_active = bool(echo) or echo_timestamps is not None
    if echo_timestamps is not None and not echo:
        echo = True
    echo_callback = resolve_echo_callback(process, echo, echo_timestamps)
    if idle_detector is not None:
        result = process.wait_for_idle(
            idle_detector,
            echo=echo,
            echo_timestamps=echo_timestamps,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
        )
        finalize_wait(process)
        return result
    if process._pty_process is not None:
        effective_timeout = timeout if timeout is not None else process.timeout
        if not echo_active:
            code = process._pty_process.wait(
                timeout=effective_timeout,
                raise_on_abnormal_exit=raise_on_abnormal_exit,
            )
        else:
            deadline = (
                time.time() + effective_timeout
                if effective_timeout is not None
                else None
            )
            while True:
                code = process.poll()
                if code is not None:
                    code = process._pty_process.wait(timeout=0)
                    break
                if deadline is not None and time.time() >= deadline:
                    process._handle_timeout(effective_timeout)
                if echo_callback is not None:
                    echo_streams(process, echo_callback)
                else:
                    process._pty_process._echo_to_console(sys.stdout)
                # #199: intentional — wait_for loop polling at 10ms
                # to interleave echo-stream draining with the
                # exit/timeout check. A condvar wouldn't carry the
                # echo work this loop also performs.
                time.sleep(0.01)
            if echo_callback is not None:
                echo_streams(process, echo_callback)
            else:
                process._pty_process._echo_to_console(sys.stdout)
        process._end_time = process._end_time or time.time()
        RunningProcessManagerSingleton.unregister(process)
        process._exit_status = classify_exit_status(
            code, process.KEYBOARD_INTERRUPT_EXIT_CODES
        )
        finalize_wait(process)
        return code
    effective_timeout = timeout if timeout is not None else process.timeout
    deadline = (
        time.time() + effective_timeout if effective_timeout is not None else None
    )
    if not echo_active:
        try:
            code = process._proc.wait(timeout=effective_timeout)
        except TimeoutError:
            process._handle_timeout(effective_timeout)
    else:
        while True:
            code = process.poll()
            if code is not None:
                code = process._proc.wait(timeout=0)
                break
            if deadline is not None and time.time() >= deadline:
                process._handle_timeout(effective_timeout)
            echo_streams(process, echo_callback)
            # #199: intentional — same echo-interleave pattern as
            # the wait_for poll above, used for `wait()`'s subprocess
            # variant. 10ms cadence shared with the PTY branch.
            time.sleep(0.01)

    if echo_active:
        echo_streams(process, echo_callback)

    process._end_time = process._end_time or time.time()
    RunningProcessManagerSingleton.unregister(process)
    process._exit_status = classify_exit_status(
        code, process.KEYBOARD_INTERRUPT_EXIT_CODES
    )
    if code in process.KEYBOARD_INTERRUPT_EXIT_CODES:
        finalize_wait(process)
        raise KeyboardInterrupt
    if raise_on_abnormal_exit and process._exit_status.abnormal:
        finalize_wait(process)
        raise ProcessAbnormalExit(process._exit_status)
    finalize_wait(process)
    return code


def wait_for_idle(
    process: RunningProcess,
    idle_detector: IdleDetector | None = None,
    *,
    echo: bool | EchoCallback = False,
    echo_timestamps: str | None = None,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
) -> IdleWaitResult:
    try:
        return _wait_for_idle_impl(
            process,
            idle_detector,
            echo=echo,
            echo_timestamps=echo_timestamps,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
        )
    except KeyboardInterrupt:
        if not process._allows_child_ctrl_c_interruption:
            with suppress(Exception):
                process.kill()
        raise


def _wait_for_idle_impl(
    process: RunningProcess,
    idle_detector: IdleDetector | None = None,
    *,
    echo: bool | EchoCallback = False,
    echo_timestamps: str | None = None,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
) -> IdleWaitResult:
    _validate_echo_flag(echo)
    _validate_echo_timestamps(echo_timestamps)
    if process._pty_process is None:
        raise NotImplementedError(
            "idle detection currently only supports PTY-backed processes"
        )

    echo_active = bool(echo) or echo_timestamps is not None
    echo_callback = resolve_echo_callback(process, echo, echo_timestamps)
    effective_timeout = timeout if timeout is not None else process.timeout
    result = process._pty_process.wait_for_idle(
        idle_detector,
        timeout=effective_timeout,
        raise_on_abnormal_exit=raise_on_abnormal_exit,
        echo_output=echo_active,
    )
    if echo_active:
        if echo_callback is not None:
            echo_streams(process, echo_callback)
        else:
            process._pty_process._echo_to_console(sys.stdout)
    if result.returncode is not None:
        process._end_time = process._end_time or time.time()
        RunningProcessManagerSingleton.unregister(process)
        process._exit_status = classify_exit_status(
            result.returncode, process.KEYBOARD_INTERRUPT_EXIT_CODES
        )
    return result


def wait_for_expect(
    process: RunningProcess,
    next_expect: Expect | None = None,
    *,
    echo: bool | EchoCallback = False,
    echo_timestamps: str | None = None,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
) -> WaitForResult:
    try:
        return _wait_for_expect_impl(
            process,
            next_expect,
            echo=echo,
            echo_timestamps=echo_timestamps,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
        )
    except KeyboardInterrupt:
        if not process._allows_child_ctrl_c_interruption:
            with suppress(Exception):
                process.kill()
        raise


def _wait_for_expect_impl(
    process: RunningProcess,
    next_expect: Expect | None = None,
    *,
    echo: bool | EchoCallback = False,
    echo_timestamps: str | None = None,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
) -> WaitForResult:
    _validate_echo_flag(echo)
    _validate_echo_timestamps(echo_timestamps)
    if process._pty_process is None:
        raise NotImplementedError(
            "wait_for_expect currently only supports PTY-backed processes"
        )
    echo_active = bool(echo) or echo_timestamps is not None
    echo_callback = resolve_echo_callback(process, echo, echo_timestamps)
    result = process._pty_process.wait_for_expect(
        next_expect,
        timeout=timeout if timeout is not None else process.timeout,
        raise_on_abnormal_exit=raise_on_abnormal_exit,
        echo_output=echo_active,
    )
    if echo_active:
        if echo_callback is not None:
            echo_streams(process, echo_callback)
        else:
            process._pty_process._echo_to_console(sys.stdout)
    if result.returncode is not None:
        process._end_time = process._end_time or time.time()
        RunningProcessManagerSingleton.unregister(process)
        process._exit_status = classify_exit_status(
            result.returncode, process.KEYBOARD_INTERRUPT_EXIT_CODES
        )
    return result


def checkpoint(process: RunningProcess) -> WaitCheckpoint:
    if process._pty_process is None:
        raise NotImplementedError(
            "checkpoint currently only supports PTY-backed processes"
        )
    return process._pty_process.checkpoint()


def wait_for(
    process: RunningProcess,
    *conditions: WaitCondition
    | Callable[..., object]
    | list[WaitCondition | Callable[..., object]]
    | tuple[WaitCondition | Callable[..., object], ...],
    echo: bool | EchoCallback = False,
    echo_timestamps: str | None = None,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
) -> WaitForResult:
    try:
        return _wait_for_impl(
            process,
            *conditions,
            echo=echo,
            echo_timestamps=echo_timestamps,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
        )
    except KeyboardInterrupt:
        if not process._allows_child_ctrl_c_interruption:
            with suppress(Exception):
                process.kill()
        raise


def _wait_for_impl(
    process: RunningProcess,
    *conditions: WaitCondition
    | Callable[..., object]
    | list[WaitCondition | Callable[..., object]]
    | tuple[WaitCondition | Callable[..., object], ...],
    echo: bool | EchoCallback = False,
    echo_timestamps: str | None = None,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
) -> WaitForResult:
    _validate_echo_flag(echo)
    _validate_echo_timestamps(echo_timestamps)
    if process._pty_process is None:
        raise NotImplementedError(
            "wait_for currently only supports PTY-backed processes"
        )

    echo_active = bool(echo) or echo_timestamps is not None
    echo_callback = resolve_echo_callback(process, echo, echo_timestamps)
    result = process._pty_process.wait_for(
        *conditions,
        timeout=timeout if timeout is not None else process.timeout,
        raise_on_abnormal_exit=raise_on_abnormal_exit,
        echo_output=echo_active,
    )
    if echo_active:
        if echo_callback is not None:
            echo_streams(process, echo_callback)
        else:
            process._pty_process._echo_to_console(sys.stdout)
    if result.returncode is not None:
        process._end_time = process._end_time or time.time()
        RunningProcessManagerSingleton.unregister(process)
        process._exit_status = classify_exit_status(
            result.returncode, process.KEYBOARD_INTERRUPT_EXIT_CODES
        )
    return result
