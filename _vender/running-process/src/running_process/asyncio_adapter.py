"""Off-CPU profiling for asyncio, the Python arm of the probe's async tier.

This is adapter #2 of the three the probe's off-CPU design calls for (tokio,
asyncio, and a caller-supplied producer). It answers the question a CPU
profile cannot: *where is this program waiting?* A request that spent nine
seconds awaiting a socket and one second computing looks, in a CPU profile,
like one second of work.

Why sampling rather than instrumentation
----------------------------------------
asyncio exposes no per-task idle/busy accounting, and adding it would mean
wrapping every coroutine — a cost the application pays continuously whether or
not anyone is profiling. Sampling ``asyncio.all_tasks()`` on an interval costs
nothing until a profile is asked for, and off-CPU time is a *duration*
question: a task observed pending across N samples was waiting for roughly N
intervals. That is exactly the accuracy a flame graph needs, and no more.

What a task's stack means here
------------------------------
The coroutine stack, not the thread stack. Every asyncio task shares one
thread, and that thread's stack at any instant is the event loop's — identical
for every task and therefore useless for telling them apart. The coroutine
frames are what distinguish "waiting in fetch_user" from "waiting in
write_audit_log".

Output
------
Collapsed stacks (``a;b;c <weight>``), the format the probe daemon's flame
graph already consumes. Nothing here is asyncio-specific downstream: the same
renderer draws CPU, heap, and tokio profiles.
"""

from __future__ import annotations

import asyncio
import dataclasses
import time
from collections import defaultdict

# Frames below this are the event loop's own plumbing. They appear at the root
# of every single task, so keeping them would put one enormous box at the base
# of the flame graph that conveys nothing and squeezes everything else.
_LOOP_INTERNALS = (
    "asyncio/base_events.py",
    "asyncio/events.py",
    "asyncio/futures.py",
    "asyncio/tasks.py",
    "asyncio/runners.py",
    "asyncio/selector_events.py",
    "asyncio/proactor_events.py",
    "asyncio/windows_events.py",
    "asyncio/unix_events.py",
)

# Hard ceiling on a sampling session, matching the probe's CPU and off-CPU
# profilers. Not a default a caller can raise: an unbounded session is one
# someone can start, forget, and leave sampling a production process forever.
MAX_DURATION_SECONDS = 60.0

# Below this the sampler's own overhead stops being negligible against the
# interval, and the profile starts measuring the profiler.
MIN_INTERVAL_SECONDS = 0.001


@dataclasses.dataclass(frozen=True)
class TaskSample:
    """One task's observed state at one sampling instant."""

    name: str
    stack: tuple[str, ...]
    """Coroutine frames, root first."""
    pending: bool
    """Whether the task was waiting rather than running."""


@dataclasses.dataclass
class AsyncProfile:
    """What a sampling session observed."""

    idle_by_stack: dict[tuple[str, ...], int] = dataclasses.field(default_factory=dict)
    """Samples in which a stack was observed waiting."""
    samples_taken: int = 0
    tasks_seen: int = 0
    duration_seconds: float = 0.0
    interval_seconds: float = 0.0
    clamped: bool = False
    """Whether the request was reduced to fit the enforced bounds."""

    @property
    def idle_nanos_by_stack(self) -> dict[tuple[str, ...], int]:
        """Idle samples converted to nanoseconds.

        A sample is an observation that a task was waiting *at that instant*;
        multiplying by the interval turns a count into the duration it stands
        for, which is what the flame graph weights by.
        """
        per_sample = int(self.interval_seconds * 1_000_000_000)
        return {stack: count * per_sample for stack, count in self.idle_by_stack.items()}


def clamp_duration(seconds: float) -> tuple[float, bool]:
    """Bring a requested duration inside the enforced ceiling.

    Clamps rather than refuses: someone who asked for five minutes wants a
    profile, and sixty seconds of one is a better answer than an error. The
    reduction is reported back rather than quietly substituted.
    """
    if seconds > MAX_DURATION_SECONDS:
        return MAX_DURATION_SECONDS, True
    return max(seconds, 0.0), False


def _is_loop_internal(filename: str) -> bool:
    normalized = filename.replace("\\", "/")
    return any(marker in normalized for marker in _LOOP_INTERNALS)


def task_stack(task: asyncio.Task) -> tuple[str, ...]:
    """Coroutine frames for `task`, root first, loop plumbing removed.

    Returns an empty tuple when the task has no inspectable stack — a task
    that just completed, or one whose coroutine was written in C. That is
    reported as "no stack" rather than folded under a synthetic root, because
    an invented root would collect unrelated tasks into one misleading box.
    """
    frames: list[str] = []
    try:
        stack = task.get_stack()
    except (AttributeError, RuntimeError):
        # A task can finish between enumeration and inspection. Its absence is
        # a normal outcome of sampling a live program, not an error.
        return ()

    for frame in stack:
        code = frame.f_code
        if _is_loop_internal(code.co_filename):
            continue
        frames.append(f"{code.co_name} ({_short_path(code.co_filename)}:{frame.f_lineno})")

    # `get_stack` returns innermost-first; flame graphs are drawn root-first.
    frames.reverse()
    return tuple(frames)


def _short_path(path: str) -> str:
    """Last two path components, which is enough to disambiguate without noise."""
    parts = path.replace("\\", "/").rsplit("/", 2)
    return "/".join(parts[-2:]) if len(parts) > 1 else path


def sample_once(loop: asyncio.AbstractEventLoop | None = None) -> list[TaskSample]:
    """Observe every live task once.

    A task is counted as *waiting* when it is neither done nor currently
    executing. `current_task` is excluded because it is the sampler itself (or
    whatever invoked it) — counting the observer as waiting would put the
    profiler in its own profile.
    """
    try:
        tasks = asyncio.all_tasks(loop) if loop is not None else asyncio.all_tasks()
    except RuntimeError:
        # No running loop. A legitimate state to sample from, not an error.
        return []

    try:
        current = asyncio.current_task(loop) if loop is not None else asyncio.current_task()
    except RuntimeError:
        current = None

    samples: list[TaskSample] = []
    for task in tasks:
        if task is current or task.done():
            continue
        stack = task_stack(task)
        if not stack:
            continue
        samples.append(
            TaskSample(name=_task_name(task), stack=stack, pending=True)
        )
    return samples


def _task_name(task: asyncio.Task) -> str:
    try:
        return task.get_name()
    except AttributeError:
        return repr(task)


async def profile(
    duration_seconds: float = 5.0,
    interval_seconds: float = 0.01,
) -> AsyncProfile:
    """Sample the running loop for `duration_seconds`.

    Runs as a task on the loop it is measuring, which is the only way to see
    `all_tasks()` — and is why each sample is deliberately cheap: the sampler
    competes with the very tasks it is observing, so anything expensive here
    would distort the answer.
    """
    duration, clamped = clamp_duration(duration_seconds)
    interval = max(interval_seconds, MIN_INTERVAL_SECONDS)

    idle: dict[tuple[str, ...], int] = defaultdict(int)
    names: set[str] = set()
    started = time.perf_counter()
    samples_taken = 0
    deadline = started + duration

    while time.perf_counter() < deadline:
        for sample in sample_once():
            idle[sample.stack] += 1
            names.add(sample.name)
        samples_taken += 1
        # Yield to the loop rather than blocking it. `sleep` is what lets the
        # tasks being profiled actually run; a blocking wait here would make
        # every task look permanently idle, which is the profile the sampler
        # itself created.
        await asyncio.sleep(interval)

    return AsyncProfile(
        idle_by_stack=dict(idle),
        samples_taken=samples_taken,
        tasks_seen=len(names),
        duration_seconds=time.perf_counter() - started,
        interval_seconds=interval,
        clamped=clamped,
    )


def to_collapsed(profile_result: AsyncProfile) -> str:
    """Render as collapsed stacks weighted by idle nanoseconds.

    The format the probe daemon's flame graph consumes, identical to what its
    CPU and heap profilers emit — so nothing downstream is asyncio-specific.
    """
    rows = sorted(
        profile_result.idle_nanos_by_stack.items(),
        key=lambda row: (-row[1], row[0]),
    )
    lines = []
    for stack, nanos in rows:
        if nanos <= 0:
            continue
        # Semicolons separate frames and the format has no escape syntax, so
        # one inside a function or file name would forge a frame and reparent
        # everything beneath it.
        lines.append(";".join(frame.replace(";", ":") for frame in stack) + f" {nanos}")
    return "\n".join(lines) + ("\n" if lines else "")
