#!/usr/bin/env python3
"""Every workflow job that runs a repo Python script must pin its interpreter.

soldr#2763. The v0.9.3 release died on macOS ARM64 with::

    AttributeError: 'PosixPath' object has no attribute 'hardlink_to'

`Path.hardlink_to` is 3.10+. The job ran `.github/scripts/*.py` under whatever
`python3` the runner image happened to ship, and that image's was older. The
call site had been extracted from inline YAML one PR earlier, so this was its
first execution on that lane -- the interpreter floor had been a per-file
convention all along, and nothing was checking it.

An audit at the time found a second landmine already in place
(`validate_npm_release_recovery.py` importing `tomllib`, 3.11+, unguarded), which
is what makes this a class rather than an incident: fixing the one API that blew
up means re-dispatching the release and dying at the next one a cycle later.

## What is checked

For each job in `.github/workflows/*.yml`: if any step's `run:` invokes a script
under `.github/scripts/` or `ci/`, the job must pin the interpreter that runs
it. See `job_pins_interpreter` for what counts -- in particular, setting up uv
and then calling bare `python3` does *not*, which is a distinction two jobs in
this repo were already on the wrong side of.

## Why a ratchet, and not a threshold

Twenty-two jobs are unpinned today across eighteen workflow files. Failing all
of them at once would block every PR on a sweep through lanes that cannot be
validated locally, so those jobs are baselined in `BASELINE` below: they may
stay as they are, but no *new* unpinned job may appear. Same bargain
`loc_ratchet.py` strikes, and for the same reason -- "don't make it worse" beats
"fix it before you may proceed" when the fix is a wide, hard-to-test sweep.

The baseline fails in **both** directions, like `check_dependency_inventory.py`:
a job that gets pinned must be removed from the list. Otherwise the list rots
into a permanent record of a problem that was quietly solved, and the next
reader cannot tell which entries are real.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

import yaml

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"

# A `run:` mentioning a path under these directories is running repo Python.
SCRIPT_PATTERN = re.compile(r"(?:\.github/scripts|ci)/[\w./-]+\.py")

# `actions/setup-python` prepends its chosen interpreter to PATH, so a bare
# `python3` in a later step resolves to it. `astral-sh/setup-uv` does not: it
# installs uv and leaves `python3` meaning whatever the image ships. So uv pins
# only the invocations actually routed through `uv run`.
SETUP_PYTHON_PATTERN = re.compile(r"actions/setup-python")
SETUP_UV_PATTERN = re.compile(r"astral-sh/setup-uv")
UV_RUN_PATTERN = re.compile(r"uv run[^\n]*?(?:\.github/scripts|ci)/[\w./-]+\.py")

# Jobs running repo Python under an unpinned interpreter as of soldr#2763.
# Entries are `(workflow file, job id)`. Shrink this list; never grow it.
BASELINE: frozenset[tuple[str, str]] = frozenset(
    {
        ("_bootstrap-e2e.yml", "bootstrap-e2e"),
        ("_build-and-test.yml", "build-and-test"),
        ("_ci-cross-build-linux.yml", "cross-build"),
        ("baseline-zero-deps.yml", "build-soldr"),
        ("baseline-zero-deps.yml", "docker-baseline"),
        ("benchmark-stats.yml", "gate"),
        # soldr#2763: `ci.yml: lint` left this list when its scripts moved to
        # `uv run --python 3.13`. `setup-soldr-action.yml: smoke` still sets up
        # uv and then calls bare `python3`, so the uv step buys it nothing.
        ("setup-soldr-action.yml", "smoke"),
        ("ci.yml", "windows-e2e-policy"),
        ("ci.yml", "wheel-cross-policy"),
        ("cross-compile-all-targets.yml", "bootstrap-and-linux-x86"),
        ("cross-compile-all-targets.yml", "cross-compile"),
        ("cross-compile-stress.yml", "resolve-matrix"),
        ("cross-compile-stress.yml", "cross-build"),
        ("cross-compile-stress.yml", "summarize"),
        ("docker-linux-cross-smoke.yml", "smoke"),
        ("dylint-cache-acceptance.yml", "dylint-cache"),
        ("dylint-cook-acceptance.yml", "dylint-cook"),
        ("nextest-cacheability.yml", "nextest-cacheability"),
        ("perf-matrix.yml", "gate"),
        ("thin-v2-verify.yml", "verify"),
        ("vcpkg-windows-refresh.yml", "resolve-matrix"),
        ("win-gnu-smoke.yml", "link-smoke"),
    }
)


def job_run_text(job: dict) -> str:
    """Every `run:` body in the job, joined."""
    return "\n".join(
        step.get("run") or ""
        for step in job.get("steps") or []
        if isinstance(step, dict)
    )


def job_uses_text(job: dict) -> str:
    """Every `uses:` reference in the job, joined."""
    return "\n".join(
        step.get("uses") or ""
        for step in job.get("steps") or []
        if isinstance(step, dict)
    )


def job_runs_repo_python(job: dict) -> bool:
    """Does any step in `job` invoke a script under `.github/scripts/` or `ci/`?"""
    return bool(SCRIPT_PATTERN.search(job_run_text(job)))


def job_pins_interpreter(job: dict) -> bool:
    """Does `job` fix which Python runs its repo scripts?

    Three ways, and the third is the one that is easy to get wrong:

    * `container:` -- the image is pinned in the workflow, so the interpreter
      cannot vary with the runner image.
    * `actions/setup-python` -- prepends its interpreter to PATH, so a later
      bare `python3` resolves to the pinned one.
    * `astral-sh/setup-uv` -- **only for invocations routed through `uv run`**.
      Installing uv does not change what `python3` means, so a job that sets up
      uv and then runs `python3 script.py` is exactly as exposed as one that set
      up nothing at all. Two jobs in this repo were in that state when the guard
      was written, and a check keyed on the setup step alone called them safe.
    """
    if job.get("container"):
        return True
    uses = job_uses_text(job)
    if SETUP_PYTHON_PATTERN.search(uses):
        return True
    if not SETUP_UV_PATTERN.search(uses):
        return False
    # Every repo-script invocation must go through `uv run`, not merely one.
    runs = job_run_text(job)
    if len(UV_RUN_PATTERN.findall(runs)) != len(SCRIPT_PATTERN.findall(runs)):
        return False
    # ...and uv must exist before the first step that uses it.
    #
    # soldr#2763: this check was missing, and the guard passed a Lint job whose
    # first `uv run` was step 1 while `setup-uv` was step 8 -- every one of
    # those steps would have died on `uv: command not found`. A guard that
    # reports a job pinned when it cannot run at all is worse than no guard.
    return uv_is_installed_before_use(job)


def uv_is_installed_before_use(job: dict) -> bool:
    """Does `astral-sh/setup-uv` run before the first step invoking `uv`?

    Ordering is invisible to a "does this job set up uv" check and fatal at
    runtime: `uv run` in a step before the setup fails with
    `uv: command not found`, which is how soldr#2800's first attempt broke.
    """
    setup_at: int | None = None
    first_use: int | None = None
    for index, step in enumerate(job.get("steps") or []):
        if not isinstance(step, dict):
            continue
        if setup_at is None and SETUP_UV_PATTERN.search(step.get("uses") or ""):
            setup_at = index
        if first_use is None and "uv run" in (step.get("run") or ""):
            first_use = index
    if first_use is None:
        return True
    return setup_at is not None and setup_at < first_use


def unpinned_jobs(workflow_dir: pathlib.Path) -> set[tuple[str, str]]:
    """Every `(file, job)` that runs repo Python without pinning an interpreter."""
    found: set[tuple[str, str]] = set()
    paths = sorted(workflow_dir.glob("*.yml")) + sorted(workflow_dir.glob("*.yaml"))
    for path in paths:
        document = yaml.safe_load(path.read_text(encoding="utf-8"))
        if not isinstance(document, dict):
            continue
        for job_id, job in (document.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            if job_runs_repo_python(job) and not job_pins_interpreter(job):
                found.add((path.name, str(job_id)))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workflow-dir",
        type=pathlib.Path,
        default=WORKFLOW_DIR,
        help="directory of workflow YAML to check (default: .github/workflows)",
    )
    args = parser.parse_args()

    found = unpinned_jobs(args.workflow_dir)
    added = sorted(found - BASELINE)
    fixed = sorted(BASELINE - found)

    if added:
        print("error: workflow job runs repo Python under an unpinned interpreter:")
        for workflow, job in added:
            print(f"  {workflow}: job '{job}'")
        print()
        print(
            "Pin it: add `actions/setup-python`, or add `astral-sh/setup-uv` and\n"
            "invoke the script as `uv run --python <version> <script>`. Setting up\n"
            "uv without routing the call through `uv run` does not count -- it\n"
            "leaves `python3` meaning whatever the runner image ships, which\n"
            "differs per platform and drifts without notice (soldr#2763)."
        )
    if fixed:
        if added:
            print()
        print("error: these jobs now pin an interpreter and must leave the baseline:")
        for workflow, job in fixed:
            print(f"  {workflow}: job '{job}'")
        print()
        print(
            "Remove them from BASELINE in this script. The list is a record of\n"
            "work still to do; entries that are done make it unreadable."
        )
    if added or fixed:
        return 1

    print(
        f"check_workflow_python_pin: no unpinned jobs beyond the "
        f"{len(BASELINE)} baselined."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
