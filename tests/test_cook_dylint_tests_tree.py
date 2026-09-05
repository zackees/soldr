"""Unit tests for `cook_dylint_tests_tree.py` (soldr#3042).

Covers the pure helpers only -- `lint_roots`, `cook_command`, `cook_env`, and
`parse_outcome` -- because `main()` is a thin sequential subprocess loop over
them and is exercised end-to-end by the workflow, not by this suite.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "cook_dylint_tests_tree.py"
)
REPO_ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture(scope="module")
def cook():
    return load_script_module(SCRIPT, "cook_dylint_tests_tree")


def test_lint_roots_finds_only_dirs_with_a_cargo_toml(tmp_path, cook):
    lints_dir = tmp_path / "dylints"
    lints_dir.mkdir()

    for name in ("zeta_lint", "alpha_lint"):
        crate = lints_dir / name
        crate.mkdir()
        (crate / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")

    decoy = lints_dir / "not_a_crate"
    decoy.mkdir()
    (decoy / "README.md").write_text("not a manifest\n", encoding="utf-8")

    roots = cook.lint_roots(tmp_path)

    assert roots == [lints_dir / "alpha_lint", lints_dir / "zeta_lint"]


def test_lint_roots_on_the_real_repo_returns_exactly_six_directories(cook):
    roots = cook.lint_roots(REPO_ROOT)

    assert len(roots) == 6
    assert roots == sorted(roots)
    for root in roots:
        assert (root / "Cargo.toml").is_file()


def test_cook_command_contains_the_required_flags(cook):
    soldr = Path("/repo/target/x86_64-unknown-linux-gnu/debug/soldr")
    target_root = Path("/repo/target")

    command = cook.cook_command(soldr, target_root)

    assert "--tree" in command
    assert command[command.index("--tree") + 1] == "tests"
    assert "--tests" in command
    assert "--json" in command
    assert "--target-root" in command
    assert command[command.index("--target-root") + 1] == str(target_root)


def test_cook_env_sets_and_removes_the_expected_variables(cook):
    soldr = Path("/repo/target/x86_64-unknown-linux-gnu/debug/soldr")
    base = {
        "PATH": "/usr/bin",
        "CARGO_BUILD_JOBS": "1",
        "SOLDR_JOBS": "1",
        "CARGO_TARGET_DIR": "/repo/target",
    }

    env = cook.cook_env(base, soldr)

    assert env["SOLDR_RUSTC_WRAPPER"] == str(soldr)
    assert env["SOLDR_LINKER"] == "default"
    assert env["SOLDR_NO_GC_TARGET"] == "1"
    assert "CARGO_BUILD_JOBS" not in env
    assert "SOLDR_JOBS" not in env
    assert "CARGO_TARGET_DIR" not in env
    assert env["PATH"] == "/usr/bin"


def test_parse_outcome_handles_a_trailing_blank_line(cook):
    stdout = '{"schema_version": 1, "outcome": "skip"}\n\n'

    assert cook.parse_outcome(stdout) == "skip"


def test_parse_outcome_handles_non_json_payload(cook):
    assert cook.parse_outcome("not json at all") == "unknown"
