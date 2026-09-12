"""Unit tests for `cook_dylint_tests_tree.py` (soldr#3042, soldr#3159).

Covers the pure helpers -- `lint_roots`, `dependency_closure`,
`diverging_closures`, `cook_command`, `cook_env`, and `parse_outcome` --
because `main()` is a thin sequential subprocess loop over them and is
exercised end-to-end by the workflow, not by this suite.
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


def test_every_lint_crate_resolves_the_same_dependency_closure(cook):
    """The six lint crates must pin the same third-party dependency versions.

    They are six separate cargo workspaces with six separate lockfiles and
    nothing keeping them in step, so they drift apart silently as each is
    updated alone. They had: soldr#3159 found twelve shared deps at differing
    versions, `cc` at four of them (1.4.0 / 1.4.1 / 1.4.2 / 1.4.4).

    That costs real build time with no upside. All six cook into ONE shared
    `target/dylint/tests` tree, so every divergence makes that tree carry an
    extra copy of the same crate -- four `cc`s, two `thiserror`s -- compiled,
    linked and kept on disk because six lockfiles disagreed by accident.

    Calls the script's own `diverging_closures` rather than re-deriving the
    closure here: a test that re-implements what it tests validates a copy and
    cannot catch drift between the two (CLAUDE.md, agent code-smell rule).
    """
    roots = cook.lint_roots(REPO_ROOT)
    assert len(roots) == 6

    diverged = cook.diverging_closures(roots)

    assert not diverged, (
        f"{diverged} no longer pin the same dependency versions as "
        f"{roots[0].name}. Six lockfiles that disagree make the shared "
        "target/dylint/tests tree build duplicate copies of the same crates. "
        "Re-sync them: copy the newest dylints/*/Cargo.lock over the others "
        "and run `soldr cargo metadata` in each to re-stamp its own root "
        "package entry."
    )


def test_dependency_closure_excludes_the_crates_own_entry(tmp_path, cook):
    """The root package is the one entry guaranteed to differ between crates."""
    crate = tmp_path / "my_lint"
    crate.mkdir()
    (crate / "Cargo.lock").write_text(
        "version = 4\n\n"
        '[[package]]\nname = "my_lint"\nversion = "0.1.0"\n\n'
        '[[package]]\nname = "libc"\nversion = "0.2.1"\nchecksum = "abc"\n',
        encoding="utf-8",
    )

    assert cook.dependency_closure(crate) == frozenset({("libc", "0.2.1", "abc")})


def test_diverging_closures_names_only_the_crates_that_differ(tmp_path, cook):
    """A divergent crate is reported; matching ones are not."""

    def crate(name: str, libc_version: str) -> Path:
        root = tmp_path / name
        root.mkdir()
        (root / "Cargo.lock").write_text(
            "version = 4\n\n"
            f'[[package]]\nname = "{name}"\nversion = "0.1.0"\n\n'
            f'[[package]]\nname = "libc"\nversion = "{libc_version}"\n',
            encoding="utf-8",
        )
        return root

    reference = crate("a_lint", "0.2.1")
    same = crate("b_lint", "0.2.1")
    drifted = crate("c_lint", "0.2.2")

    assert cook.diverging_closures([reference, same, drifted]) == ["c_lint"]
    assert cook.diverging_closures([reference, same]) == []
    assert cook.diverging_closures([]) == []


def test_diverging_closures_ignores_a_differing_root_package_name(tmp_path, cook):
    """Crates with identical deps agree even though their own names differ."""

    def crate(name: str) -> Path:
        root = tmp_path / name
        root.mkdir()
        (root / "Cargo.lock").write_text(
            "version = 4\n\n"
            f'[[package]]\nname = "{name}"\nversion = "0.1.0"\n\n'
            '[[package]]\nname = "libc"\nversion = "0.2.1"\n',
            encoding="utf-8",
        )
        return root

    assert cook.diverging_closures([crate("a_lint"), crate("z_lint")]) == []
