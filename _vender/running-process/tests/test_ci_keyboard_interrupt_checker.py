"""Tests for the KeyboardInterrupt discipline checker.

The checker was reintroduced after being dropped during the Rust port, and
these pin the patterns that made a verbatim restoration unusable: it flagged
six sites that were all already correct.
"""

import textwrap
import unittest

from ci.lint_python import keyboard_interrupt_checker as kbi


def codes(source: str) -> list[str]:
    return [v.code for v in kbi.check_file("t.py", textwrap.dedent(source))]


class TestFlagsRealViolations(unittest.TestCase):
    """The checker must still catch what it exists to catch."""

    def test_broad_except_without_keyboard_interrupt_is_flagged(self):
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except Exception:
                        log()
                """
            ),
            ["KBI001"],
        )

    def test_bare_except_is_flagged(self):
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except:
                        log()
                """
            ),
            ["KBI001"],
        )

    def test_swallowing_keyboard_interrupt_is_flagged(self):
        # The core failure: an interrupt on a worker thread that never
        # reaches the main thread is an interrupt the user cannot deliver.
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except KeyboardInterrupt:
                        log()
                """
            ),
            ["KBI002"],
        )


class TestAcceptsCorrectPatterns(unittest.TestCase):
    """Patterns the codebase already used, which a verbatim restore flagged."""

    def test_re_raising_is_accepted(self):
        # `raise` propagates the interrupt, which is exactly what
        # handle_keyboard_interrupt does on the main thread.
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except KeyboardInterrupt:
                        cleanup()
                        raise
                """
            ),
            [],
        )

    def test_the_helper_is_accepted(self):
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except KeyboardInterrupt as e:
                        handle_keyboard_interrupt(e)
                """
            ),
            [],
        )

    def test_interrupt_main_is_accepted(self):
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except KeyboardInterrupt:
                        _thread.interrupt_main()
                """
            ),
            [],
        )

    def test_isinstance_dispatch_inside_a_broad_handler_is_accepted(self):
        # A worker that must also record the non-interrupt case naturally
        # writes it this way; demanding a separate clause is a rewrite of
        # working code into a shape that is no safer.
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except BaseException as exc:
                        state.error = exc
                        if isinstance(exc, KeyboardInterrupt):
                            _thread.interrupt_main()
                        return
                """
            ),
            [],
        )

    def test_isinstance_dispatch_that_swallows_is_still_flagged(self):
        # Recognising the shape must not become a way to bypass the rule:
        # dispatching on KeyboardInterrupt and then doing nothing with it is
        # the very thing being guarded against.
        self.assertEqual(
            codes(
                """
                def f():
                    try:
                        work()
                    except BaseException as exc:
                        if isinstance(exc, KeyboardInterrupt):
                            log("interrupted")
                        return
                """
            ),
            ["KBI001"],
        )


class TestSuppression(unittest.TestCase):
    """Deliberate exceptions must be expressible, and must name themselves."""

    def test_noqa_with_the_code_suppresses(self):
        self.assertEqual(
            codes(
                """
                def main():
                    try:
                        serve()
                    except KeyboardInterrupt:  # noqa: KBI002
                        shutdown()
                """
            ),
            [],
        )

    def test_a_bare_noqa_does_not_suppress(self):
        # Silencing this rule should say which rule it is silencing.
        self.assertEqual(
            codes(
                """
                def main():
                    try:
                        serve()
                    except KeyboardInterrupt:  # noqa
                        shutdown()
                """
            ),
            ["KBI002"],
        )

    def test_a_different_code_does_not_suppress(self):
        self.assertEqual(
            codes(
                """
                def main():
                    try:
                        serve()
                    except KeyboardInterrupt:  # noqa: KBI001
                        shutdown()
                """
            ),
            ["KBI002"],
        )


class TestRepositoryIsClean(unittest.TestCase):
    """The gate must pass on the tree it is being added to."""

    def test_src_has_no_violations(self):
        from pathlib import Path

        root = Path(__file__).resolve().parent.parent
        found: list[str] = []
        for path in kbi.collect_python_files([str(root / "src")], [".venv", "dist", ".build"]):
            for violation in kbi.check_file(str(path), path.read_text(encoding="utf-8")):
                found.append(f"{path}:{violation.line} {violation.code}")
        self.assertEqual(found, [], f"unexpected KBI violations: {found}")

class TestRuffDoesNotStripSuppressions(unittest.TestCase):
    """`ruff check --fix` must leave `# noqa: KBI00x` alone.

    Ruff treats a noqa naming a code it does not own as an unused directive
    (RUF100) and `--fix` DELETES it. Since `./lint` runs ruff before this
    checker, that silently disarmed every suppression and then failed on the
    very line that was deliberately exempted. `[tool.ruff.lint] external`
    declares the codes so ruff leaves them alone.
    """

    def test_ruff_config_declares_the_kbi_codes_as_external(self):
        from pathlib import Path

        import tomllib

        root = Path(__file__).resolve().parent.parent
        config = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))
        external = config["tool"]["ruff"]["lint"].get("external", [])
        self.assertIn("KBI001", external)
        self.assertIn("KBI002", external)

    def test_a_kbi_suppression_survives_ruff_fix(self):
        import shutil
        import subprocess
        import sys
        import tempfile
        from pathlib import Path

        ruff = shutil.which("ruff") or str(Path(sys.executable).with_name("ruff"))
        if not Path(ruff).exists():
            self.skipTest("ruff not available")

        root = Path(__file__).resolve().parent.parent
        with tempfile.TemporaryDirectory(dir=root) as tmp:
            # Inside the repo so ruff picks up pyproject.toml's config.
            probe = Path(tmp) / "probe.py"
            probe.write_text(
                "def main():\n"
                "    try:\n"
                "        serve()\n"
                "    except KeyboardInterrupt:  # noqa: KBI002\n"
                "        shutdown()\n",
                encoding="utf-8",
            )
            subprocess.run(
                [ruff, "check", "--fix", str(probe)],
                capture_output=True,
                cwd=root,
                check=False,
            )
            self.assertIn(
                "# noqa: KBI002",
                probe.read_text(encoding="utf-8"),
                "ruff --fix stripped the suppression; ./lint would then fail "
                "on the deliberately exempted line",
            )


if __name__ == "__main__":
    unittest.main()
