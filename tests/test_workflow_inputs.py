"""Reusable workflow inputs must be honored, not decorative.

`_build-and-test.yml` once declared `shared_key` as `required: true` while
the body keyed its cache from `inputs.target` instead, so every caller
maintained a value that was read by nothing.

The guard below is generic: any `workflow_call` input that no job body
references fails, which catches the next one rather than only re-checking
the originally reported input.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"


def test_obsolete_vcpkg_refresh_surface_is_absent() -> None:
    """soldr#2814: no weekly producer or unconsumed bundle contract remains."""
    for relative_path in (
        ".github/workflows/vcpkg-windows-refresh.yml",
        ".github/vcpkg-pin.txt",
        ".github/scripts/vcpkg_triplet_matrix.py",
    ):
        assert not (REPO_ROOT / relative_path).exists(), relative_path

    for workflow_path in WORKFLOWS.glob("*.yml"):
        assert "vcpkg" not in workflow_path.read_text(encoding="utf-8").lower()


def _declared_workflow_call_inputs(text: str) -> list[str]:
    """Input names under `on.workflow_call.inputs`.

    Deliberately a small scanner rather than a YAML dependency: these
    files are machine-generated-shaped and consistently indented, and
    the repo's other workflow tests are plain text scans too.
    """
    if "workflow_call:" not in text:
        return []
    lines = text.split("\n")
    start = next(i for i, line in enumerate(lines) if line.strip() == "workflow_call:")
    inputs_idx = None
    for i in range(start + 1, len(lines)):
        if lines[i].strip() == "inputs:":
            inputs_idx = i
            break
        # Left the workflow_call block entirely.
        if lines[i] and not lines[i].startswith(" "):
            break
    if inputs_idx is None:
        return []

    names: list[str] = []
    # Input names sit exactly one indent level below `inputs:`.
    indent = len(lines[inputs_idx]) - len(lines[inputs_idx].lstrip()) + 2
    pattern = re.compile(r"^ {%d}([A-Za-z_][A-Za-z0-9_]*):\s*$" % indent)
    start_after = inputs_idx + 1
    for line in lines[start_after:]:
        if line.strip() and (len(line) - len(line.lstrip())) < indent:
            break
        match = pattern.match(line)
        if match:
            names.append(match.group(1))
    return names


@pytest.mark.parametrize(
    "workflow",
    sorted(p.name for p in WORKFLOWS.glob("_*.yml")),
)
def test_every_declared_input_is_referenced(workflow: str) -> None:
    text = (WORKFLOWS / workflow).read_text(encoding="utf-8")
    declared = _declared_workflow_call_inputs(text)
    if not declared:
        pytest.skip(f"{workflow} declares no workflow_call inputs")

    unused = [name for name in declared if f"inputs.{name}" not in text]
    assert not unused, (
        f"{workflow} declares workflow_call input(s) {unused} that no job body "
        f"reads. A `required: true` input that is read by nothing forces every "
        f"caller to maintain a value for nothing. Either use it, or remove it "
        "and drop the argument from its callers."
    )


def test_build_and_test_callers_no_longer_pass_shared_key() -> None:
    # Passing an argument a reusable workflow does not declare is a hard
    # error from Actions, so a stale caller would break CI outright.
    for path in WORKFLOWS.glob("*.yml"):
        text = path.read_text(encoding="utf-8")
        if "_build-and-test.yml" not in text:
            continue
        assert "shared_key" not in text, (
            f"{path.name} still passes `shared_key` to _build-and-test.yml, "
            "which no longer declares it (soldr#1664)"
        )
