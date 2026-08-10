"""Top-level ``running-processor`` CLI.

Usage::

    running-processor dashboard [--port PORT] [--no-browser]
    running-processor --help
"""

from __future__ import annotations

import argparse
import sys
from collections.abc import Sequence


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="running-processor",
        description="running-process tools and dashboards",
    )
    sub = parser.add_subparsers(dest="command")

    # ── dashboard ──────────────────────────────────────────────────────
    dash = sub.add_parser("dashboard", help="Launch the web dashboard")
    dash.add_argument(
        "--port", type=int, default=8787,
        help="HTTP server port (default: 8787)",
    )
    dash.add_argument(
        "--no-browser", action="store_true",
        help="Don't auto-open the browser",
    )

    args = parser.parse_args(argv)

    if args.command is None:
        parser.print_help()
        return 0

    if args.command == "dashboard":
        from running_process.dashboard import main as dashboard_main

        # Forward the parsed args as a list so dashboard's own parser can
        # re-parse them.  Simpler: just call its internals directly.
        dash_argv: list[str] = []
        if args.port != 8787:
            dash_argv += ["--port", str(args.port)]
        if args.no_browser:
            dash_argv.append("--no-browser")
        return dashboard_main(dash_argv)

    parser.print_help()
    return 0


if __name__ == "__main__":
    sys.exit(main())
