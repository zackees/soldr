#!/usr/bin/env python3
"""Run Soldr's complete smoke pipeline in the warmed Linux dev runner.

The first invocation builds from source through the published Soldr 0.8.29
driver baked into ``soldr-cook-dev``. Later invocations reuse persistent
target, tool-home, Soldr, uv-cache, and virtual-environment volumes.

Usage::

    uv run --no-project python ci/smoke_local.py
    uv run --no-project python ci/smoke_local.py --tokio-console
    uv run --no-project python ci/smoke_local.py --status
    uv run --no-project python ci/smoke_local.py --wipe
"""

from __future__ import annotations

import sys

import perf_local


def main(argv: list[str]) -> int:
    if not argv:
        return perf_local.main(["smoke"])
    if argv == ["--tokio-console"]:
        return perf_local.main(["smoke-console"])
    if argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0
    controls = ("--status", "--stop", "--reset-runner", "--wipe")
    if argv[0] in controls and len(argv) == 1:
        return perf_local.main(argv)
    print(f"error: unsupported smoke-runner arguments: {' '.join(argv)}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
