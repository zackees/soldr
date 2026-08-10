"""wait_for_idle variants: dataclass config, callable predicate, stability window, triggers."""

from __future__ import annotations

import contextlib
import os
import sys
import threading
import time
from types import SimpleNamespace

import pytest

from running_process import (
    IdleDecision,
    IdleDetection,
    IdleStartTrigger,
    IdleTiming,
    ProcessIdleDetection,
    PtyIdleDetection,
    RunningProcess,
)
from running_process.pty import (
    IdleInfoDiff,
    IdleWaitResult,
    PseudoTerminalProcess,
)


def test_wait_with_idle_detector_none_preserves_int_return_type() -> None:
    process = RunningProcess([sys.executable, "-c", "print('done')"], use_pty=True, text=True)
    result = process.wait(timeout=5, idle_detector=None)
    assert isinstance(result, int)
    assert result == 0


def test_pseudo_terminal_wait_for_idle_uses_dataclass_config(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    snapshots = iter(
        [
            SimpleNamespace(
                sampled_at=0.00,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=0,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.02,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.04,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=10,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.16,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=10,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.22,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=10,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
        ]
    )

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(
        process,
        "_sample_idle_snapshot",
        lambda process_cfg=None: next(snapshots),
    )

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.1,
                stability_window_seconds=0.05,
                sample_interval_seconds=0.02,
            )
        ),
        timeout=1.0,
    )
    assert isinstance(result, IdleWaitResult)
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert result.idle_for_seconds >= 0.1


def test_pseudo_terminal_wait_for_idle_uses_callable_predicate(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen: list[IdleInfoDiff] = []
    snapshots = iter(
        [
            SimpleNamespace(
                sampled_at=0.00,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=0,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.02,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.04,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.08,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
        ]
    )

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(
        process,
        "_sample_idle_snapshot",
        lambda process_cfg=None: next(snapshots),
    )

    def capture(diff: IdleInfoDiff) -> IdleDecision:
        seen.append(diff)
        if diff.pty_output_bytes > 0:
            return IdleDecision.ACTIVE
        if diff.process_alive:
            return IdleDecision.BEGIN_IDLE
        return IdleDecision.IS_IDLE

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            ),
            idle_reached=capture,
        ),
        timeout=1.0,
    )
    assert isinstance(result, IdleWaitResult)
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert any(diff.pty_output_bytes > 0 for diff in seen)
    assert seen
    assert all(item.delta_seconds >= 0.0 for item in seen)


def test_idle_reached_callback_accumulates_diff_when_callback_is_slow(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    seen: list[IdleInfoDiff] = []
    snapshots = iter(
        [
            SimpleNamespace(
                sampled_at=0.00,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=0,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.01,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=0,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.06,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=0,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
        ]
    )

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(
        process,
        "_sample_idle_snapshot",
        lambda process_cfg=None: next(snapshots),
    )

    def capture(diff: IdleInfoDiff) -> IdleDecision:
        seen.append(diff)
        return IdleDecision.BEGIN_IDLE

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.04,
                stability_window_seconds=0.01,
                sample_interval_seconds=0.01,
            ),
            idle_reached=capture,
        ),
        timeout=1.0,
    )
    assert result.idle_detected is True
    assert any(item.delta_seconds >= 0.03 for item in seen)


def test_pseudo_terminal_wait_for_idle_hybrid_config_uses_custom_predicate(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    snapshots = iter(
        [
            SimpleNamespace(
                sampled_at=0.00,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=0,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.02,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.04,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
            SimpleNamespace(
                sampled_at=0.10,
                process_alive=True,
                pty_input_bytes=0,
                pty_output_bytes=5,
                pty_control_churn_bytes=0,
                cpu_percent=0.0,
                disk_io_bytes=0,
                network_io_bytes=0,
                returncode=None,
            ),
        ]
    )

    process = PseudoTerminalProcess(
        [sys.executable, "-c", "print('x')"],
        auto_run=False,
    )
    monkeypatch.setattr(process, "_pump_native_output", lambda timeout, consume_all: None)
    monkeypatch.setattr(
        process,
        "_sample_idle_snapshot",
        lambda process_cfg=None: next(snapshots),
    )

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.08,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            ),
            idle_reached=lambda diff: (
                IdleDecision.BEGIN_IDLE
                if diff.pty_output_bytes == 0 and diff.delta_seconds >= 0.02
                else IdleDecision.DEFAULT
            ),
        ),
        timeout=1.0,
    )
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"


def test_pseudo_terminal_wait_for_idle_reports_process_exit_before_idle() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.05)"],
        text=True,
    )
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.4,
                stability_window_seconds=0.05,
                sample_interval_seconds=0.02,
            ),
            idle_reached=lambda _diff: IdleDecision.ACTIVE,
        ),
        timeout=1.0,
    )
    assert result.idle_detected is False
    assert result.exit_reason == "process_exit"
    assert result.returncode == 0


def test_pseudo_terminal_wait_for_idle_honors_stability_window() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            ("import sys, time\nprint('start', flush=True)\ntime.sleep(0.4)\n"),
        ],
        text=True,
    )
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.15,
                sample_interval_seconds=0.02,
            )
        ),
        timeout=1.0,
    )
    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert result.idle_for_seconds >= 0.15
    process.kill()


def test_pseudo_terminal_wait_for_idle_passes_diff_and_context_to_predicate() -> None:
    from running_process.pty import IdleContext, IdleDiff

    seen: list[tuple[IdleDiff, IdleContext]] = []

    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.3)"],
        text=True,
    )

    def capture(diff: IdleDiff, ctx: IdleContext) -> bool:
        seen.append((diff, ctx))
        return False

    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.05,
                stability_window_seconds=0.02,
                sample_interval_seconds=0.02,
            ),
            predicate=capture,
        ),
        timeout=1.0,
    )
    assert result.exit_reason == "idle_timeout"
    assert seen
    assert all(item[0].process_alive is True for item in seen[:1])


def test_idle_detection_rejects_conflicting_custom_callback_fields() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.2)"],
        text=True,
    )
    with pytest.raises(ValueError, match="mutually exclusive"):
        process.wait_for_idle(
            IdleDetection(
                idle_reached=lambda _diff: IdleDecision.ACTIVE,
                predicate=lambda _diff, _ctx: False,
            ),
            timeout=0.1,
        )
    process.kill()


def test_idle_reached_callback_requires_idle_decision_result() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(0.2)"],
        text=True,
    )
    with pytest.raises(TypeError, match="IdleDecision"):
        process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=5.0,
                    stability_window_seconds=0.01,
                    sample_interval_seconds=0.01,
                ),
                idle_reached=lambda _diff: False,  # type: ignore[return-value]
            ),
            timeout=0.2,
        )
    process.kill()


def test_pseudo_terminal_idle_timeout_signal_can_be_reenabled_during_wait() -> None:
    process = RunningProcess.pseudo_terminal(
        [sys.executable, "-c", "import time; time.sleep(1.5)"],
        text=True,
    )
    process.idle_timeout_enabled = False

    def enable_later() -> None:
        time.sleep(0.3)
        process.idle_timeout_enabled = True

    worker = threading.Thread(target=enable_later, daemon=True)
    worker.start()
    started = time.time()
    result = process.wait_for_idle(
        IdleDetection(
            timing=IdleTiming(
                timeout_seconds=0.2,
                stability_window_seconds=0.1,
                sample_interval_seconds=0.05,
            )
        ),
        timeout=2.0,
    )
    elapsed = time.time() - started
    worker.join(timeout=2.0)

    assert result.idle_detected is True
    assert result.exit_reason == "idle_timeout"
    assert elapsed >= 0.3
    process.kill()


def test_pseudo_terminal_wait_for_idle_does_not_arm_input_submit_on_newline_bytes() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n")

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                ),
                pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_SUBMIT),
            ),
            timeout=0.8,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.idle_detected is False
        assert result.exit_reason == "process_exit"
        assert elapsed >= 0.25
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_idle_can_arm_on_explicit_input_submit() -> None:
    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n", submit=True)

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=0.05,
                    stability_window_seconds=0.02,
                    sample_interval_seconds=0.01,
                ),
                pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_SUBMIT),
            ),
            # A harness bound on how long to wait for an answer, NOT the
            # property under test — that is the three assertions below. Held
            # at 0.35s this failed on loaded CI runners (#723) because the
            # budget expired before the ~0.19s detection could happen, which
            # says nothing about idle detection being wrong. Widening it makes
            # the test more deterministic without weakening any assertion:
            # `exit_reason` still distinguishes detection from expiry.
            timeout=2.0,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.idle_detected is True
        # The discriminator: had the outer budget expired instead, this would
        # be "timeout". Widening that budget cannot mask a failure to detect.
        assert result.exit_reason == "idle_timeout"
        # Idle must not be reported instantly — the input submit has to arm it
        # first. This is the lower bound the widened budget does not touch.
        assert elapsed >= 0.15
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_idle_can_arm_on_input_newline() -> None:
    # PTY scheduling on macOS CI runners adds tens of ms of jitter to
    # write→subprocess delivery and to "idle" sampling. On a dev laptop
    # the sub-100 ms idle window resolves cleanly; on macos-15 GH it
    # tightropes the deadline and intermittently misses. Scale every
    # coordinated timing by 5x when GITHUB_ACTIONS=true so the test
    # still exercises the same behaviour with realistic margins.
    scale = 5 if os.environ.get("GITHUB_ACTIONS") == "true" else 1
    sleep_after = 0.3 * scale
    submit_delay = 0.12 * scale
    idle_timeout = 0.05 * scale
    idle_stability = 0.02 * scale
    idle_sample = 0.01 * scale
    outer_timeout = 0.35 * scale
    min_elapsed = 0.15 * scale

    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                f"time.sleep({sleep_after})\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(submit_delay)
        process.write("hello\n")

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for_idle(
            IdleDetection(
                timing=IdleTiming(
                    timeout_seconds=idle_timeout,
                    stability_window_seconds=idle_stability,
                    sample_interval_seconds=idle_sample,
                ),
                pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_NEWLINE),
            ),
            timeout=outer_timeout,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.idle_detected is True
        assert result.exit_reason == "idle_timeout"
        assert elapsed >= min_elapsed
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_wait_for_idle_condition_can_arm_on_explicit_input_submit() -> None:
    from running_process import Idle

    process = RunningProcess.pseudo_terminal(
        [
            sys.executable,
            "-c",
            (
                "import sys, time\n"
                "sys.stdout.write('ready>')\n"
                "sys.stdout.flush()\n"
                "sys.stdin.readline()\n"
                "time.sleep(0.3)\n"
            ),
        ],
        text=True,
    )

    def submit_later() -> None:
        time.sleep(0.12)
        process.write("hello\n", submit=True)

    worker = threading.Thread(target=submit_later, daemon=True)
    worker.start()
    try:
        started = time.time()
        result = process.wait_for(
            Idle(
                IdleDetection(
                    timing=IdleTiming(
                        timeout_seconds=0.05,
                        stability_window_seconds=0.02,
                        sample_interval_seconds=0.01,
                    ),
                    pty=PtyIdleDetection(start_trigger=IdleStartTrigger.INPUT_SUBMIT),
                )
            ),
            # Generous outer ceiling on purpose. This test asks whether
            # arming on INPUT_SUBMIT works, not whether the runner is fast:
            # the correctness claim is guarded by `elapsed >= 0.15` below,
            # which still proves the wait honoured the submit trigger and the
            # stability window rather than returning early.
            #
            # A 0.35 s ceiling made the two claims share one assertion, so a
            # loaded macos-arm runner reported `idle_detected=False` and
            # failed PRs that could not affect PTY behaviour (#723, seen again
            # on #815 where a re-run of identical code passed).
            timeout=5.0,
        )
        elapsed = time.time() - started
        worker.join(timeout=1.0)

        assert result.matched is True
        assert result.exit_reason == "condition_met"
        assert result.idle_result is not None
        assert result.idle_result.idle_detected is True
        assert result.idle_result.exit_reason == "idle_timeout"
        assert elapsed >= 0.15
    finally:
        with contextlib.suppress(Exception):
            worker.join(timeout=1.0)
        with contextlib.suppress(Exception):
            process.kill()


def test_pseudo_terminal_idle_sampling_uses_native_process_metrics() -> None:
    class FakeMetrics:
        def prime(self) -> None:
            return None

        def sample(self) -> tuple[bool, float, int, int]:
            return (True, 7.5, 4096, 0)

    process = PseudoTerminalProcess([sys.executable, "-c", "print('x')"], auto_run=False)
    process._native_process_metrics = FakeMetrics()
    process._pty_input_bytes_total = 2
    process._pty_output_bytes_total = 3
    process._pty_control_churn_bytes_total = 1

    sample = process._sample_idle_snapshot(ProcessIdleDetection())

    assert sample.process_alive is True
    assert sample.cpu_percent == 7.5
    assert sample.disk_io_bytes == 4096
    assert sample.network_io_bytes == 0
