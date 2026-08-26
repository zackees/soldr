"""A job that runs a repo Python script must pin the interpreter it runs under.

Workflow jobs must not invoke `.github/scripts/*.py` and `ci/*.py` with a bare
`python3`.
That resolves to whatever interpreter the runner image happens to ship, which
differs per platform and drifts as GitHub rolls images -- so a script can work
on every lane for months and then fail on one, with nothing in the repo having
changed.

That is exactly how v0.9.3 died. `release-auto.yml`'s `build` matrix ran
`stage_release_binaries.py` under the macOS ARM64 image's `python3`, which
predates `Path.hardlink_to` (3.10+):

    AttributeError: 'PosixPath' object has no attribute 'hardlink_to'

The script was fine; the interpreter was a decade-old assumption nobody had
written down. It had been latent since #2469 extracted the script out of inline
YAML, and surfaced on its first release execution on that lane -- after the
whole matrix had already built.

The failure mode this guards is *the second one*. Fixing `hardlink_to` alone
leaves ~15 more scripts in that job that have still never run under that
interpreter, so the next 3.10+/3.11+ API dies the same way, one release cycle
per discovery. Pinning the job is what makes the whole set safe at once.

This is a ratchet, not a threshold
----------------------------------
`RATCHET` lists the jobs that were already unpinned when this test landed. They
are not endorsed -- they are grandfathered so that fixing them can happen in
reviewable pieces instead of one 20-job PR that touches every lane in the repo.
The test enforces two directions:

  * a job not in `RATCHET` may not run a repo script unpinned -- no new debt;
  * a job in `RATCHET` that has since been pinned must be *removed* from it --
    so the list can only shrink, and a stale entry cannot quietly re-authorise
    a regression.

`release-auto.yml` is deliberately absent from `RATCHET`: every job in the
release lane invokes repository scripts through `uv run --no-project --python
3.13` as of soldr#2763, and none may regress.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest
import yaml

REPO = Path(__file__).resolve().parents[1]
WORKFLOWS = sorted((REPO / ".github" / "workflows").glob("*.y*ml"))

# `python3 .github/scripts/x.py`, `python ci/x.py`, and env-prefixed variants --
# but NOT `uv run --python 3.13 ... x.py`, which carries its own pin.
BARE_INVOCATION = re.compile(
    r"(?m)^\s*(?:[A-Z_][A-Z0-9_]*=\S*\s+)*(?:python3?|py)\s+"
    r"(?P<script>\S*(?:\.github/scripts|ci)/\S+\.py)"
)
SETUP_PYTHON = "actions/setup-python"
SETUP_UV = "astral-sh/setup-uv"
RELEASE_UV_COMMAND = "uv run --no-project --python 3.13"

# Jobs that were already unpinned when this guard landed (soldr#2763).
# Shrink this list; never grow it.
RATCHET = frozenset(
    {
        ("_bootstrap-e2e.yml", "bootstrap-e2e"),
        ("_build-and-test.yml", "build-and-test"),
        ("_ci-cross-build-linux.yml", "cross-build"),
        ("baseline-zero-deps.yml", "build-soldr"),
        ("baseline-zero-deps.yml", "docker-baseline"),
        ("benchmark-stats.yml", "gate"),
        ("cross-compile-all-targets.yml", "bootstrap-and-linux-x86"),
        ("cross-compile-all-targets.yml", "cross-compile"),
        ("docker-linux-cross-smoke.yml", "smoke"),
        ("perf-matrix.yml", "gate"),
    }
)


def _jobs():
    """Yield (workflow_name, job_name, runs_repo_script, pins_interpreter)."""
    for path in WORKFLOWS:
        doc = yaml.safe_load(path.read_text(encoding="utf-8"))
        if not isinstance(doc, dict):
            continue
        for job_name, job in (doc.get("jobs") or {}).items():
            if not isinstance(job, dict):
                continue
            steps = job.get("steps") or []
            scripts = [
                m.group("script")
                for step in steps
                for m in BARE_INVOCATION.finditer(str(step.get("run") or ""))
            ]
            pinned = any(SETUP_PYTHON in str(s.get("uses") or "") for s in steps)
            yield path.name, job_name, scripts, pinned


def test_no_unpinned_job_runs_a_repo_script():
    offenders = [
        (wf, job, scripts)
        for wf, job, scripts, pinned in _jobs()
        if scripts and not pinned and (wf, job) not in RATCHET
    ]
    assert (
        not offenders
    ), "jobs running repo scripts under an unpinned interpreter:\n" + "\n".join(
        f"  {wf} :: {job} -> {', '.join(sorted({Path(s).name for s in scripts}))}"
        for wf, job, scripts in offenders
    )


def test_ratchet_has_no_stale_entries():
    """A grandfathered job that got pinned must leave the list."""
    still_unpinned = {
        (wf, job) for wf, job, scripts, pinned in _jobs() if scripts and not pinned
    }
    known = {(wf, job) for wf, job, _, _ in _jobs()}
    stale = sorted(
        entry for entry in RATCHET if entry in known and entry not in still_unpinned
    )
    assert not stale, (
        "these jobs now pin their interpreter -- remove them from RATCHET:\n"
        + "\n".join(f"  {wf} :: {job}" for wf, job in stale)
    )


def test_ratchet_entries_all_exist():
    """A renamed or deleted job must not linger as a dead exemption."""
    known = {(wf, job) for wf, job, _, _ in _jobs()}
    missing = sorted(entry for entry in RATCHET if entry not in known)
    assert (
        not missing
    ), "RATCHET names jobs that no longer exist (renamed or deleted?):\n" + "\n".join(
        f"  {wf} :: {job}" for wf, job in missing
    )


def test_release_scripts_use_pinned_uv_without_bare_python():
    """Every release job that runs a repository script owns a uv setup step."""
    release = yaml.safe_load(
        (REPO / ".github" / "workflows" / "release-auto.yml").read_text(
            encoding="utf-8"
        )
    )
    for job_name, job in (release.get("jobs") or {}).items():
        steps = job.get("steps") or []
        script_runs = [
            str(step.get("run") or "")
            for step in steps
            if ".github/scripts/" in str(step.get("run") or "")
        ]
        if not script_runs:
            continue
        assert any(SETUP_UV in str(step.get("uses") or "") for step in steps), (
            f"release job {job_name!r} runs repository scripts without setup-uv"
        )
        assert not any(BARE_INVOCATION.search(run) for run in script_runs), (
            f"release job {job_name!r} reintroduced bare Python: {script_runs}"
        )
        assert all(RELEASE_UV_COMMAND in run for run in script_runs), (
            f"release job {job_name!r} must pin every repository script with "
            f"{RELEASE_UV_COMMAND!r}: {script_runs}"
        )


@pytest.mark.parametrize(
    "job_name",
    [
        "prepare",
        "build",
        "smoke_macos_arm64",
        "smoke_windows",
        "publish",
        "verify_github_release",
        "publish-pypi",
        "publish-npm",
        "release-completeness",
    ],
)
def test_every_release_job_pins_python(job_name):
    """The release lane has no exemptions -- this is the lane v0.9.3 died on."""
    rows = {
        job: (scripts, pinned)
        for wf, job, scripts, pinned in _jobs()
        if wf == "release-auto.yml"
    }
    assert job_name in rows, f"release-auto.yml has no job {job_name!r}"
    scripts, pinned = rows[job_name]
    if scripts:
        assert pinned, f"release job {job_name!r} runs {scripts} unpinned"
