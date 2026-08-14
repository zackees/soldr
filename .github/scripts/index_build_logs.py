#!/usr/bin/env python3
"""Print an index of the build/diagnostic logs a CI lane produced.

Replaces the two `Print build XML logs` / `Print JSONL compile journals` steps
in `_build-and-test.yml`, which dumped every matching file's full contents into
the job console. A compile journal carries one JSON object per compile unit, so
a workspace build pushed megabytes of scrollback between the reader and the
actual failure -- the opposite of diagnosable, and the reason soldr#2493's
broker bringup stall was so hard to attribute.

What CI needs in the console is a *map*: which logs exist, how big they are,
and where to get them. The contents belong in the downloadable
`build-logs-<target>` artifact, which the sibling upload step publishes.

Discovery deliberately does not require a `logs/` path component. Three
families of evidence live outside one and were invisible to both the old print
steps and the old artifact globs:

  * archived compile journals under `cache/zccache/history/<id>/`
  * `broker-spawn.log` / `daemon-spawn.log` at the soldr state root
  * `broker/broker-bringup.jsonl`, the per-phase broker cold-start timing

Usage:
    python3 .github/scripts/index_build_logs.py --root ~/.soldr \\
        --root "$RUNNER_TEMP/soldr-self-tests" --artifact-name build-logs-x
"""

from __future__ import annotations

import argparse
import pathlib
import sys

# Suffixes worth indexing. `.log` covers the broker/daemon spawn logs, `.jsonl`
# the compile journal + bringup timings, `.xml` the per-build toolchain-home
# log soldr#1799 added, and `.json` the zccache session stats.
LOG_SUFFIXES = (".xml", ".jsonl", ".json", ".log")

# Directories that hold build state rather than diagnostics. Walking into these
# turns an index into a full `target/` listing.
SKIP_DIRECTORY_NAMES = frozenset({"bin", "sdk", "shims", "rustup", "cargo", "toolchains"})


def human_bytes(size: int) -> str:
    """Render a byte count compactly, so the index column stays readable."""
    value = float(size)
    for unit in ("B", "KB", "MB"):
        if value < 1024:
            return f"{value:.0f}{unit}" if unit == "B" else f"{value:.1f}{unit}"
        value /= 1024
    return f"{value:.1f}GB"


def discover(root: pathlib.Path) -> list[pathlib.Path]:
    """Return every diagnostic-looking file under `root`, sorted by path."""
    if not root.is_dir():
        return []
    found: list[pathlib.Path] = []
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            entries = list(current.iterdir())
        except OSError:
            # An unreadable directory is not worth failing an `always()` step.
            continue
        for entry in entries:
            try:
                # Never follow a directory symlink: soldr's state tree contains
                # relocated-image and trash directories, and a cycle would make
                # this walk loop forever. `continue-on-error` bounds the exit
                # code, not the runtime, so the lane would hang until timeout.
                if entry.is_dir() and not entry.is_symlink():
                    if entry.name not in SKIP_DIRECTORY_NAMES:
                        stack.append(entry)
                elif entry.is_file() and entry.suffix in LOG_SUFFIXES:
                    found.append(entry)
            except OSError:
                continue
    return sorted(found)


def render(roots: list[pathlib.Path], artifact_name: str | None) -> str:
    """Build the console index. Pure, so the formatting is unit-testable."""
    lines: list[str] = []
    total_files = 0
    total_bytes = 0
    for root in roots:
        files = discover(root)
        lines.append(f"===== {root} ({len(files)} log files) =====")
        if not files:
            lines.append("  (none)")
            continue
        for path in files:
            try:
                size = path.stat().st_size
            except OSError:
                size = 0
            total_bytes += size
            total_files += 1
            lines.append(f"  {human_bytes(size):>8}  {path}")
    lines.append(
        f"Indexed {total_files} log files ({human_bytes(total_bytes)} total). "
        "Contents are not printed here."
    )
    if artifact_name:
        lines.append(f"Download the full contents from the '{artifact_name}' artifact.")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        action="append",
        default=[],
        help="directory to index; repeatable. Missing roots are reported as empty.",
    )
    parser.add_argument(
        "--artifact-name",
        default=None,
        help="artifact the contents are uploaded to, named in the closing hint",
    )
    args = parser.parse_args(argv)
    roots = [pathlib.Path(root).expanduser() for root in args.root]
    print(render(roots, args.artifact_name))
    # Informational only: this must never fail a lane on its own.
    return 0


if __name__ == "__main__":
    sys.exit(main())
