"""End-to-end PTY passthrough test for #150 (Python mirror).

Companion to `crates/running-process/tests/daemon_tui_repaint_test.rs`
which exercises the daemon code path. This file exercises the in-
process PTY path through `RunningProcess.pseudo_terminal(...)`.

`testbin-tui-counter` emits raw ANSI clear+home + 10 lines of
`COUNTER: N`. We spawn it under a pseudo-terminal, read until the
last counter appears, then assert byte-exact ANSI markers survived
the trip — proving `PSEUDOCONSOLE_PASSTHROUGH_MODE` is active on
Windows (POSIX PTYs are passthrough by design).
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import cast

REPO_ROOT = Path(__file__).resolve().parent.parent


def _resolve_testbin() -> Path:
    """Locate `testbin-tui-counter` under target/{debug,release}/.

    Skip the test if it hasn't been built. CI builds the workspace
    before running pytest, so the binary is normally present.
    """
    ext = ".exe" if sys.platform == "win32" else ""
    candidates = [
        REPO_ROOT / "target" / "debug" / f"testbin-tui-counter{ext}",
        REPO_ROOT / "target" / "release" / f"testbin-tui-counter{ext}",
        REPO_ROOT
        / "target"
        / "x86_64-pc-windows-msvc"
        / "debug"
        / f"testbin-tui-counter{ext}",
        REPO_ROOT
        / "target"
        / "x86_64-pc-windows-msvc"
        / "release"
        / f"testbin-tui-counter{ext}",
    ]
    for path in candidates:
        if path.exists():
            return path
    raise unittest.SkipTest(
        "testbin-tui-counter not built; "
        "run `cargo build -p testbins --bin testbin-tui-counter` first"
    )


class TestPtyTuiRepaint(unittest.TestCase):
    """Asserts raw ANSI bytes survive the round trip through ConPTY /
    POSIX PTY to the in-process reader."""

    def test_ansi_clear_and_cursor_home_survive_pty(self) -> None:
        # #150 W8: PSEUDOCONSOLE_PASSTHROUGH_MODE is only honored on
        # Windows 11 / Server 2022 (build 22000+). On Win10 ConPTY
        # silently ignores the flag, the master pipe sees only the
        # synthesized DSR query, and byte-exact assertions can't
        # hold. Skip with a clear note; POSIX PTYs run normally.
        if sys.platform == "win32":
            version = sys.getwindowsversion()  # type: ignore[attr-defined]
            if version.build < 22000:
                raise unittest.SkipTest(
                    "PSEUDOCONSOLE_PASSTHROUGH_MODE requires Windows 11+ "
                    f"(current build {version.build}). The ConPty "
                    "implementation is correct but the OS won't honor "
                    "the flag — see #150 conpty_passthrough/mod.rs doc."
                )

        from running_process import RunningProcess

        testbin = _resolve_testbin()
        process = RunningProcess.pseudo_terminal(
            [str(testbin)],
            rows=24,
            cols=80,
        )
        try:
            # Wait for the child to finish (~500ms of ticks + headroom).
            exit_code = process.wait(timeout=5)
        finally:
            process.close()

        self.assertEqual(
            exit_code,
            0,
            f"testbin-tui-counter exited non-zero: {exit_code}",
        )

        # `.output` returns the full captured stream. PTY-mode captures
        # are always bytes (text mode is silently ignored — see
        # tests/pty/test_pty_capture.py). The `cast` keeps pyright
        # happy; the runtime check below confirms the actual type.
        captured = cast(bytes, process.output)
        self.assertIsInstance(captured, (bytes, bytearray))
        self.assertGreater(
            len(captured),
            0,
            "expected non-empty captured output from PTY",
        )

        # Byte-exact ANSI assertions — the whole point of #150's
        # PSEUDOCONSOLE_PASSTHROUGH_MODE rewrite. Pre-#150 these
        # bytes would be eaten by ConPTY's virtual-screen rendering
        # on Windows.
        self.assertIn(
            b"\x1b[2J",
            captured,
            "clear-screen escape `\\x1b[2J` missing from PTY output; "
            "ConPTY may not be in PASSTHROUGH_MODE",
        )
        # The testbin emits `\x1b[1;1H`, but ConPTY on current Windows
        # runner images canonicalizes CUP(1,1) to the equivalent short
        # form `\x1b[H` even in passthrough mode. Either form proves the
        # cursor-home escape survived the PTY instead of being eaten.
        #
        # Anchor on the `COUNTER` payload: the testbin's clear preamble
        # is `\x1b[H\x1b[2J`, so a bare "contains `\x1b[H`" check would
        # pass on the preamble alone and stop proving that the per-tick
        # repaint escapes survived. See #660.
        self.assertTrue(
            b"\x1b[1;1HCOUNTER" in captured or b"\x1b[HCOUNTER" in captured,
            "tick repaint missing from PTY output: expected "
            "`\\x1b[1;1HCOUNTER` or `\\x1b[HCOUNTER`, got "
            f"{captured!r}",
        )

        # Plaintext bookend assertions: first and last counter values
        # must both appear, in order.
        text = captured.decode("utf-8", errors="replace")
        self.assertIn(
            "COUNTER: 0",
            text,
            f"first counter line missing: {text!r}",
        )
        self.assertIn(
            "COUNTER: 9",
            text,
            f"last counter line missing: {text!r}",
        )
        first_idx = text.index("COUNTER: 0")
        last_idx = text.rindex("COUNTER: 9")
        self.assertLess(
            first_idx,
            last_idx,
            f"counter lines not in order: first={first_idx}, last={last_idx}",
        )


if __name__ == "__main__":
    unittest.main()
