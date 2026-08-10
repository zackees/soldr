from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from enum import Enum
from typing import Literal

from running_process.expect import ExpectAction, ExpectMatch, ExpectPattern


class InteractiveMode(str, Enum):
    PSEUDO_TERMINAL = "pseudo_terminal"
    CONSOLE_SHARED = "console_shared"
    CONSOLE_ISOLATED = "console_isolated"


@dataclass(frozen=True)
class InteractiveLaunchSpec:
    mode: InteractiveMode
    uses_pty: bool
    ctrl_c_owner: str
    creationflags: int | None
    restore_terminal: bool


@dataclass(frozen=True)
class InterruptResult:
    exit_reason: str
    interrupt_count: int
    returncode: int | None


class IdleDecision(str, Enum):
    DEFAULT = "default"
    ACTIVE = "active"
    BEGIN_IDLE = "begin_idle"
    IS_IDLE = "is_idle"


class IdleStartTrigger(str, Enum):
    IMMEDIATE = "immediate"
    INPUT_NEWLINE = "input_newline"
    INPUT_SUBMIT = "input_submit"


@dataclass(slots=True)
class IdleTiming:
    timeout_seconds: float = 10.0
    stability_window_seconds: float = 0.75
    sample_interval_seconds: float = 0.25


@dataclass(slots=True)
class PtyIdleDetection:
    reset_on_input: bool = True
    reset_on_output: bool = True
    count_control_churn_as_output: bool = True
    start_trigger: IdleStartTrigger = IdleStartTrigger.IMMEDIATE


@dataclass(slots=True)
class ProcessIdleDetection:
    cpu_percent_before_reset: float = 2.0
    max_disk_io_bytes_before_reset: int = 4096
    max_network_bytes_before_reset: int = 4096


@dataclass(slots=True)
class IdleInfoDiff:
    delta_seconds: float
    process_alive: bool
    pty_input_bytes: int = 0
    pty_output_bytes: int = 0
    pty_control_churn_bytes: int = 0
    cpu_percent: float = 0.0
    disk_io_bytes: int = 0
    network_io_bytes: int = 0

    @property
    def interval_seconds(self) -> float:
        return self.delta_seconds


@dataclass(slots=True)
class IdleContext:
    idle_for_seconds: float
    stable_for_seconds: float
    sample_count: int


IdleDiff = IdleInfoDiff
IdleReachedCallback = Callable[[IdleInfoDiff], IdleDecision]
IdleResetPredicate = Callable[[IdleInfoDiff, IdleContext], bool]


@dataclass(slots=True)
class IdleDetection:
    timing: IdleTiming = field(default_factory=IdleTiming)
    pty: PtyIdleDetection | None = field(default_factory=PtyIdleDetection)
    process: ProcessIdleDetection | None = None
    idle_reached: IdleReachedCallback | None = None
    predicate: IdleResetPredicate | None = None


IdleDetector = IdleDetection | IdleResetPredicate | None


@dataclass(frozen=True, slots=True)
class IdleWaitResult:
    returncode: int | None
    idle_detected: bool
    exit_reason: Literal["process_exit", "idle_timeout", "timeout", "interrupt"]
    idle_for_seconds: float = 0.0

    @property
    def reason(self) -> str:
        return self.exit_reason

    @property
    def idle_for(self) -> float:
        return self.idle_for_seconds


@dataclass(frozen=True, slots=True)
class Idle:
    detector: IdleDetector = field(default_factory=IdleDetection)
    on_callback: Callable[..., object] | None = None


class WaitCallbackResult(str, Enum):
    EXIT = "exit"
    CONTINUE = "continue"
    CONTINUE_AND_DISARM = "continue_and_disarm"


@dataclass(frozen=True, slots=True)
class WaitCheckpoint:
    offset: int


@dataclass(frozen=True, slots=True)
class Expect:
    pattern: ExpectPattern
    action: ExpectAction = None
    NOT: ExpectPattern | None = None
    after: WaitCheckpoint | Literal["start", "now"] = "start"
    on_callback: Callable[..., object] | None = None


@dataclass(frozen=True, slots=True)
class Callback:
    callback: Callable[..., object]
    poll_interval_seconds: float = 0.05


WaitCondition = Idle | Expect | Callback


@dataclass(frozen=True, slots=True)
class WaitForResult:
    returncode: int | None
    matched: bool
    exit_reason: Literal["condition_met", "process_exit", "timeout", "interrupt"]
    condition: WaitCondition | None = None
    expect_match: ExpectMatch | None = None
    idle_result: IdleWaitResult | None = None
    callback_result: object | None = None

    @property
    def reason(self) -> str:
        return self.exit_reason
