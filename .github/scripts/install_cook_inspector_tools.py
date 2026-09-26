#!/usr/bin/env python3
"""Best-effort installer for the native tools `cook_inspect.py` wants
(soldr#3043 cook-timeout inspector follow-up): `perf` (a `linux-tools-*`
package keyed to the running kernel), `gdb`, `elfutils` (ships `eu-stack`,
the gdb fallback), and `bpfcc-tools` (ships `offcputime-bpfcc`, the preferred
off-CPU sampler when present).

Every one of these is optional: `cook_inspect.py` checks `shutil.which` for
each tool and degrades that one capture phase when it is missing (see its
module docstring). This script exists only to make the common case (a
GitHub-hosted Ubuntu runner) have every tool available, and it NEVER fails
the job -- `main()` always returns 0 regardless of which packages actually
installed, so a workflow step running this needs no `continue-on-error` of
its own (the caller still sets one anyway, belt-and-suspenders).

Linux only. Callers must guard the step with `if: runner.os == 'Linux'`;
this script does not check `sys.platform` itself because `apt-get` simply
is not runnable on any other OS and `run()` already degrades that to "not
runnable" without raising.

Every `apt-get`/`sudo` invocation inherits this process's stdout/stderr
(no `capture_output`, no `DEVNULL`) so apt's own output lands straight in
the job log -- nothing here is allowed to swallow a child's streams.
"""

from __future__ import annotations

import platform
import subprocess
import sys

# `elfutils` ships `eu-stack`, the gdb fallback backtrace tool.
PACKAGES_ALWAYS = ["gdb", "elfutils"]
# `bpfcc-tools` ships `offcputime-bpfcc`, cook_inspect.py's preferred
# off-CPU sampler when present (it reports actual durations, not just
# sched_switch event counts).
BPFCC_PACKAGE = "bpfcc-tools"
# perf's package name is versioned to the running kernel release, and
# GitHub-hosted runner kernels are routinely ahead of what the distro's repo
# has a matching `linux-tools-<uname release>` package for. Every candidate
# is tried in order and the loop stops at the first one that installs.
PERF_CANDIDATES = [
    f"linux-tools-{platform.uname().release}",
    "linux-tools-generic",
    "linux-tools-azure",
]


def run(argv: list[str]) -> bool:
    """Run `argv`, inheriting stdout/stderr. Returns whether it succeeded.
    Never raises: a missing `sudo`/`apt-get` binary on a non-Ubuntu host is
    reported the same way as any other failure.
    """
    print(f"+ {' '.join(argv)}", file=sys.stderr)
    try:
        result = subprocess.run(argv, check=False)
    except OSError as error:
        print(
            f"install_cook_inspector_tools: {argv[0]} not runnable: {error}",
            file=sys.stderr,
        )
        return False
    return result.returncode == 0


def install_best_effort() -> None:
    if not run(["sudo", "-n", "apt-get", "update", "-qq"]):
        print(
            "install_cook_inspector_tools: apt-get update failed; continuing "
            "best-effort with whatever apt cache is already present",
            file=sys.stderr,
        )

    for package in [*PACKAGES_ALWAYS, BPFCC_PACKAGE]:
        if not run(
            [
                "sudo",
                "-n",
                "apt-get",
                "install",
                "-y",
                "--no-install-recommends",
                package,
            ]
        ):
            print(
                f"install_cook_inspector_tools: {package} unavailable; the "
                "matching cook_inspect.py capture phase will degrade",
                file=sys.stderr,
            )

    for candidate in PERF_CANDIDATES:
        if run(
            [
                "sudo",
                "-n",
                "apt-get",
                "install",
                "-y",
                "--no-install-recommends",
                candidate,
            ]
        ):
            break
    else:
        print(
            "install_cook_inspector_tools: no linux-tools (perf) package "
            "installed for this kernel; on-CPU/off-CPU sampling will degrade",
            file=sys.stderr,
        )


def main() -> int:
    install_best_effort()
    # Always 0: this step must never fail the job over a missing diagnostic
    # tool. See the module docstring.
    return 0


if __name__ == "__main__":
    sys.exit(main())
