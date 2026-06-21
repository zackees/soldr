"""Tests for the LFS migration script (issue #872, prep).

Validates argument parsing, the ``--dry-run`` plan-only path, the
exit-code contract on the preflight checks, and the ``--help`` text.
Synthetic repos are built inline with ``tempfile`` so the suite never
performs a real ``git lfs migrate import`` (which would rewrite history).
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "lfs_migrate_manifest_branch.py"


def _load_module():
    spec = importlib.util.spec_from_file_location(
        "lfs_migrate_manifest_branch", SCRIPT_PATH
    )
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["lfs_migrate_manifest_branch"] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture(scope="module")
def mod():
    return _load_module()


# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------


def test_default_arguments(mod):
    ns = mod.parse_args([])
    assert ns.dry_run is False
    assert ns.include == mod.DEFAULT_INCLUDE
    assert ns.branch == mod.DEFAULT_BRANCH
    assert ns.repo == "."
    assert ns.skip_clean_check is False


def test_dry_run_flag(mod):
    ns = mod.parse_args(["--dry-run"])
    assert ns.dry_run is True


def test_override_include(mod):
    ns = mod.parse_args(["--include", "deps/**/*.bin"])
    assert ns.include == "deps/**/*.bin"


def test_override_branch(mod):
    ns = mod.parse_args(["--branch", "vendor"])
    assert ns.branch == "vendor"


def test_default_include_covers_documented_extensions(mod):
    """The runbook documents these four extensions — keep them in sync."""
    for ext in ("*.tar.zst", "*.tar.xz", "*.zip", "*.tar.gz"):
        assert (
            f"deps/**/{ext}" in mod.DEFAULT_INCLUDE
        ), f"DEFAULT_INCLUDE drifted from the runbook: missing {ext}"


def test_exit_code_constants_distinct(mod):
    codes = [
        mod.EXIT_OK,
        mod.EXIT_USAGE,
        mod.EXIT_NO_LFS,
        mod.EXIT_WRONG_BRANCH,
        mod.EXIT_DIRTY,
        mod.EXIT_MIGRATE_FAILED,
    ]
    assert len(set(codes)) == len(codes), "exit-code constants must be unique"
    assert mod.EXIT_OK == 0


# ---------------------------------------------------------------------------
# helpers — byte formatting
# ---------------------------------------------------------------------------


def test_fmt_bytes_units(mod):
    assert mod.fmt_bytes(0) == "0.0 B"
    assert mod.fmt_bytes(1023) == "1023.0 B"
    assert mod.fmt_bytes(1024) == "1.0 KiB"
    assert mod.fmt_bytes(1024 * 1024) == "1.0 MiB"
    assert mod.fmt_bytes(1024 * 1024 * 1024) == "1.0 GiB"


def test_dir_size_missing_path(mod, tmp_path):
    assert mod.dir_size_bytes(tmp_path / "does-not-exist") == 0


def test_dir_size_counts_files(mod, tmp_path):
    (tmp_path / "a.bin").write_bytes(b"x" * 100)
    (tmp_path / "sub").mkdir()
    (tmp_path / "sub" / "b.bin").write_bytes(b"y" * 250)
    assert mod.dir_size_bytes(tmp_path) == 350


# ---------------------------------------------------------------------------
# --help / CLI subprocess surface
# ---------------------------------------------------------------------------


def test_help_subprocess():
    """The script must respond to --help without import-time side effects."""
    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), "--help"],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    out = result.stdout
    assert "manifest branch" in out
    assert "--dry-run" in out
    assert "--include" in out
    # The post-run push instructions must appear in epilog so operators see
    # them without re-reading the source.
    assert "git lfs push --all origin manifest" in out
    assert "git push --force-with-lease origin manifest" in out


def test_dry_run_subprocess(tmp_path):
    """--dry-run prints the plan and exits 0 without touching git."""
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT_PATH),
            "--dry-run",
            "--repo",
            str(tmp_path),  # not a git repo — dry-run must not care
        ],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    assert "DRY RUN" in result.stdout
    assert "would run:" in result.stdout
    assert "git lfs migrate import" in result.stdout


# ---------------------------------------------------------------------------
# preflight — exit-code contract
# ---------------------------------------------------------------------------


def test_preflight_rejects_non_repo(mod, tmp_path):
    ns = mod.parse_args(["--repo", str(tmp_path)])
    rc = mod.preflight(ns)
    assert rc == mod.EXIT_USAGE


def _init_git_repo(repo: Path, branch: str) -> bool:
    """Best-effort: init a synthetic repo on ``branch``. Skip test if git absent."""
    try:
        subprocess.run(
            ["git", "init", "-q", "-b", branch, str(repo)],
            check=True,
            capture_output=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError):
        return False
    # Minimal config so commits work in CI.
    for kv in (("user.email", "test@example.invalid"), ("user.name", "Test")):
        subprocess.run(
            ["git", "-C", str(repo), "config", kv[0], kv[1]],
            check=True,
            capture_output=True,
        )
    # Empty initial commit so HEAD resolves.
    subprocess.run(
        ["git", "-C", str(repo), "commit", "--allow-empty", "-m", "init", "-q"],
        check=True,
        capture_output=True,
    )
    return True


def test_preflight_rejects_wrong_branch(mod, tmp_path):
    """If git+lfs are installed, preflight must reject the wrong branch."""
    if not mod.have_git_lfs():
        pytest.skip("git lfs not installed on this host")
    if not _init_git_repo(tmp_path, "main"):
        pytest.skip("git not installed on this host")
    ns = mod.parse_args(["--repo", str(tmp_path), "--branch", "manifest"])
    rc = mod.preflight(ns)
    assert rc == mod.EXIT_WRONG_BRANCH


def test_preflight_accepts_clean_matching_branch(mod, tmp_path):
    """Clean repo on the expected branch with LFS installed should preflight OK."""
    if not mod.have_git_lfs():
        pytest.skip("git lfs not installed on this host")
    if not _init_git_repo(tmp_path, "manifest"):
        pytest.skip("git not installed on this host")
    ns = mod.parse_args(["--repo", str(tmp_path), "--branch", "manifest"])
    rc = mod.preflight(ns)
    assert rc == mod.EXIT_OK


def test_preflight_rejects_dirty_worktree(mod, tmp_path):
    if not mod.have_git_lfs():
        pytest.skip("git lfs not installed on this host")
    if not _init_git_repo(tmp_path, "manifest"):
        pytest.skip("git not installed on this host")
    (tmp_path / "dirty.txt").write_text("uncommitted")
    ns = mod.parse_args(["--repo", str(tmp_path), "--branch", "manifest"])
    rc = mod.preflight(ns)
    assert rc == mod.EXIT_DIRTY


# ---------------------------------------------------------------------------
# regression: ancillary files referenced by the runbook exist
# ---------------------------------------------------------------------------


def test_runbook_exists():
    runbook = REPO_ROOT / "docs" / "MANIFEST_LFS_MIGRATION.md"
    assert runbook.exists()
    text = runbook.read_text(encoding="utf-8")
    # Make sure the runbook still points at this script by name.
    assert "lfs_migrate_manifest_branch.sh" in text


def test_gitattributes_snippet_exists():
    snippet = REPO_ROOT / ".github" / "snippets" / "manifest-branch.gitattributes"
    assert snippet.exists()
    text = snippet.read_text(encoding="utf-8")
    for ext in ("*.tar.zst", "*.tar.xz", "*.zip", "*.tar.gz"):
        assert (
            f"deps/**/{ext}" in text
        ), f"snippet missing {ext} — keep in sync with DEFAULT_INCLUDE"
    # Every line that declares a pattern must enable the lfs filter.
    for line in text.splitlines():
        if line.strip().startswith("deps/"):
            assert "filter=lfs" in line
            assert "diff=lfs" in line
            assert "merge=lfs" in line
            assert "-text" in line


def test_shell_wrapper_exists():
    wrapper = REPO_ROOT / ".github" / "scripts" / "lfs_migrate_manifest_branch.sh"
    assert wrapper.exists()
    text = wrapper.read_text(encoding="utf-8")
    assert "lfs_migrate_manifest_branch.py" in text
