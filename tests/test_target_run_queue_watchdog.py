"""Regression coverage for the independent queue-starvation observer (#2968)."""

from __future__ import annotations

import importlib.util
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Protocol

SCRIPT = Path(__file__).parents[1] / ".github" / "scripts" / "target_run_queue_watchdog.py"
SPEC = importlib.util.spec_from_file_location("target_run_queue_watchdog", SCRIPT)
assert SPEC and SPEC.loader
watchdog = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = watchdog
SPEC.loader.exec_module(watchdog)


PREFIX = "target-run x86_64-apple-darwin"


BASE = datetime(2026, 8, 28, 5, 0, tzinfo=UTC)


class WatchVerdict(Protocol):  # pylint: disable=too-few-public-methods
    """Attribute-only watchdog result view used by this test module."""

    ok: bool
    message: str


def job(
    status: str,
    conclusion: str | None = None,
    *,
    created_at: datetime = BASE,
) -> dict[str, object]:
    return {
        "name": f"{PREFIX} (all)",
        "status": status,
        "conclusion": conclusion,
        "created_at": created_at.isoformat().replace("+00:00", "Z"),
    }


def run(
    sequence: list[list[dict[str, object]]], *, grace: float = 30, now: datetime
) -> WatchVerdict:
    responses = iter(sequence)
    return watchdog.watch_for_start(
        lambda: next(responses),
        job_prefix=PREFIX,
        runner="macos-15",
        grace_seconds=grace,
        poll_seconds=10,
        now=lambda: now,
        sleep=lambda _: None,
    )


def test_queued_target_run_exceeds_bounded_grace_with_runner_diagnostics() -> None:
    verdict = run([[job("queued")]], now=BASE + timedelta(seconds=30))
    assert not verdict.ok
    assert "queue starvation exceeded 30s" in verdict.message
    assert "runner='macos-15'" in verdict.message
    assert "queue_age=30s" in verdict.message
    assert "created_at='2026-08-28T05:00:00Z'" in verdict.message


def test_started_target_run_satisfies_watchdog_without_waiting_for_completion() -> None:
    verdict = run([[job("queued")], [job("in_progress")]], now=BASE + timedelta(seconds=10))
    assert verdict.ok
    assert "started" in verdict.message


def test_completed_failed_target_run_is_reported_as_failure() -> None:
    verdict = run([[job("completed", "failure")]], now=BASE + timedelta(seconds=1))
    assert not verdict.ok
    assert "completed unsuccessfully" in verdict.message
    assert "conclusion='failure'" in verdict.message


def test_completed_success_target_run_is_accepted() -> None:
    verdict = run([[job("completed", "success")]], now=BASE + timedelta(seconds=1))
    assert verdict.ok
    assert "completed successfully" in verdict.message


def test_api_error_is_a_diagnostic_failure() -> None:
    verdict = watchdog.watch_for_start(
        lambda: (_ for _ in ()).throw(OSError("service unavailable")),
        job_prefix=PREFIX,
        runner="macos-15",
        grace_seconds=30,
        poll_seconds=10,
    )
    assert not verdict.ok
    assert "GitHub API error" in verdict.message


def test_missing_created_timestamp_is_an_api_diagnostic_failure() -> None:
    missing_timestamp = {"name": f"{PREFIX} (all)", "status": "queued"}
    verdict = watchdog.watch_for_start(
        lambda: [missing_timestamp],
        job_prefix=PREFIX,
        runner="macos-15",
        grace_seconds=30,
        poll_seconds=10,
    )
    assert not verdict.ok
    assert "missing created_at" in verdict.message


def test_missing_job_can_materialize_before_visibility_grace() -> None:
    responses = iter([[], [job("in_progress")]])
    ticks = iter([0.0, 0.0, 5.0])
    verdict = watchdog.watch_for_start(
        lambda: next(responses),
        job_prefix=PREFIX,
        runner="macos-15",
        grace_seconds=30,
        poll_seconds=10,
        now=lambda: BASE + timedelta(seconds=5),
        monotonic=lambda: next(ticks),
        sleep=lambda _: None,
    )
    assert verdict.ok
    assert "started" in verdict.message


def test_missing_job_exceeds_visibility_grace_with_runner_diagnostics() -> None:
    ticks = iter([0.0, 0.0, 30.0])
    verdict = watchdog.watch_for_start(
        lambda: [],
        job_prefix=PREFIX,
        runner="macos-15",
        grace_seconds=30,
        poll_seconds=10,
        monotonic=lambda: next(ticks),
        sleep=lambda _: None,
    )
    assert not verdict.ok
    assert "did not materialize" in verdict.message
    assert "visibility_age=30s" in verdict.message
