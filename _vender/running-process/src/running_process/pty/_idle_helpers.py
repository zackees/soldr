from __future__ import annotations

import inspect
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from running_process.expect import ensure_text
from running_process.pty._types import (
    Callback,
    Expect,
    Idle,
    IdleContext,
    IdleDetection,
    IdleDetector,
    IdleDiff,
    IdleInfoDiff,
    IdleReachedCallback,
    IdleResetPredicate,
    IdleStartTrigger,
    IdleTiming,
    WaitCallbackResult,
    WaitCondition,
)
from running_process.pty._wait_input import WaitInputBuffer, _BufferedInput

if TYPE_CHECKING:
    from running_process.pty._pseudo_terminal import PseudoTerminalProcess


def _compile_idle_detector(
    idle_detector: IdleDetector,
) -> tuple[IdleTiming | None, IdleReachedCallback | None, IdleResetPredicate | None]:
    if idle_detector is None:
        return None, None, None
    if isinstance(idle_detector, IdleDetection):
        if idle_detector.idle_reached is not None and idle_detector.predicate is not None:
            raise ValueError("idle_reached and predicate are mutually exclusive")
        if idle_detector.idle_reached is not None:
            return idle_detector.timing, idle_detector.idle_reached, None
        predicate = idle_detector.predicate or _build_default_idle_reset(idle_detector)
        return idle_detector.timing, None, predicate
    if callable(idle_detector):
        callback_arity = _callable_arity(idle_detector)
        if callback_arity == 1:
            return IdleTiming(), idle_detector, None
        if callback_arity == 2:
            return IdleTiming(), None, idle_detector
        raise TypeError("idle_detector callable must accept 1 or 2 positional arguments")
    raise TypeError(
        "idle_detector must be None, an IdleDetection instance, or a callable callback"
    )


def _callable_arity(callback: Callable[..., Any]) -> int:
    signature = inspect.signature(callback)
    required_positional = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        and parameter.default is inspect.Parameter.empty
    ]
    has_varargs = any(
        parameter.kind is inspect.Parameter.VAR_POSITIONAL
        for parameter in signature.parameters.values()
    )
    if has_varargs:
        if len(required_positional) <= 1:
            return 1
        if len(required_positional) == 2:
            return 2
    if len(required_positional) in {1, 2}:
        return len(required_positional)
    raise TypeError("idle_detector callable must accept 1 or 2 positional arguments")


def _wait_callback_arity(callback: Callable[..., object]) -> int:
    signature = inspect.signature(callback)
    required_positional = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        and parameter.default is inspect.Parameter.empty
    ]
    has_varargs = any(
        parameter.kind is inspect.Parameter.VAR_POSITIONAL
        for parameter in signature.parameters.values()
    )
    if has_varargs and len(required_positional) <= 2:
        return len(required_positional)
    if len(required_positional) in {0, 1, 2}:
        return len(required_positional)
    raise TypeError("wait callback must accept 0, 1, or 2 positional arguments")


def _invoke_wait_callback(
    callback: Callable[..., object], process: PseudoTerminalProcess
) -> tuple[object, list[str | bytes]]:
    arity = _wait_callback_arity(callback)
    input_buffer = WaitInputBuffer()
    if arity == 0:
        result = callback()
    elif arity == 1:
        result = callback(input_buffer)
    else:
        result = callback(input_buffer, process)
    return result, input_buffer.drain()


def _condition_callback_arity(callback: Callable[..., object]) -> int:
    signature = inspect.signature(callback)
    required_positional = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        and parameter.default is inspect.Parameter.empty
    ]
    has_varargs = any(
        parameter.kind is inspect.Parameter.VAR_POSITIONAL
        for parameter in signature.parameters.values()
    )
    if has_varargs and len(required_positional) <= 3:
        return len(required_positional)
    if len(required_positional) in {0, 1, 2, 3}:
        return len(required_positional)
    raise TypeError("condition on_callback must accept 0, 1, 2, or 3 positional arguments")


def _invoke_condition_callback(
    callback: Callable[..., object],
    payload: object,
    process: PseudoTerminalProcess,
) -> tuple[WaitCallbackResult, list[str | bytes]]:
    arity = _condition_callback_arity(callback)
    input_buffer = WaitInputBuffer()
    if arity == 0:
        result = callback()
    elif arity == 1:
        result = callback(payload)
    elif arity == 2:
        result = callback(payload, input_buffer)
    else:
        result = callback(payload, input_buffer, process)
    if not isinstance(result, WaitCallbackResult):
        raise TypeError("condition on_callback must return a WaitCallbackResult")
    return result, input_buffer.drain()


def _normalize_wait_conditions(
    *conditions: (
        WaitCondition
        | Callable[..., object]
        | list[WaitCondition | Callable[..., object]]
        | tuple[WaitCondition | Callable[..., object], ...]
    ),
) -> list[WaitCondition]:
    normalized: list[WaitCondition] = []
    for condition in conditions:
        if isinstance(condition, Idle | Expect | Callback):
            normalized.append(condition)
            continue
        if callable(condition):
            normalized.append(Callback(condition))
            continue
        if isinstance(condition, list | tuple):
            for nested in condition:
                if isinstance(nested, Idle | Expect | Callback):
                    normalized.append(nested)
                    continue
                if callable(nested):
                    normalized.append(Callback(nested))
                    continue
                raise TypeError("wait_for conditions must be Idle, Expect, Callback, or a callable")
            continue
        raise TypeError("wait_for conditions must be Idle, Expect, Callback, or a callable")
    return normalized


def _flush_wait_input(
    process: PseudoTerminalProcess, items: list[str | bytes | _BufferedInput]
) -> None:
    for item in items:
        if isinstance(item, _BufferedInput):
            process.write(item.data, submit=item.submit)
            continue
        process.write(item)


def _resolve_expect_offset(
    condition: Expect, process: PseudoTerminalProcess
) -> int:
    if condition.after == "start":
        return 0
    if condition.after == "now":
        return len(ensure_text(process.output, process.encoding, process.errors))
    return max(0, condition.after.offset)


def _build_default_idle_reset(cfg: IdleDetection) -> IdleResetPredicate:
    return lambda diff, ctx: _default_idle_reset(diff, ctx, cfg)


def _input_contains_newline(data: bytes) -> bool:
    return b"\r" in data or b"\n" in data


def _start_event_count(
    process: PseudoTerminalProcess, start_trigger: IdleStartTrigger
) -> int:
    process._sync_native_input_metrics()
    if start_trigger is IdleStartTrigger.INPUT_NEWLINE:
        return process._pty_newline_events_total
    if start_trigger is IdleStartTrigger.INPUT_SUBMIT:
        return process._pty_submit_events_total
    return 1


def _default_idle_reset(diff: IdleDiff, _ctx: IdleContext, cfg: IdleDetection) -> bool:
    pty_cfg = cfg.pty
    if pty_cfg is not None:
        if pty_cfg.reset_on_input and diff.pty_input_bytes > 0:
            return True
        output_bytes = diff.pty_output_bytes
        if pty_cfg.count_control_churn_as_output:
            output_bytes += diff.pty_control_churn_bytes
        if pty_cfg.reset_on_output and output_bytes > 0:
            return True

    process_cfg = cfg.process
    if process_cfg is not None:
        if diff.cpu_percent > process_cfg.cpu_percent_before_reset:
            return True
        if diff.disk_io_bytes > process_cfg.max_disk_io_bytes_before_reset:
            return True
        if diff.network_io_bytes > process_cfg.max_network_bytes_before_reset:
            return True

    return False


def _merge_idle_diff(base: IdleInfoDiff, update: IdleInfoDiff) -> IdleInfoDiff:
    total_delta = base.delta_seconds + update.delta_seconds
    weighted_cpu = 0.0
    if total_delta > 0:
        weighted_cpu = (
            (base.cpu_percent * base.delta_seconds) + (update.cpu_percent * update.delta_seconds)
        ) / total_delta
    return IdleInfoDiff(
        delta_seconds=total_delta,
        process_alive=update.process_alive,
        pty_input_bytes=base.pty_input_bytes + update.pty_input_bytes,
        pty_output_bytes=base.pty_output_bytes + update.pty_output_bytes,
        pty_control_churn_bytes=base.pty_control_churn_bytes + update.pty_control_churn_bytes,
        cpu_percent=weighted_cpu,
        disk_io_bytes=base.disk_io_bytes + update.disk_io_bytes,
        network_io_bytes=base.network_io_bytes + update.network_io_bytes,
    )


def _control_churn_bytes(chunk: bytes) -> int:
    total = 0
    index = 0
    while index < len(chunk):
        byte = chunk[index]
        if byte == 0x1B:
            start = index
            index += 1
            if index < len(chunk) and chunk[index] == ord("["):
                index += 1
                while index < len(chunk):
                    current = chunk[index]
                    index += 1
                    if 0x40 <= current <= 0x7E:
                        break
            total += index - start
            continue
        if byte in {0x08, 0x0D, 0x7F}:
            total += 1
        index += 1
    return total
