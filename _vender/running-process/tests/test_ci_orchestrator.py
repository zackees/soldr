"""Tests for the `python -m ci <stage>` dispatcher (#516)."""

from __future__ import annotations

import importlib
import inspect
import unittest

from ci.__main__ import STAGES, main, run_stage


class TestStageRegistry(unittest.TestCase):
    """The registry must describe modules that actually exist.

    A stage naming a module that was renamed or deleted would only surface
    when someone ran that stage -- and the whole point of this entry point is
    that a contributor reaches for it precisely when something is already
    broken. Failing here instead keeps that from being the second surprise.
    """

    def test_every_stage_resolves_to_a_module_with_a_main(self) -> None:
        for stage, module_name in STAGES.items():
            with self.subTest(stage=stage):
                module = importlib.import_module(module_name)
                self.assertTrue(
                    callable(getattr(module, "main", None)),
                    f"{module_name} has no callable main()",
                )

    def test_every_stage_main_accepts_the_dispatcher_calling_convention(self) -> None:
        # The dispatcher calls `main(argv)` when the signature has parameters
        # and `main()` when it does not. Asserting the parameter *count* was
        # the wrong model: `build_wheel.main(argv=None, *, default_mode=...)`
        # has two parameters and is still perfectly callable as `main(argv)`.
        #
        # So bind against the real convention. That is the thing that would
        # actually raise TypeError at the worst moment.
        for stage, module_name in STAGES.items():
            with self.subTest(stage=stage):
                signature = inspect.signature(importlib.import_module(module_name).main)
                try:
                    if signature.parameters:
                        signature.bind([])
                    else:
                        signature.bind()
                except TypeError as error:  # pragma: no cover - the failure path
                    self.fail(f"{module_name}.main cannot be called by the dispatcher: {error}")

    def test_stage_names_are_command_line_shaped(self) -> None:
        # These are typed by hand. Underscores and capitals are the kind of
        # inconsistency that makes a CLI annoying in a way nobody reports.
        for stage in STAGES:
            with self.subTest(stage=stage):
                self.assertRegex(stage, r"^[a-z][a-z0-9-]*$")


class TestDispatch(unittest.TestCase):
    def test_no_arguments_prints_usage_and_fails(self) -> None:
        # Exit 2, not 0: a bare `python -m ci` in a script is a mistake, and
        # succeeding silently would let a workflow "pass" having run nothing.
        self.assertEqual(main([]), 2)

    def test_help_prints_usage_and_succeeds(self) -> None:
        self.assertEqual(main(["--help"]), 0)

    def test_an_unknown_stage_is_refused(self) -> None:
        self.assertEqual(main(["definitely-not-a-stage"]), 2)

    def test_a_failing_stage_propagates_its_exit_code(self) -> None:
        # The contract that matters most for a CI entry point: a stage that
        # fails must fail the process. Swallowing it would turn a red job
        # green, which is the one bug in a dispatcher nobody notices until it
        # has been hiding failures for a while.
        import sys
        import types

        module = types.ModuleType("ci._fake_failing_stage")
        module.main = lambda: 3  # type: ignore[attr-defined]
        sys.modules["ci._fake_failing_stage"] = module
        STAGES["fake-failing"] = "ci._fake_failing_stage"
        try:
            self.assertEqual(run_stage("fake-failing", []), 3)
        finally:
            del STAGES["fake-failing"]
            del sys.modules["ci._fake_failing_stage"]

    def test_a_stage_returning_none_is_treated_as_success(self) -> None:
        # Several `main`s return None on success rather than 0. Reading that
        # as a failure would make every one of them look broken.
        import sys
        import types

        module = types.ModuleType("ci._fake_quiet_stage")
        module.main = lambda: None  # type: ignore[attr-defined]
        sys.modules["ci._fake_quiet_stage"] = module
        STAGES["fake-quiet"] = "ci._fake_quiet_stage"
        try:
            self.assertEqual(run_stage("fake-quiet", []), 0)
        finally:
            del STAGES["fake-quiet"]
            del sys.modules["ci._fake_quiet_stage"]

    def test_arguments_to_an_argless_stage_are_refused_not_dropped(self) -> None:
        # Silently ignoring them reads as "the flag had no effect" rather than
        # "the flag was never delivered", which is a much harder thing to
        # notice from a CI log.
        argless = [
            stage
            for stage, module_name in STAGES.items()
            if not inspect.signature(importlib.import_module(module_name).main).parameters
        ]
        self.assertTrue(argless, "expected at least one stage whose main takes no argv")
        self.assertEqual(run_stage(argless[0], ["--some-flag"]), 2)


if __name__ == "__main__":
    unittest.main()
