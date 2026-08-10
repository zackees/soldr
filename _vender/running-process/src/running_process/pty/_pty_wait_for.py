"""Pseudo-terminal multi-condition wait_for() helper extracted from ``PseudoTerminalProcess``."""

from __future__ import annotations

import sys
import threading
import time
from collections.abc import Callable
from typing import TYPE_CHECKING

from running_process.exit_status import ProcessAbnormalExit, classify_exit_status
from running_process.expect import (
    ExpectMatch,
    apply_expect_action,
    search_expect_pattern,
)
from running_process.pty._idle_helpers import (
    _build_default_idle_reset,
    _compile_idle_detector,
    _flush_wait_input,
    _invoke_condition_callback,
    _invoke_wait_callback,
    _normalize_wait_conditions,
    _resolve_expect_offset,
    _start_event_count,
)
from running_process.pty._idle_state import (
    _ExpectRuntimeState,
    _IdleRuntimeState,
    _IdleSample,
    _WaitCallbackState,
)
from running_process.pty._types import (
    Callback,
    Expect,
    Idle,
    IdleContext,
    IdleDecision,
    IdleDetection,
    IdleInfoDiff,
    IdleReachedCallback,
    IdleResetPredicate,
    IdleStartTrigger,
    IdleTiming,
    IdleWaitResult,
    ProcessIdleDetection,
    WaitCallbackResult,
    WaitCondition,
    WaitForResult,
)

if TYPE_CHECKING:
    from running_process.pty._pseudo_terminal import PseudoTerminalProcess


_PTY_POLL_INTERVAL_SECONDS = 0.001
_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS = 2.0


def wait_for(
    process: PseudoTerminalProcess,
    *conditions: (
        WaitCondition
        | Callable[..., object]
        | list[WaitCondition | Callable[..., object]]
        | tuple[WaitCondition | Callable[..., object], ...]
    ),
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
    echo_output: bool = False,
) -> WaitForResult:
    from running_process.pty._pseudo_terminal import KEYBOARD_INTERRUPT_EXIT_CODES

    wait_conditions = _normalize_wait_conditions(*conditions)
    loop_iterations = 0
    sleep_ns = 0
    expect_scan_ns = 0
    expect_scan_count = 0
    history_update_ns = 0
    history_update_count = 0

    if not wait_conditions:
        code = process.wait(
            timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit
        )
        return WaitForResult(returncode=code, matched=False, exit_reason="process_exit")

    idle_conditions = [
        condition for condition in wait_conditions if isinstance(condition, Idle)
    ]
    if len(idle_conditions) > 1:
        raise ValueError("wait_for supports at most one Idle condition")

    if (
        len(wait_conditions) == 1
        and isinstance(wait_conditions[0], Idle)
        and wait_conditions[0].on_callback is None
    ):
        idle_condition = wait_conditions[0]
        idle_result = process.wait_for_idle(
            idle_condition.detector,
            timeout=timeout,
            raise_on_abnormal_exit=raise_on_abnormal_exit,
            echo_output=echo_output,
        )
        return WaitForResult(
            returncode=idle_result.returncode,
            matched=idle_result.idle_detected,
            exit_reason=(
                "condition_met"
                if idle_result.idle_detected
                else (
                    "interrupt"
                    if idle_result.exit_reason == "interrupt"
                    else idle_result.exit_reason
                )
            ),
            condition=idle_condition if idle_result.idle_detected else None,
            idle_result=idle_result,
        )

    idle_condition = idle_conditions[0] if idle_conditions else None
    expect_conditions = [
        condition for condition in wait_conditions if isinstance(condition, Expect)
    ]
    if expect_conditions and not process.capture:
        raise NotImplementedError(
            "PTY wait_for() Expect conditions require capture=True"
        )
    expect_states: list[tuple[Expect, _ExpectRuntimeState]] = [
        (
            condition,
            _ExpectRuntimeState(
                search_offset=_resolve_expect_offset(condition, process)
            ),
        )
        for condition in expect_conditions
    ]
    callback_conditions = [
        condition for condition in wait_conditions if isinstance(condition, Callback)
    ]

    timing: IdleTiming | None = None
    idle_reached: IdleReachedCallback | None = None
    predicate: IdleResetPredicate | None = None
    default_predicate: IdleResetPredicate | None = None
    idle_state: _IdleRuntimeState | None = None
    idle_timeout_enabled = process.idle_timeout_enabled
    previous: _IdleSample | None = None
    process_cfg: ProcessIdleDetection | None = None
    start_trigger = IdleStartTrigger.IMMEDIATE
    start_events_seen = _start_event_count(process, start_trigger)
    idle_armed = idle_condition is not None
    next_idle_sample_at: float | None = None

    if idle_condition is not None:
        timing, idle_reached, predicate = _compile_idle_detector(
            idle_condition.detector
        )
        if timing is None or (idle_reached is None and predicate is None):
            raise ValueError("Idle condition requires an active idle detector")
        if isinstance(idle_condition.detector, IdleDetection):
            default_predicate = _build_default_idle_reset(idle_condition.detector)
            process_cfg = idle_condition.detector.process
            if idle_condition.detector.pty is not None:
                start_trigger = idle_condition.detector.pty.start_trigger
        else:
            default_predicate = _build_default_idle_reset(IdleDetection())
        started = time.time()
        idle_state = _IdleRuntimeState(last_reset_at=started, stable_since=None)
        previous = process._sample_idle_snapshot(process_cfg=process_cfg)
        next_idle_sample_at = started + timing.sample_interval_seconds
        start_events_seen = _start_event_count(process, start_trigger)
        idle_armed = (
            start_trigger is IdleStartTrigger.IMMEDIATE or start_events_seen > 0
        )

    callback_states: list[tuple[Callback, _WaitCallbackState]] = []
    callback_threads: list[threading.Thread] = []
    stop_callbacks = threading.Event()

    for condition in callback_conditions:
        state = _WaitCallbackState()
        callback_states.append((condition, state))

        def run_callback(
            callback_condition: Callback = condition,
            callback_state: _WaitCallbackState = state,
        ) -> None:
            while not stop_callbacks.is_set():
                try:
                    result, pending_writes = _invoke_wait_callback(
                        callback_condition.callback, process
                    )
                except BaseException as exc:
                    callback_state.error = exc
                    if isinstance(exc, KeyboardInterrupt):
                        import _thread

                        _thread.interrupt_main()
                    return
                if pending_writes:
                    with callback_state.lock:
                        callback_state.pending_writes.extend(pending_writes)
                if result:
                    callback_state.result = result
                    callback_state.ready.store(True)
                    return
                if stop_callbacks.wait(
                    max(0.001, callback_condition.poll_interval_seconds)
                ):
                    return

        thread = threading.Thread(target=run_callback, daemon=True)
        thread.start()
        callback_threads.append(thread)

    deadline = time.time() + timeout if timeout is not None else None
    if process.capture:
        buffer, history_bytes = process._snapshot_output_history()
    else:
        buffer, history_bytes = "", 0

    try:
        while True:
            loop_iterations += 1
            if echo_output:
                process._echo_to_console(sys.stdout)

            if process.capture:
                new_output, current_history_bytes = process._snapshot_output_since(
                    history_bytes
                )
                if current_history_bytes > history_bytes:
                    history_update_start = time.perf_counter_ns()
                    buffer = f"{buffer}{new_output}"
                    history_bytes = current_history_bytes
                    history_update_count += 1
                    history_update_ns += time.perf_counter_ns() - history_update_start
            for condition, state in expect_states:
                if not state.armed:
                    continue
                scoped_buffer = buffer[state.search_offset :]
                scan_start = time.perf_counter_ns()
                suppress_match = (
                    search_expect_pattern(scoped_buffer, condition.NOT)
                    if condition.NOT is not None
                    else None
                )
                match = search_expect_pattern(scoped_buffer, condition.pattern)
                expect_scan_count += 1
                expect_scan_ns += time.perf_counter_ns() - scan_start
                if suppress_match is not None and (
                    match is None or suppress_match.span[0] <= match.span[0]
                ):
                    state.search_offset += suppress_match.span[1]
                    state.armed = False
                    continue
                if match is None:
                    continue
                adjusted_match = ExpectMatch(
                    buffer=buffer,
                    matched=match.matched,
                    span=(
                        state.search_offset + match.span[0],
                        state.search_offset + match.span[1],
                    ),
                    groups=match.groups,
                )
                state.search_offset = adjusted_match.span[1]
                apply_expect_action(process, condition.action, adjusted_match)
                if condition.on_callback is not None:
                    action, pending_writes = _invoke_condition_callback(
                        condition.on_callback, adjusted_match, process
                    )
                    _flush_wait_input(process, pending_writes)
                    if action is WaitCallbackResult.CONTINUE:
                        continue
                    if action is WaitCallbackResult.CONTINUE_AND_DISARM:
                        state.armed = False
                        continue
                return WaitForResult(
                    returncode=process.poll(),
                    matched=True,
                    exit_reason="condition_met",
                    condition=condition,
                    expect_match=adjusted_match,
                )

            for condition, state in callback_states:
                if state.error is not None:
                    raise state.error
                with state.lock:
                    pending_writes = list(state.pending_writes)
                    state.pending_writes.clear()
                if pending_writes:
                    _flush_wait_input(process, pending_writes)
                if state.ready.load():
                    return WaitForResult(
                        returncode=process.poll(),
                        matched=True,
                        exit_reason="condition_met",
                        condition=condition,
                        callback_result=state.result,
                    )

            code = process.poll()
            if code is not None:
                process._drain_native_until_eof(
                    timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                )
                process._finalize("exit")
                process._exit_status = classify_exit_status(
                    code, KEYBOARD_INTERRUPT_EXIT_CODES
                )
                if process.capture:
                    new_output, current_history_bytes = process._snapshot_output_since(
                        history_bytes
                    )
                    if current_history_bytes > history_bytes:
                        history_update_start = time.perf_counter_ns()
                        buffer = f"{buffer}{new_output}"
                        history_bytes = current_history_bytes
                        history_update_count += 1
                        history_update_ns += (
                            time.perf_counter_ns() - history_update_start
                        )
                        continue
                if code in KEYBOARD_INTERRUPT_EXIT_CODES:
                    raise KeyboardInterrupt
                if raise_on_abnormal_exit and process._exit_status.abnormal:
                    raise ProcessAbnormalExit(process._exit_status)
                return WaitForResult(
                    returncode=code,
                    matched=False,
                    exit_reason="process_exit",
                )

            now = time.time()
            if deadline is not None and now >= deadline:
                return WaitForResult(
                    returncode=process.poll(),
                    matched=False,
                    exit_reason="timeout",
                )

            if (
                idle_armed
                and idle_state is not None
                and process.idle_timeout_enabled != idle_timeout_enabled
            ):
                idle_timeout_enabled = process.idle_timeout_enabled
                if idle_timeout_enabled:
                    idle_state.last_reset_at = now
                    idle_state.stable_since = None

            if (
                idle_armed
                and timing is not None
                and idle_state is not None
                and previous is not None
                and next_idle_sample_at is not None
                and now >= next_idle_sample_at
            ):
                current = process._sample_idle_snapshot(process_cfg=process_cfg)
                diff = IdleInfoDiff(
                    delta_seconds=max(0.0, current.sampled_at - previous.sampled_at),
                    process_alive=current.process_alive,
                    pty_input_bytes=current.pty_input_bytes - previous.pty_input_bytes,
                    pty_output_bytes=current.pty_output_bytes
                    - previous.pty_output_bytes,
                    pty_control_churn_bytes=(
                        current.pty_control_churn_bytes
                        - previous.pty_control_churn_bytes
                    ),
                    cpu_percent=current.cpu_percent,
                    disk_io_bytes=current.disk_io_bytes - previous.disk_io_bytes,
                    network_io_bytes=current.network_io_bytes
                    - previous.network_io_bytes,
                )
                previous = current
                sample_now = current.sampled_at
                next_idle_sample_at = sample_now + timing.sample_interval_seconds

                if not idle_armed and start_trigger is not IdleStartTrigger.IMMEDIATE:
                    current_start_events = _start_event_count(process, start_trigger)
                    if current_start_events != start_events_seen:
                        start_events_seen = current_start_events
                        idle_armed = True
                        idle_state.last_reset_at = sample_now
                        idle_state.stable_since = None
                        process.last_activity_at = sample_now
                    else:
                        continue

                stable_for = 0.0
                if idle_state.stable_since is not None:
                    stable_for = max(0.0, sample_now - idle_state.stable_since)
                ctx = IdleContext(
                    idle_for_seconds=max(0.0, sample_now - idle_state.last_reset_at),
                    stable_for_seconds=stable_for,
                    sample_count=idle_state.sample_count,
                )
                idle_state.sample_count += 1

                handled = False
                idle_detected = False
                if idle_reached is not None:
                    decision = idle_reached(diff)
                    if not isinstance(decision, IdleDecision):
                        raise TypeError(
                            "idle_reached callback must return an IdleDecision"
                        )
                    if decision is IdleDecision.ACTIVE:
                        idle_state.last_reset_at = sample_now
                        idle_state.stable_since = None
                        process.last_activity_at = sample_now
                        handled = True
                    elif decision is IdleDecision.BEGIN_IDLE:
                        if idle_state.stable_since is None:
                            idle_started_at = max(0.0, sample_now - diff.delta_seconds)
                            idle_state.last_reset_at = idle_started_at
                            idle_state.stable_since = idle_started_at
                        handled = True
                    elif decision is IdleDecision.IS_IDLE:
                        idle_detected = True

                if not handled and not idle_detected:
                    should_reset = False
                    if predicate is not None and predicate(diff, ctx):
                        should_reset = True
                    elif idle_reached is not None and default_predicate is not None:
                        should_reset = default_predicate(diff, ctx)

                    if should_reset:
                        idle_state.last_reset_at = sample_now
                        idle_state.stable_since = None
                        process.last_activity_at = sample_now
                    else:
                        if idle_state.stable_since is None:
                            idle_state.stable_since = sample_now
                        idle_for = max(0.0, sample_now - idle_state.last_reset_at)
                        stable_for = max(0.0, sample_now - idle_state.stable_since)
                        if (
                            idle_timeout_enabled
                            and idle_for >= timing.timeout_seconds
                            and stable_for >= timing.stability_window_seconds
                        ):
                            idle_detected = True

                if idle_detected:
                    idle_result = IdleWaitResult(
                        returncode=process.poll(),
                        idle_detected=True,
                        exit_reason="idle_timeout",
                        idle_for_seconds=max(
                            0.0, sample_now - idle_state.last_reset_at
                        ),
                    )
                    if (
                        idle_condition is not None
                        and idle_condition.on_callback is not None
                    ):
                        action, pending_writes = _invoke_condition_callback(
                            idle_condition.on_callback, idle_result, process
                        )
                        _flush_wait_input(process, pending_writes)
                        if action is WaitCallbackResult.CONTINUE:
                            idle_state.last_reset_at = sample_now
                            idle_state.stable_since = None
                            process.last_activity_at = sample_now
                            continue
                        if action is WaitCallbackResult.CONTINUE_AND_DISARM:
                            idle_armed = False
                            continue
                    return WaitForResult(
                        returncode=idle_result.returncode,
                        matched=True,
                        exit_reason="condition_met",
                        condition=idle_condition,
                        idle_result=idle_result,
                    )

            sleep_for = _PTY_POLL_INTERVAL_SECONDS
            if callback_conditions:
                sleep_for = min(
                    sleep_for,
                    min(
                        max(0.001, condition.poll_interval_seconds)
                        for condition in callback_conditions
                    ),
                )
            if deadline is not None:
                sleep_for = min(sleep_for, max(0.0, deadline - time.time()))
            if sleep_for > 0:
                sleep_start = time.perf_counter_ns()
                process._pump_native_output(timeout=sleep_for, consume_all=True)
                sleep_ns += time.perf_counter_ns() - sleep_start
    finally:
        stop_callbacks.set()
        for thread in callback_threads:
            thread.join(timeout=0.2)
