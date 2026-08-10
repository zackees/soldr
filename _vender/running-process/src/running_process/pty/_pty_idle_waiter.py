"""Pseudo-terminal idle-wait helpers extracted from ``PseudoTerminalProcess``."""

from __future__ import annotations

import sys
import threading
import time
import weakref
from contextlib import suppress
from typing import TYPE_CHECKING

from running_process._native import NativeIdleDetector
from running_process.exit_status import ProcessAbnormalExit, classify_exit_status
from running_process.pty._idle_helpers import (
    _build_default_idle_reset,
    _compile_idle_detector,
    _start_event_count,
)
from running_process.pty._idle_state import _IdleRuntimeState, _IdleSample
from running_process.pty._types import (
    IdleContext,
    IdleDecision,
    IdleDetection,
    IdleDetector,
    IdleInfoDiff,
    IdleStartTrigger,
    IdleWaitResult,
    ProcessIdleDetection,
    PtyIdleDetection,
)

if TYPE_CHECKING:
    from running_process.pty._pseudo_terminal import PseudoTerminalProcess


_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS = 2.0


def wait_for_idle(
    process: PseudoTerminalProcess,
    idle_detector: IdleDetector | None = None,
    *,
    timeout: float | None = None,
    raise_on_abnormal_exit: bool = False,
    echo_output: bool = False,
) -> IdleWaitResult:
    from running_process.pty._pseudo_terminal import KEYBOARD_INTERRUPT_EXIT_CODES

    if idle_detector is None:
        idle_detector = process._registered_idle_detector
    timing, idle_reached, predicate = _compile_idle_detector(idle_detector)
    if timing is None or (idle_reached is None and predicate is None):
        code = process.wait(
            timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit
        )
        return IdleWaitResult(
            returncode=code,
            idle_detected=False,
            exit_reason="process_exit",
            idle_for_seconds=0.0,
        )

    # The native idle-detector fast path is not safe after the Phase 3
    # reader-thread removal. Native PTY chunks are now staged in Rust and
    # must be pumped through `_handle_native_chunk` from Python to update
    # idle accounting. Until idle orchestration moves fully native, keep
    # the observable behavior correct by using the Python wait loop here.

    default_predicate = (
        _build_default_idle_reset(idle_detector)
        if isinstance(idle_detector, IdleDetection)
        else _build_default_idle_reset(IdleDetection())
    )

    start = time.time()
    deadline = start + timeout if timeout is not None else None
    state = _IdleRuntimeState(last_reset_at=start, stable_since=None)
    idle_timeout_enabled = process.idle_timeout_enabled
    idle_process_cfg = (
        idle_detector.process if isinstance(idle_detector, IdleDetection) else None
    )
    start_trigger = (
        idle_detector.pty.start_trigger
        if isinstance(idle_detector, IdleDetection) and idle_detector.pty is not None
        else IdleStartTrigger.IMMEDIATE
    )
    start_events_seen = _start_event_count(process, start_trigger)
    idle_armed = start_trigger is IdleStartTrigger.IMMEDIATE or start_events_seen > 0
    previous = process._sample_idle_snapshot(process_cfg=idle_process_cfg)

    try:
        while True:
            if echo_output:
                process._echo_to_console(sys.stdout)

            now = time.time()
            if process.idle_timeout_enabled != idle_timeout_enabled:
                idle_timeout_enabled = process.idle_timeout_enabled
                if idle_timeout_enabled:
                    state.last_reset_at = now
                    state.stable_since = None
            if deadline is not None and now >= deadline:
                return IdleWaitResult(
                    returncode=process.poll(),
                    idle_detected=False,
                    exit_reason="timeout",
                    idle_for_seconds=max(0.0, now - state.last_reset_at),
                )

            wait_timeout = timing.sample_interval_seconds
            if deadline is not None:
                wait_timeout = min(wait_timeout, max(0.0, deadline - now))
            if wait_timeout > 0:
                process._pump_native_output(timeout=wait_timeout, consume_all=True)

            current = process._sample_idle_snapshot(process_cfg=idle_process_cfg)
            diff = IdleInfoDiff(
                delta_seconds=max(0.0, current.sampled_at - previous.sampled_at),
                process_alive=current.process_alive,
                pty_input_bytes=current.pty_input_bytes - previous.pty_input_bytes,
                pty_output_bytes=current.pty_output_bytes - previous.pty_output_bytes,
                pty_control_churn_bytes=(
                    current.pty_control_churn_bytes - previous.pty_control_churn_bytes
                ),
                cpu_percent=current.cpu_percent,
                disk_io_bytes=current.disk_io_bytes - previous.disk_io_bytes,
                network_io_bytes=current.network_io_bytes - previous.network_io_bytes,
            )
            previous = current

            sample_now = current.sampled_at
            if not idle_armed and start_trigger is not IdleStartTrigger.IMMEDIATE:
                current_start_events = _start_event_count(process, start_trigger)
                if current_start_events != start_events_seen:
                    start_events_seen = current_start_events
                    idle_armed = True
                    state.last_reset_at = sample_now
                    state.stable_since = None
                    process.last_activity_at = sample_now
                else:
                    code = current.returncode
                    if code is not None:
                        process._drain_native_until_eof(
                            timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                        )
                        process._finalize("exit")
                        process._exit_status = classify_exit_status(
                            code, KEYBOARD_INTERRUPT_EXIT_CODES
                        )
                        interrupted = code in KEYBOARD_INTERRUPT_EXIT_CODES
                        if (
                            raise_on_abnormal_exit
                            and process._exit_status.abnormal
                            and not interrupted
                        ):
                            raise ProcessAbnormalExit(process._exit_status)
                        return IdleWaitResult(
                            returncode=code,
                            idle_detected=False,
                            exit_reason="interrupt" if interrupted else "process_exit",
                            idle_for_seconds=0.0,
                        )
                    continue

            stable_for = 0.0
            if state.stable_since is not None:
                stable_for = max(0.0, sample_now - state.stable_since)
            ctx = IdleContext(
                idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                stable_for_seconds=stable_for,
                sample_count=state.sample_count,
            )
            state.sample_count += 1

            handled = False
            if idle_reached is not None:
                decision = idle_reached(diff)
                if not isinstance(decision, IdleDecision):
                    raise TypeError("idle_reached callback must return an IdleDecision")
                if decision is IdleDecision.DEFAULT:
                    handled = False
                elif decision is IdleDecision.IS_IDLE:
                    return IdleWaitResult(
                        returncode=process.poll(),
                        idle_detected=True,
                        exit_reason="idle_timeout",
                        idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                    )
                elif decision is IdleDecision.ACTIVE:
                    state.last_reset_at = sample_now
                    state.stable_since = None
                    process.last_activity_at = sample_now
                    handled = True
                elif decision is IdleDecision.BEGIN_IDLE and state.stable_since is None:
                    idle_started_at = max(0.0, sample_now - diff.delta_seconds)
                    state.last_reset_at = idle_started_at
                    state.stable_since = idle_started_at
                    handled = True
                elif decision is IdleDecision.BEGIN_IDLE:
                    handled = True
                if handled and (
                    idle_timeout_enabled
                    and state.stable_since is not None
                    and max(0.0, sample_now - state.last_reset_at)
                    >= timing.timeout_seconds
                ):
                    return IdleWaitResult(
                        returncode=process.poll(),
                        idle_detected=True,
                        exit_reason="idle_timeout",
                        idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                    )
            if not handled:
                if (predicate is not None and predicate(diff, ctx)) or (
                    idle_reached is not None and default_predicate(diff, ctx)
                ):
                    state.last_reset_at = sample_now
                    state.stable_since = None
                    process.last_activity_at = sample_now
                else:
                    if state.stable_since is None:
                        state.stable_since = sample_now
                    idle_for = max(0.0, sample_now - state.last_reset_at)
                    stable_for = max(0.0, sample_now - state.stable_since)
                    if (
                        idle_timeout_enabled
                        and idle_for >= timing.timeout_seconds
                        and stable_for >= timing.stability_window_seconds
                    ):
                        return IdleWaitResult(
                            returncode=process.poll(),
                            idle_detected=True,
                            exit_reason="idle_timeout",
                            idle_for_seconds=idle_for,
                        )

            code = current.returncode
            if code is not None:
                process._drain_native_until_eof(
                    timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS
                )
                process._finalize("exit")
                process._exit_status = classify_exit_status(
                    code, KEYBOARD_INTERRUPT_EXIT_CODES
                )
                interrupted = code in KEYBOARD_INTERRUPT_EXIT_CODES
                if (
                    raise_on_abnormal_exit
                    and process._exit_status.abnormal
                    and not interrupted
                ):
                    raise ProcessAbnormalExit(process._exit_status)
                return IdleWaitResult(
                    returncode=code,
                    idle_detected=False,
                    exit_reason="interrupt" if interrupted else "process_exit",
                    idle_for_seconds=max(0.0, sample_now - state.last_reset_at),
                )
    finally:
        pass


def sample_idle_snapshot(
    process: PseudoTerminalProcess,
    process_cfg: ProcessIdleDetection | None,
) -> _IdleSample:
    process._sync_native_input_metrics()
    now = time.time()
    cpu_percent = 0.0
    disk_io_bytes = 0
    network_io_bytes = 0
    if process_cfg is not None and process._native_process_metrics is not None:
        process_alive, cpu_percent, disk_io_bytes, network_io_bytes = (
            process._native_process_metrics.sample()
        )
    else:
        process_alive = process.poll() is None

    # Read output accounting from Rust reader thread (atomic counters).
    output_bytes = process._pty_output_bytes_total
    churn_bytes = process._pty_control_churn_bytes_total
    if process._proc is not None:
        with suppress(AttributeError):
            output_bytes = int(process._proc.pty_output_bytes_total())
        with suppress(AttributeError):
            churn_bytes = int(process._proc.pty_control_churn_bytes_total())

    return _IdleSample(
        sampled_at=now,
        process_alive=process_alive,
        pty_input_bytes=process._pty_input_bytes_total,
        pty_output_bytes=output_bytes,
        pty_control_churn_bytes=churn_bytes,
        cpu_percent=cpu_percent,
        disk_io_bytes=disk_io_bytes,
        network_io_bytes=network_io_bytes,
        returncode=process.poll(),
    )


def wait_for_idle_native(
    process: PseudoTerminalProcess,
    idle_detector: IdleDetection,
    *,
    timeout: float | None,
) -> IdleWaitResult:
    from running_process.pty._pseudo_terminal import KEYBOARD_INTERRUPT_EXIT_CODES

    pty_cfg = idle_detector.pty or PtyIdleDetection()
    initial_idle_for = 0.0
    if process.last_activity_at is not None:
        initial_idle_for = max(0.0, time.time() - process.last_activity_at)
    process._native_idle_detector = NativeIdleDetector(
        idle_detector.timing.timeout_seconds,
        idle_detector.timing.stability_window_seconds,
        idle_detector.timing.sample_interval_seconds,
        process._idle_timeout_signal._native,
        pty_cfg.reset_on_input,
        pty_cfg.reset_on_output,
        pty_cfg.count_control_churn_as_output,
        initial_idle_for,
    )
    _start_native_exit_watcher(process)
    idle_detected, reason, idle_for_seconds, returncode = (
        process._native_idle_detector.wait(timeout=timeout)
    )
    process._native_idle_detector = None
    if returncode is not None:
        process._drain_native_until_eof(timeout=_PTY_READER_NATIVE_CLOSE_WAIT_SECONDS)
        process._finalize("exit")
        process._exit_status = classify_exit_status(
            returncode, KEYBOARD_INTERRUPT_EXIT_CODES
        )
    return IdleWaitResult(
        returncode=returncode,
        idle_detected=idle_detected,
        exit_reason=reason,  # type: ignore[arg-type]
        idle_for_seconds=idle_for_seconds,
    )


def _start_native_exit_watcher(process: PseudoTerminalProcess) -> None:
    from running_process.pty._pseudo_terminal import KEYBOARD_INTERRUPT_EXIT_CODES

    detector = process._native_idle_detector
    if detector is None:
        return
    process_ref = weakref.ref(process)

    def watch_for_exit() -> None:
        while True:
            ref = process_ref()
            if ref is None:
                return
            code = ref.poll()
            if code is not None:
                detector.mark_exit(code, code in KEYBOARD_INTERRUPT_EXIT_CODES)
                return
            # #199: intentional — exit-detection cadence on a
            # background watcher thread. 50ms gives sub-100ms
            # latency on the user-visible idle-callback fire path
            # while keeping CPU cost at ~20 polls/sec.
            time.sleep(0.05)

    process._native_exit_watcher = threading.Thread(
        target=watch_for_exit,
        daemon=True,
        name=f"pty-exit-watcher-{process.pid or 'pending'}",
    )
    process._native_exit_watcher.start()
