"""The cache-ownership guard (soldr#2937, phase 5 of soldr#2931).

The policy: cache admission follows the stability of an artifact's identity key
relative to its size. Dependency compilation (`cook`) and content-addressed
per-unit compiler outputs (`zccache-unit`) are durable; **linked test products
are never cacheable**.

Two things need covering and they pull in opposite directions:

* the guard must fail on a seeded violation, and
* it must *not* fail on the content-addressed per-unit stores that normal
  lanes depend on. soldr#2931 bans persisting linked products and bulk
  `target/` snapshots -- not zccache dogfooding. A guard that flagged the
  zccache layer would take the soldr#1698 Windows cache-roundtrip probes down
  with it, so `test_zccache_unit_store_is_not_flagged` is as load-bearing as
  the failure tests.

The real tree is checked too: a guard that passes because it stopped scanning
is worse than no guard, so `test_clean_tree_passes` runs against the actual
workflows and `test_manifest_covers_every_persisting_workflow` proves the
manifest still describes them.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_cache_ownership.py"
guard = load_script_module(_SCRIPT, "check_cache_ownership")

MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"

CACHE_ACTION = "actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809"
RUST_CACHE = "Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32"
UPLOAD_ACTION = "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"
COOK_ACTION = "zackees/setup-soldr/cook@5f1f68dcb8377818413c28ce52214261ae8ff771"


def minimal_manifest(entries: list[dict]) -> dict:
    """A schema-valid manifest wrapping `entries`.

    `experiment_workflows` is taken from the guard's own frozen constant
    because R1 requires the two to agree -- a fixture that hard-coded the list
    would start failing for a reason unrelated to what each test is about.
    """
    return {
        "schema_version": 1,
        "tiers": {"cook": "d", "zccache-unit": "d", "none": "d"},
        "exceptions": {
            "release-deliverable": "d",
            "pinned-immutable-download": "d",
            "dylint-foundation": "d",
            "cache-experiment": "d",
            "same-run-transport": "d",
            "bootstrap-driver-binary": "d",
        },
        "experiment_workflows": sorted(guard.EXPERIMENT_WORKFLOWS),
        "entries": entries,
    }


def entry(workflow: str, tier: str, **overrides: str) -> dict:
    base = {
        "id": overrides.pop("id", f"{workflow}-{tier}"),
        "workflow": workflow,
        "step": "*",
        "mechanism": "*",
        "tier": tier,
        "rationale": "fixture",
    }
    base.update(overrides)
    return base


def seed(tmp_path: Path, workflows: dict[str, str], entries: list[dict]) -> list[str]:
    """Write a throwaway tree and return the guard's findings."""
    workflow_dir = tmp_path / "workflows"
    workflow_dir.mkdir(parents=True, exist_ok=True)
    for name, body in workflows.items():
        (workflow_dir / name).write_text(body, encoding="utf-8")
    manifest_path = tmp_path / "cache-ownership.json"
    manifest_path.write_text(json.dumps(minimal_manifest(entries)), encoding="utf-8")
    return guard.check(manifest_path, workflow_dir)


def cache_test_archive(step_name: str = "Restore test archive") -> str:
    """A workflow that caches a linked nextest archive across runs."""
    return f"""
name: seeded
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - name: {step_name}
        uses: {CACHE_ACTION}
        with:
          path: dist/soldr-tests.tar.zst
          key: seeded-v1
"""


# --------------------------------------------------------------------------
# The real tree
# --------------------------------------------------------------------------


def test_clean_tree_passes() -> None:
    assert guard.check(MANIFEST, WORKFLOW_DIR) == []


def test_manifest_parses_and_is_schema_valid() -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert manifest["schema_version"] == guard.SCHEMA_VERSION
    assert guard.manifest_problems(manifest) == []

    ids = [e["id"] for e in manifest["entries"]]
    assert len(ids) == len(set(ids)), "manifest entry ids must be unique"

    vocabulary = set(manifest["tiers"]) | set(manifest["exceptions"])
    assert {"cook", "zccache-unit", "none"} <= vocabulary
    for e in manifest["entries"]:
        assert e["tier"] in vocabulary


def test_manifest_covers_every_persisting_workflow() -> None:
    steps = guard.collect_steps(WORKFLOW_DIR)
    assert steps, "the guard found nothing to classify -- it stopped scanning"

    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    covered = {e["workflow"] for e in manifest["entries"]}
    persisting = {step.workflow for step in steps}
    assert persisting - covered == set(), "unclassified persisting workflows"
    assert covered - persisting == set(), "manifest entries for dead workflows"


def test_experiment_exemptions_match_the_manifest() -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    assert set(manifest["experiment_workflows"]) == set(guard.EXPERIMENT_WORKFLOWS)


# --------------------------------------------------------------------------
# R3 -- linked test products in a cross-run store
# --------------------------------------------------------------------------


def test_seeded_test_archive_cache_fails(tmp_path: Path) -> None:
    problems = seed(
        tmp_path,
        {"seeded.yml": cache_test_archive()},
        [entry("seeded.yml", "none")],
    )
    assert any(p.startswith("R3") for p in problems), problems
    joined = "\n".join(problems)
    assert "seeded.yml" in joined
    assert "Restore test archive" in joined


def test_exempt_experiment_workflow_with_the_same_shape_passes(
    tmp_path: Path,
) -> None:
    name = "perf-matrix.yml"
    assert name in guard.EXPERIMENT_WORKFLOWS
    problems = seed(
        tmp_path,
        {name: cache_test_archive()},
        [entry(name, "cache-experiment")],
    )
    assert problems == [], problems


@pytest.mark.parametrize(
    "path",
    [
        "target/x86_64-unknown-linux-gnu/ci-nextest/deps",
        "target/debug/examples",
        "target/debug/incremental",
        "dist/soldr-tests.tar.zst",
        "artifacts/soldr-test-binaries",
        "target/debug/deps/soldr_cli.pdb",
    ],
)
def test_every_test_product_shape_is_caught(tmp_path: Path, path: str) -> None:
    body = f"""
name: seeded
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - name: Restore
        uses: {CACHE_ACTION}
        with:
          path: {path}
          key: seeded-v1
"""
    problems = seed(tmp_path, {"seeded.yml": body}, [entry("seeded.yml", "none")])
    assert any(p.startswith("R3") for p in problems), (path, problems)


def test_zccache_unit_store_is_not_flagged(tmp_path: Path) -> None:
    """Content-addressed per-unit caching stays live (the soldr#2931 nuance).

    The ban is on persisting linked products and bulk `target/` snapshots. A
    zccache store keyed by the hash of each unit's compiler inputs is exactly
    what the policy keeps, and normal lanes -- including the soldr#1698
    Windows cache-roundtrip probes -- depend on it being active.
    """
    body = f"""
name: seeded
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - name: Restore zccache
        uses: {CACHE_ACTION}
        with:
          path: ${{{{ runner.temp }}}}/soldr-build-under-test/cache/zccache
          key: seeded-zccache-v1
"""
    problems = seed(
        tmp_path, {"seeded.yml": body}, [entry("seeded.yml", "zccache-unit")]
    )
    assert problems == [], problems


def test_same_run_transport_of_a_test_archive_is_allowed(tmp_path: Path) -> None:
    """Artifact upload is run-keyed, so it is transport and not a cache."""
    body = f"""
name: seeded
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - name: Upload artifact
        uses: {UPLOAD_ACTION}
        with:
          name: seeded
          path: dist/soldr-tests.tar.zst
"""
    problems = seed(
        tmp_path,
        {"seeded.yml": body},
        [
            entry(
                "seeded.yml",
                "same-run-transport",
                mechanism="actions/upload-artifact",
            )
        ],
    )
    assert problems == [], problems


# --------------------------------------------------------------------------
# R4 -- broad target/ restore downstream of cook
# --------------------------------------------------------------------------


def _cook_workflow(cook_first: bool) -> str:
    cook = f"""      - name: Restore cooked dependency cache
        uses: {COOK_ACTION}
        with:
          target-dir: target
          flags: --profile ci-nextest --workspace
"""
    broad = f"""      - name: Restore cargo + target caches
        uses: {RUST_CACHE}
        with:
          shared-key: seeded
"""
    ordered = cook + broad if cook_first else broad + cook
    return f"""
name: seeded
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
{ordered}"""


def test_broad_target_restore_after_cook_fails(tmp_path: Path) -> None:
    problems = seed(
        tmp_path,
        {"seeded.yml": _cook_workflow(cook_first=True)},
        [entry("seeded.yml", "cook")],
    )
    assert any(p.startswith("R4") for p in problems), problems


def test_broad_target_restore_before_cook_passes(tmp_path: Path) -> None:
    problems = seed(
        tmp_path,
        {"seeded.yml": _cook_workflow(cook_first=False)},
        [entry("seeded.yml", "cook")],
    )
    assert problems == [], problems


def test_broad_target_restore_after_cook_is_exempt_in_an_experiment(
    tmp_path: Path,
) -> None:
    name = "cook-size-gate.yml"
    assert name in guard.EXPERIMENT_WORKFLOWS
    problems = seed(
        tmp_path,
        {name: _cook_workflow(cook_first=True)},
        [entry(name, "cache-experiment")],
    )
    assert problems == [], problems


# --------------------------------------------------------------------------
# R1 / R2 / R5 -- the manifest contract
# --------------------------------------------------------------------------


def test_uncovered_workflow_fails(tmp_path: Path) -> None:
    problems = seed(tmp_path, {"seeded.yml": cache_test_archive()}, [])
    assert any(p.startswith("R2") and "seeded.yml" in p for p in problems), problems


def test_entry_for_a_workflow_that_persists_nothing_fails(tmp_path: Path) -> None:
    body = "name: seeded\non: push\njobs:\n  build:\n    runs-on: ubuntu-24.04\n"
    problems = seed(tmp_path, {"seeded.yml": body}, [entry("seeded.yml", "none")])
    assert any(p.startswith("R2") for p in problems), problems


def test_widening_the_exemption_list_in_the_manifest_alone_fails() -> None:
    manifest = minimal_manifest([])
    manifest["experiment_workflows"] = sorted(
        set(guard.EXPERIMENT_WORKFLOWS) | {"ci.yml"}
    )
    problems = guard.manifest_problems(manifest)
    assert any(p.startswith("R1") for p in problems), problems


def test_unknown_tier_fails() -> None:
    manifest = minimal_manifest([entry("ci.yml", "made-up-tier")])
    problems = guard.manifest_problems(manifest)
    assert any("unknown tier" in p for p in problems), problems


def test_a_cache_may_not_claim_same_run_transport() -> None:
    manifest = minimal_manifest(
        [entry("ci.yml", "same-run-transport", mechanism="actions/cache")]
    )
    problems = guard.manifest_problems(manifest)
    assert any(p.startswith("R5") for p in problems), problems


def test_main_reports_zero_on_the_real_tree(capsys: pytest.CaptureFixture) -> None:
    code = guard.main(
        ["--manifest", str(MANIFEST), "--workflow-dir", str(WORKFLOW_DIR)]
    )
    assert code == 0, capsys.readouterr().out
