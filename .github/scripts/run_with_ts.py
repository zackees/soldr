#!/usr/bin/env python3
"""Run a command, stream its merged stdout/stderr with elapsed-seconds prefix.

Cross-platform companion to ``ts_step.py`` (which is a pure
stdin → stdout transformer for use in shell pipelines like
``cmd 2>&1 | python ts_step.py``). This wrapper EXECUTES a command
and prefixes its output line-by-line.

The split exists because PowerShell's pipe / exit-code semantics make
the stdin-piped pattern awkward: ``$LASTEXITCODE`` after ``cmd | python``
is python's exit code, not ``cmd``'s. By contrast, this wrapper runs
``cmd`` as a subprocess and propagates its real exit code to the
caller, so PowerShell steps can drop ``run_with_ts.py`` in front of
``soldr`` (or any other command) without the exit-code dance.

Output format matches ``ts_step.py`` byte-for-byte:

    {seconds:7.2f} <original line bytes>

ANSI color sequences in the line body are passed through unchanged
(we read/write the raw stdout buffer), so colored cargo output stays
colored on the GHA UI. The PREFIX itself is plain — colorizing it
would distract from the tool output.

``time.monotonic()`` is used (not ``time.time()``) so the prefix is
unaffected by NTP slews or DST transitions mid-step.

Usage (from PowerShell or bash):

    python .github/scripts/run_with_ts.py soldr cargo build --release ...

Exit code: the wrapped command's exit code, or 127 if the command
couldn't be started.
"""

from __future__ import annotations

import subprocess
import sys
import time


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: run_with_ts.py <cmd> [args...]", file=sys.stderr)
        return 2

    cmd = sys.argv[1:]
    start = time.monotonic()
    out = sys.stdout.buffer

    try:
        # Long-lived by design: stdout is streamed line by line below,
        # and construction is inside try/except so a failure to start
        # reports as exit 127 instead of a traceback.
        # pylint: disable-next=consider-using-with
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            bufsize=0,
        )
    except OSError as err:
        print(f"run_with_ts.py: failed to start {cmd!r}: {err}", file=sys.stderr)
        return 127

    # mypy / Pyright reassurance — Popen with stdout=PIPE always sets .stdout.
    assert proc.stdout is not None
    for line in iter(proc.stdout.readline, b""):
        elapsed = time.monotonic() - start
        out.write(f"{elapsed:7.2f} ".encode("ascii"))
        out.write(line)
        out.flush()

    proc.wait()
    return proc.returncode if proc.returncode is not None else 0


if __name__ == "__main__":
    raise SystemExit(main())
