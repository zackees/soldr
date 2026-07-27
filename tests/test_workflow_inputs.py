"""Workflow inputs must be honored, not decorative (soldr#1664).

Two defects motivated these tests:

* `vcpkg-windows-refresh.yml` advertised a `triplets` input but the
  matrix hard-coded both values, so scoping an expensive Windows port
  build did nothing.
* `_build-and-test.yml` declared `shared_key` as `required: true` while
  the body keyed its cache from `inputs.target` instead, so every caller
  maintained a value that was read by nothing.

The guard below is generic: any `workflow_call` input that no job body
references fails, which catches the next one of these rather than only
re-checking the two that were reported.
"""

from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
SCRIPT = REPO_ROOT / ".github" / "scripts" / "vcpkg_triplet_matrix.py"


def _load_script():
    """Import the resolver by path.

    `.github/scripts/` is not a package and is not on `sys.path`, and
    inserting it would put a mid-file import after module-level code —
    which isort rejects. Loading by spec keeps every import at the top.
    """
    spec = importlib.util.spec_from_file_location("vcpkg_triplet_matrix", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


_resolver = _load_script()
SUPPORTED_TRIPLETS = _resolver.SUPPORTED_TRIPLETS
build_matrix = _resolver.build_matrix
parse_triplets = _resolver.parse_triplets

# ── the triplets input ───────────────────────────────────────────────


def test_empty_input_means_every_supported_triplet() -> None:
    # `schedule` runs supply no workflow_dispatch inputs, so the empty
    # case must reproduce the previous hard-coded matrix exactly.
    assert parse_triplets("") == list(SUPPORTED_TRIPLETS)
    assert parse_triplets("   ") == list(SUPPORTED_TRIPLETS)


def test_single_triplet_scopes_the_build() -> None:
    # The whole point of the issue: asking for one must not build both.
    assert parse_triplets("x64-windows-static-md") == ["x64-windows-static-md"]


def test_whitespace_and_duplicates_are_normalised() -> None:
    assert parse_triplets(" arm64-windows-static-md , x64-windows-static-md ") == [
        "arm64-windows-static-md",
        "x64-windows-static-md",
    ]
    # Order the operator wrote is preserved, duplicates collapse.
    assert parse_triplets("x64-windows-static-md,x64-windows-static-md") == [
        "x64-windows-static-md"
    ]


def test_stray_separators_are_tolerated() -> None:
    assert parse_triplets("x64-windows-static-md,,") == ["x64-windows-static-md"]


def test_unknown_triplet_is_rejected_naming_every_offender() -> None:
    # Ignoring a typo would look like a successful refresh that quietly
    # skipped a bundle, which is worse than failing.
    with pytest.raises(ValueError) as excinfo:
        parse_triplets("x64-windows-static-md,x86-windows,not-a-triplet")
    message = str(excinfo.value)
    assert "x86-windows" in message
    assert "not-a-triplet" in message
    # ...and it says what IS allowed.
    assert "x64-windows-static-md" in message


def test_matrix_shape_matches_what_the_workflow_consumes() -> None:
    # `strategy.matrix: ${{ fromJSON(...) }}` needs the key to be the
    # matrix dimension name used as `matrix.triplet`.
    assert build_matrix("x64-windows-static-md") == {
        "triplet": ["x64-windows-static-md"]
    }


def test_script_is_runnable_and_emits_github_output(tmp_path: Path) -> None:
    out = tmp_path / "github_output"
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--triplets",
            "arm64-windows-static-md",
            "--output",
            str(out),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    written = out.read_text(encoding="utf-8").strip()
    assert written.startswith("matrix=")
    assert json.loads(written.removeprefix("matrix=")) == {
        "triplet": ["arm64-windows-static-md"]
    }


def test_script_exits_nonzero_on_an_unknown_triplet() -> None:
    result = subprocess.run(
        [sys.executable, str(SCRIPT), "--triplets", "nonsense"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 1
    assert "unknown triplet" in result.stderr


def test_refresh_workflow_actually_consumes_the_triplets_input() -> None:
    text = (WORKFLOWS / "vcpkg-windows-refresh.yml").read_text(encoding="utf-8")
    assert "inputs.triplets" in text, (
        "the `triplets` input is declared but never read — that is exactly "
        "the soldr#1664 defect"
    )
    assert (
        "fromJSON(needs.resolve-matrix.outputs.matrix)" in text
    ), "the build matrix must come from the resolver, not a hard-coded list"


# ── no dead required inputs ──────────────────────────────────────────


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
        f"caller to maintain a value for nothing — soldr#1664. Either use it, "
        f"or remove it and drop the argument from its callers."
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
            f"which no longer declares it (soldr#1664)"
        )
