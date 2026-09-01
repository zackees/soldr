from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_nextest_bare_cargo.py"
guard = load_script_module(SCRIPT)


def test_current_nextest_tree_has_no_literal_bare_cargo_launches() -> None:
    assert guard.ripgrep_bare_cargo() == ()


def test_ripgrep_names_a_new_literal_bare_cargo_launch(tmp_path: Path) -> None:
    source = tmp_path / "crates" / "demo" / "tests" / "fixture.rs"
    source.parent.mkdir(parents=True)
    source.write_text(
        'fn bad() { let _ = std::process::Command::new("cargo").status(); }\n',
        encoding="utf-8",
    )

    matches = guard.ripgrep_bare_cargo(tmp_path)
    assert len(matches) == 1
    assert "fixture.rs" in matches[0]
    assert 'Command::new("cargo")' in matches[0]


def test_lint_job_runs_the_ripgrep_guard() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )
    assert "check_nextest_bare_cargo.py" in workflow
