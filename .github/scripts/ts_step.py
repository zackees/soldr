#!/usr/bin/env python3
"""Timestamp every stdin line with seconds-since-script-start.

Usage from a GHA `run:` block (the wrapper pattern lives in the
workflow YAML — this script is the inner stream-transformer):

    {
        ... your shell body ...
    } 2>&1 | python3 -u .github/scripts/ts_step.py

Format: each line is prefixed with `{seconds:7.2f} ` so output lines up
visually:

      0.01 Compiling soldr-cli v0.7.57 (...)
     12.43 Finished `release` profile [unoptimized + debuginfo]

ANSI color sequences in the line body are passed through byte-for-byte
(we read/write `sys.stdin.buffer` / `sys.stdout.buffer`), so colored
cargo output stays colored on the GHA UI. The PREFIX itself is plain;
colorizing it would distract from the tool output.

`time.monotonic()` is used (not `time.time()`) so the prefix is
unaffected by NTP slews or daylight-saving transitions that could
happen mid-step.
"""

from __future__ import annotations

import sys
import time


def main() -> int:
    start = time.monotonic()
    out = sys.stdout.buffer
    stream = sys.stdin.buffer
    for line in iter(stream.readline, b""):
        elapsed = time.monotonic() - start
        prefix = f"{elapsed:7.2f} ".encode("ascii")
        out.write(prefix)
        out.write(line)
        out.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
