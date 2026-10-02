"""A named success cannot authorize a candidate-controlled CI policy."""

import copy
import runpy
from pathlib import Path

import pytest

SCRIPT = Path(__file__).parents[1] / ".github/scripts/ci_policy_provenance.py"


def evidence(sha, blob="c" * 40):
    tree_sha = ("d" if sha.startswith("a") else "e") * 40
    return {
        "commit": {"sha": sha, "commit": {"tree": {"sha": tree_sha}}},
        "tree": {
            "sha": tree_sha,
            "truncated": False,
            "tree": [
                {
                    "path": ".github/workflows/ci.yml",
                    "type": "blob",
                    "mode": "100644",
                    "sha": blob,
                },
                {
                    "path": "src/main.rs",
                    "type": "blob",
                    "mode": "100644",
                    "sha": "f" * 40,
                },
            ],
        },
    }


def check(base, candidate):
    return runpy.run_path(str(SCRIPT))["verify_policy_trees"](
        base,
        candidate,
        reviewed_sha="a" * 40,
        candidate_sha="b" * 40,
        protected_prefixes=[".github/", "ci/"],
        protected_files=["lint", "test"],
    )


def test_source_change_keeps_reviewed_policy():
    base, candidate = evidence("a" * 40), evidence("b" * 40)
    candidate["tree"]["tree"][1]["sha"] = "1" * 40
    assert check(base, candidate)["success"] is True


@pytest.mark.parametrize(
    "mutation", ["modify", "add", "delete", "symlink", "submodule"]
)
def test_policy_cannot_be_replaced_or_expanded(mutation):
    base, candidate = evidence("a" * 40), evidence("b" * 40)
    entries = candidate["tree"]["tree"]
    if mutation == "modify":
        entries[0]["sha"] = "1" * 40
    elif mutation == "add":
        entries.append(
            {
                "path": ".github/workflows/fake.yml",
                "type": "blob",
                "mode": "100644",
                "sha": "1" * 40,
            }
        )
    elif mutation == "delete":
        entries.pop(0)
    elif mutation == "symlink":
        entries[0]["mode"] = "120000"
    else:
        entries[0].update(type="commit", mode="160000")
    assert check(base, candidate)["success"] is False


@pytest.mark.parametrize(
    "mutation",
    [
        "truncated",
        "missing-truncated",
        "wrong-commit",
        "wrong-tree",
        "duplicate",
        "invalid-path",
        "invalid-sha",
    ],
)
def test_incomplete_or_unbound_api_evidence_refuses(mutation):
    base, candidate = evidence("a" * 40), evidence("b" * 40)
    if mutation == "truncated":
        candidate["tree"]["truncated"] = True
    elif mutation == "missing-truncated":
        candidate["tree"].pop("truncated")
    elif mutation == "wrong-commit":
        candidate["commit"]["sha"] = "1" * 40
    elif mutation == "wrong-tree":
        candidate["tree"]["sha"] = "1" * 40
    elif mutation == "duplicate":
        candidate["tree"]["tree"].append(copy.deepcopy(candidate["tree"]["tree"][0]))
    elif mutation == "invalid-path":
        candidate["tree"]["tree"][0]["path"] = ".github/../source"
    else:
        candidate["tree"]["tree"][0]["sha"] = "not-a-blob"
    assert check(base, candidate)["success"] is False


def test_policy_bootstrap_does_not_self_authorize():
    assert check(evidence("a" * 40), evidence("b" * 40, "1" * 40))["success"] is False


def test_prefix_root_cannot_be_replaced_by_symlink_with_other_policy_present():
    base, candidate = evidence("a" * 40), evidence("b" * 40)
    other = {
        "path": "ci/selector.py",
        "type": "blob",
        "mode": "100644",
        "sha": "1" * 40,
    }
    base["tree"]["tree"].append(copy.deepcopy(other))
    candidate["tree"]["tree"] = [
        other,
        {"path": ".github", "type": "blob", "mode": "120000", "sha": "2" * 40},
    ]
    report = check(base, candidate)
    assert report["success"] is False
    assert any(".github: policy symlink" in failure for failure in report["failures"])


@pytest.mark.parametrize("prefixes", [[], [".github"], ["../policy/"], [".github//"]])
def test_candidate_cannot_supply_invalid_protected_inventory(prefixes):
    report = runpy.run_path(str(SCRIPT))["verify_policy_trees"](
        evidence("a" * 40),
        evidence("b" * 40),
        reviewed_sha="a" * 40,
        candidate_sha="b" * 40,
        protected_prefixes=prefixes,
        protected_files=[],
    )
    assert report["success"] is False


def test_nonrecursive_tree_cannot_hide_changed_policy_subtree():
    base, candidate = evidence("a" * 40), evidence("b" * 40)
    for obj, tree_blob in ((base, "1" * 40), (candidate, "2" * 40)):
        obj["tree"]["tree"] = [
            {"path": ".github", "type": "tree", "mode": "040000", "sha": tree_blob},
            {"path": "lint", "type": "blob", "mode": "100755", "sha": "3" * 40},
        ]
    assert check(base, candidate)["success"] is False


def test_nonrecursive_tree_cannot_hide_nested_protected_ancestor():
    base, candidate = evidence("a" * 40), evidence("b" * 40)
    for obj, tree_blob in ((base, "1" * 40), (candidate, "2" * 40)):
        obj["tree"]["tree"] = [
            {"path": "scripts", "type": "tree", "mode": "040000", "sha": tree_blob},
            {"path": "lint", "type": "blob", "mode": "100755", "sha": "3" * 40},
        ]
    report = runpy.run_path(str(SCRIPT))["verify_policy_trees"](
        base,
        candidate,
        reviewed_sha="a" * 40,
        candidate_sha="b" * 40,
        protected_prefixes=["scripts/ci/"],
        protected_files=["lint"],
    )
    assert report["success"] is False
