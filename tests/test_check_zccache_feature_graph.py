"""Guard tests for `ci/check_zccache_feature_graph.py` (soldr#2901)."""

from __future__ import annotations

import subprocess
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).resolve().parents[1] / "ci" / "check_zccache_feature_graph.py"


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_zccache_feature_graph")


def completed(returncode: int, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(
        args=["soldr", "cargo", "tree"],
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
    )


def test_repository_manifests_match_the_settled_feature_set(guard) -> None:
    assert guard._check_manifest_features() == []


def test_soldr_cli_is_the_only_crate_requesting_a_feature(guard) -> None:
    assert guard.MANIFEST_FEATURES["crates/soldr-cli/Cargo.toml"] == ["formatter"]
    assert guard.MANIFEST_FEATURES["crates/soldr-cache/Cargo.toml"] == []
    assert guard.MANIFEST_FEATURES["crates/soldr-daemon/Cargo.toml"] == []


def test_cli_and_its_expansion_are_all_forbidden(guard) -> None:
    # Every feature `cli` turns on is independently a way back in, so each
    # must be named. `formatter` is deliberately absent: soldr-cli needs it.
    assert "cli" in guard.FORBIDDEN_FEATURES
    assert "formatter" not in guard.FORBIDDEN_FEATURES
    for expansion in ("daemon-entry", "download-client", "gha", "symbols"):
        assert expansion in guard.FORBIDDEN_FEATURES


def test_normal_tree_without_the_archive_stack_passes(guard) -> None:
    with patch.object(
        guard,
        "_run",
        return_value=completed(0, "soldr-cli v0.9.13\n`-- zccache v1.13.22\n"),
    ):
        assert not guard._check_no_normal_archive_stack("soldr")


def test_normal_tree_with_sevenz_fails(guard) -> None:
    with patch.object(
        guard,
        "_run",
        return_value=completed(0, "`-- sevenz-rust2 v0.19.0\n"),
    ):
        failures = guard._check_no_normal_archive_stack("soldr")

    assert len(failures) == 1
    assert "normal dependency path to sevenz-rust2" in failures[0]


def test_normal_tree_command_error_fails_closed(guard) -> None:
    with patch.object(
        guard,
        "_run",
        return_value=completed(101, stderr="metadata failed"),
    ):
        failures = guard._check_no_normal_archive_stack("soldr")

    assert len(failures) == 1
    assert "could not inspect normal dependency tree" in failures[0]
    assert "metadata failed" in failures[0]


def test_feature_tree_flags_a_restored_cli_feature(guard) -> None:
    tree = 'zccache v1.13.22\n|-- zccache feature "cli"\n|-- zccache feature "formatter"\n'
    with patch.object(guard, "_run", return_value=completed(0, tree)):
        failures = guard._check_tree(
            "soldr",
            "soldr-cli",
            required_features=("formatter",),
            forbidden_features=guard.FORBIDDEN_FEATURES,
        )

    assert len(failures) == 1
    assert "forbidden zccache feature 'cli'" in failures[0]


def test_feature_tree_flags_a_missing_required_feature(guard) -> None:
    tree = 'zccache v1.13.22\n|-- zccache feature "default"\n'
    with patch.object(guard, "_run", return_value=completed(0, tree)):
        failures = guard._check_tree(
            "soldr",
            "soldr-cli",
            required_features=("formatter",),
            forbidden_features=guard.FORBIDDEN_FEATURES,
        )

    assert failures == ["soldr-cli: must resolve zccache/formatter"]


def test_feature_tree_command_error_fails_closed(guard) -> None:
    with patch.object(guard, "_run", return_value=completed(101, stderr="boom")):
        failures = guard._check_tree(
            "soldr",
            "soldr-cache",
            required_features=(),
            forbidden_features=guard.FORBIDDEN_FEATURES,
        )

    assert len(failures) == 1
    assert "could not inspect zccache features" in failures[0]
