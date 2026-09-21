"""Unit coverage for the PR-only jobs selected by canonical CI."""

import importlib.util
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/ci_path_policy.py"
SPEC = importlib.util.spec_from_file_location("ci_path_policy", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def test_docs_only_pr_skips_expensive_jobs() -> None:
    outputs = MODULE.select("pull_request", ["README.md", "docs/API.md"])
    assert outputs == {
        "docs_only": "true",
        "run_setup_soldr": "false",
        "run_cook_size_gate": "false",
    }


def test_retained_pr_signals_keep_their_former_path_selection() -> None:
    outputs = MODULE.select(
        "pull_request",
        [
            "crates/soldr-cache/src/cache_lib/strip_target.rs",
            "action.yml",
        ],
    )
    assert outputs["docs_only"] == "false"
    assert outputs["run_setup_soldr"] == "true"
    assert outputs["run_cook_size_gate"] == "true"
