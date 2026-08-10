from __future__ import annotations

import threading
from dataclasses import dataclass, field

from running_process.pty._errors import SignalBool
from running_process.pty._types import IdleDecision, IdleInfoDiff


@dataclass(slots=True)
class _IdleRuntimeState:
    last_reset_at: float
    stable_since: float | None
    sample_count: int = 0


@dataclass(slots=True)
class _IdleSample:
    sampled_at: float
    process_alive: bool
    pty_input_bytes: int
    pty_output_bytes: int
    pty_control_churn_bytes: int
    cpu_percent: float
    disk_io_bytes: int
    network_io_bytes: int
    returncode: int | None


@dataclass(slots=True)
class _IdleCallbackThreadState:
    pending_diff: IdleInfoDiff | None = None
    inflight: bool = False
    latest_decision: IdleDecision | None = None
    error: BaseException | None = None
    closed: bool = False


@dataclass(slots=True)
class _WaitCallbackState:
    ready: SignalBool = field(default_factory=SignalBool)
    result: object | None = None
    error: BaseException | None = None
    pending_writes: list[str | bytes] = field(default_factory=list)
    lock: threading.Lock = field(default_factory=threading.Lock)


@dataclass(slots=True)
class _ExpectRuntimeState:
    search_offset: int = 0
    armed: bool = True
