"""Tests for the asyncio off-CPU adapter.

The load-bearing property is an inversion: a task that spends its time
*waiting* must dominate the profile even though a task next to it burned far
more CPU. If that inversion does not hold, the adapter is just a slower CPU
profiler.
"""

from __future__ import annotations

import asyncio
import unittest

from running_process.asyncio_adapter import (
    MAX_DURATION_SECONDS,
    AsyncProfile,
    clamp_duration,
    profile,
    sample_once,
    task_stack,
    to_collapsed,
)


async def waits_a_long_time() -> None:
    """Off-CPU: sleeping, using no CPU at all."""
    await asyncio.sleep(0.4)


async def burns_cpu() -> None:
    """On-CPU: busy, yielding often enough not to starve the loop."""
    total = 0
    for _ in range(60):
        for i in range(2000):
            total += i * i
        await asyncio.sleep(0)
    assert total > 0


class TestBounds(unittest.TestCase):
    def test_a_duration_over_the_cap_is_clamped_and_reported(self) -> None:
        # A cap, not a raisable default: an unbounded session is one someone
        # can start, forget, and leave sampling a production process forever.
        seconds, clamped = clamp_duration(3600.0)
        self.assertEqual(seconds, MAX_DURATION_SECONDS)
        self.assertTrue(clamped)

    def test_a_duration_under_the_cap_is_untouched(self) -> None:
        seconds, clamped = clamp_duration(5.0)
        self.assertEqual(seconds, 5.0)
        self.assertFalse(clamped)

    def test_a_negative_duration_becomes_zero_rather_than_looping_forever(self) -> None:
        seconds, clamped = clamp_duration(-3.0)
        self.assertEqual(seconds, 0.0)
        self.assertFalse(clamped)


class TestStacks(unittest.TestCase):
    def test_a_stack_is_root_first_and_names_the_coroutine(self) -> None:
        async def run() -> tuple[str, ...]:
            task = asyncio.ensure_future(waits_a_long_time())
            await asyncio.sleep(0.02)
            stack = task_stack(task)
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            return stack

        stack = asyncio.run(run())
        self.assertTrue(stack, "a sleeping task must have an inspectable stack")
        joined = " ".join(stack)
        self.assertIn("waits_a_long_time", joined)

    def test_event_loop_plumbing_is_stripped(self) -> None:
        # Loop internals sit at the root of *every* task, so keeping them
        # would put one enormous box at the base of the graph conveying
        # nothing and squeezing everything else.
        async def run() -> tuple[str, ...]:
            task = asyncio.ensure_future(waits_a_long_time())
            await asyncio.sleep(0.02)
            stack = task_stack(task)
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass
            return stack

        for frame in asyncio.run(run()):
            self.assertNotIn("base_events.py", frame)
            self.assertNotIn("asyncio/tasks.py", frame.replace("\\", "/"))

    def test_sampling_outside_a_loop_returns_nothing_rather_than_raising(self) -> None:
        # A legitimate state to sample from, not an error.
        self.assertEqual(sample_once(), [])

    def test_the_sampler_does_not_appear_in_its_own_profile(self) -> None:
        async def run() -> list[str]:
            return [sample.name for sample in sample_once()]

        # The only live task is the one calling `sample_once`, and it is
        # excluded as `current_task` — so nothing is reported.
        self.assertEqual(asyncio.run(run()), [])


class TestProfiling(unittest.TestCase):
    def test_the_waiting_task_dominates_even_though_another_burned_more_cpu(self) -> None:
        # The inversion that justifies this adapter existing. A CPU profile
        # would rank these the other way round.
        async def run() -> AsyncProfile:
            waiting = asyncio.ensure_future(waits_a_long_time())
            busy = asyncio.ensure_future(burns_cpu())
            result = await profile(duration_seconds=0.3, interval_seconds=0.005)
            for task in (waiting, busy):
                task.cancel()
            await asyncio.gather(waiting, busy, return_exceptions=True)
            return result

        result = asyncio.run(run())
        self.assertGreater(result.samples_taken, 0)
        self.assertGreater(result.tasks_seen, 0)

        collapsed = to_collapsed(result)
        self.assertTrue(collapsed, "a profile with tasks must render something")
        hottest = collapsed.splitlines()[0]
        self.assertIn(
            "waits_a_long_time",
            hottest,
            f"the waiting task should dominate the off-CPU view; got {collapsed!r}",
        )

    def test_idle_samples_convert_to_the_duration_they_stand_for(self) -> None:
        # A sample is an observation that a task was waiting at that instant;
        # multiplying by the interval turns a count into a duration, which is
        # what the flame graph weights by.
        result = AsyncProfile(
            idle_by_stack={("main", "wait"): 10},
            samples_taken=10,
            tasks_seen=1,
            duration_seconds=0.1,
            interval_seconds=0.01,
        )
        self.assertEqual(
            result.idle_nanos_by_stack[("main", "wait")], 10 * 10_000_000
        )

    def test_a_profile_of_an_idle_loop_is_empty_rather_than_invented(self) -> None:
        async def run() -> AsyncProfile:
            return await profile(duration_seconds=0.05, interval_seconds=0.005)

        result = asyncio.run(run())
        self.assertEqual(result.idle_by_stack, {})
        self.assertEqual(to_collapsed(result), "")


class TestCollapsedRendering(unittest.TestCase):
    def test_output_is_hottest_first(self) -> None:
        result = AsyncProfile(
            idle_by_stack={("main", "slow"): 9, ("main", "quick"): 1},
            interval_seconds=0.001,
        )
        lines = to_collapsed(result).splitlines()
        self.assertEqual(lines[0], "main;slow 9000000")
        self.assertEqual(lines[1], "main;quick 1000000")

    def test_a_semicolon_in_a_frame_cannot_forge_a_frame(self) -> None:
        # The collapsed format has no escape syntax, so one would reparent
        # everything beneath it.
        result = AsyncProfile(
            idle_by_stack={("handler (a;b.py:3)",): 1},
            interval_seconds=0.001,
        )
        rendered = to_collapsed(result)
        self.assertIn("a:b.py", rendered)
        self.assertNotIn("a;b.py", rendered)

    def test_zero_weight_stacks_are_not_drawn(self) -> None:
        result = AsyncProfile(idle_by_stack={("x",): 0}, interval_seconds=0.001)
        self.assertEqual(to_collapsed(result), "")


if __name__ == "__main__":
    unittest.main()
