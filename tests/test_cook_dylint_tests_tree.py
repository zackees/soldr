"""Unit tests for the tests-tree cook driver (soldr#3042).

Covers the pure helpers only. `main` shells out to a source-built `soldr`,
which is not available in this test environment, so it is not covered here;
the step that invokes it is instead checked by
`test_build_and_test_guards.py`, which asserts its position and wiring in
`_build-and-test.yml` (string matching, not execution).
"""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "cook_dylint_tests_tree.py"


def _module():
    return load_script_module(SCRIPT, "cook_dylint_tests_tree")


def test_lint_roots_finds_only_dirs_with_a_cargo_toml(tmp_path: Path) -> None:
    module = _module()
    lints_dir = tmp_path / "dylints"
    for name in ("zzz_lint", "aaa_lint"):
        crate = lints_dir / name
        crate.mkdir(parents=True)
        (crate / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
    decoy = lints_dir / "not_a_crate"
    decoy.mkdir()
    (decoy / "README.md").write_text("not a manifest\n", encoding="utf-8")

    roots = module.lint_roots(tmp_path)

    assert roots == [lints_dir / "aaa_lint", lints_dir / "zzz_lint"]


def test_lint_roots_on_the_real_repo_finds_all_six_lints() -> None:
    module = _module()
    roots = module.lint_roots(REPO_ROOT)
    assert [root.name for root in roots] == [
        "ban_platform_cfg_outside_boundary",
        "ban_raw_env_flag",
        "ban_raw_ipc_transport",
        "ban_raw_local_socket_name",
        "ban_raw_network_access",
        "ban_raw_process_creation",
    ]


def test_cook_command_shape(tmp_path: Path) -> None:
    module = _module()
    soldr = tmp_path / "soldr"
    target_root = tmp_path / "target"

    command = module.cook_command(soldr, target_root)

    assert command[0] == str(soldr)
    assert "--tree" in command
    assert command[command.index("--tree") + 1] == "tests"
    assert "--tests" in command
    assert "--json" in command
    assert "--target-root" in command
    assert command[command.index("--target-root") + 1] == str(target_root)


def test_cook_env_sets_and_removes_expected_vars(tmp_path: Path) -> None:
    module = _module()
    soldr = tmp_path / "soldr"
    base = {
        "PATH": "/usr/bin",
        "CARGO_BUILD_JOBS": "1",
        "SOLDR_JOBS": "1",
        "CARGO_TARGET_DIR": "/somewhere",
    }

    env = module.cook_env(base, soldr)

    assert env["SOLDR_RUSTC_WRAPPER"] == str(soldr)
    assert env["SOLDR_LINKER"] == "default"
    assert env["SOLDR_NO_GC_TARGET"] == "1"
    assert "CARGO_BUILD_JOBS" not in env
    assert "SOLDR_JOBS" not in env
    assert "CARGO_TARGET_DIR" not in env
    assert env["PATH"] == "/usr/bin"


def test_parse_outcome_handles_a_trailing_blank_line() -> None:
    module = _module()
    stdout = '{"outcome": "miss"}\n\n'
    assert module.parse_outcome(stdout) == "miss"


def test_parse_outcome_returns_unknown_for_non_json_payload() -> None:
    module = _module()
    assert module.parse_outcome("not json at all\n") == "unknown"


def test_parse_outcome_skips_relayed_cargo_output_after_the_payload() -> None:
    """The payload is printed last, but the child Cargo shares the stream.

    Treating a stray trailing line as "no result" would report `unknown` for
    a cook that in fact reported one -- the log line is the only visibility
    this step has into hit/miss/skip.
    """
    module = _module()
    stdout = '{"outcome": "skip"}\nwarning: something arrived afterwards\n'
    assert module.parse_outcome(stdout) == "skip"


def test_a_repo_root_with_no_lint_crates_fails_rather_than_no_ops(
    tmp_path: Path,
) -> None:
    """An empty run must be loud.

    A wrong `--repo-root` would otherwise make the step succeed at nothing,
    and the only symptom would be the third-party dependency layer quietly
    compiling inside the concurrent Dylint UI-test / Fresh Nextest window
    again -- exactly the contention soldr#3042 removes. No subprocess is
    launched on this path, so the assertion needs no `soldr` binary.
    """
    module = _module()
    code = module.main(
        [
            "--soldr",
            str(tmp_path / "soldr"),
            "--target-root",
            str(tmp_path / "target"),
            "--repo-root",
            str(tmp_path),
        ]
    )
    assert code == 1
