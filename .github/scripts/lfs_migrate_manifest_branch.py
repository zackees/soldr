#!/usr/bin/env python3
"""Convert vendored binary assets on the `manifest` branch into Git LFS objects.

Run from a clean checkout of the `manifest` branch; review the rewritten
history with `git log` before pushing. This script DOES NOT push.

Owner-runbook context: see ``docs/MANIFEST_LFS_MIGRATION.md``. This script
performs step 2.3 of that runbook and stops short of step 2.5 (the actual
``git push``). The separation is deliberate — enabling LFS on a public repo
affects bandwidth billing irreversibly at scale.

What the script does, in order:
    1. Verify ``git lfs version`` succeeds (Git LFS is installed).
    2. Verify the current branch is ``manifest`` and the working tree is clean.
    3. Snapshot ``.git`` size before the rewrite.
    4. Run ``git lfs migrate import --include=<LFS_PATTERNS> --everything``.
    5. Snapshot ``.git`` size after, count migrated objects, print a summary.
    6. Stop. Do not push. Tell the operator the exact push commands.

Flags:
    --dry-run         Print the planned actions and exit 0 without running
                      any git operations. Used by the smoke test and by
                      operators who want to confirm pattern coverage first.
    --include PATTERN Override the comma-separated include patterns.
                      Default: ``deps/**/*.tar.zst,deps/**/*.tar.xz,
                      deps/**/*.zip,deps/**/*.tar.gz``.
    --branch NAME     Branch the working tree must be on. Default ``manifest``.
                      Tests override this so they can drive the pre-flight
                      against synthetic repos.
    --skip-clean-check
                      Skip the dirty-worktree check. Documented but
                      intentionally not advertised in --help summary — used
                      only by the smoke test against synthetic repos.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

DEFAULT_INCLUDE = "deps/**/*.tar.zst,deps/**/*.tar.xz,deps/**/*.zip,deps/**/*.tar.gz"
DEFAULT_BRANCH = "manifest"

# Exit-code contract — kept stable so the test suite can assert against
# specific failure modes without parsing stderr.
EXIT_OK = 0
EXIT_USAGE = 2
EXIT_NO_LFS = 10
EXIT_WRONG_BRANCH = 11
EXIT_DIRTY = 12
EXIT_MIGRATE_FAILED = 13


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse CLI arguments. Exposed for unit testing."""
    parser = argparse.ArgumentParser(
        description=(
            "Convert vendored deps/ assets on the manifest branch into "
            "Git LFS objects. Does NOT push."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "After a successful run, manually inspect history then push:\n"
            "    git log --oneline -20\n"
            "    git lfs ls-files | head\n"
            "    git lfs push --all origin manifest\n"
            "    git push --force-with-lease origin manifest\n"
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the planned actions and exit without touching git.",
    )
    parser.add_argument(
        "--include",
        default=DEFAULT_INCLUDE,
        help=f"Comma-separated include patterns (default: {DEFAULT_INCLUDE}).",
    )
    parser.add_argument(
        "--branch",
        default=DEFAULT_BRANCH,
        help=f"Required branch name (default: {DEFAULT_BRANCH}).",
    )
    parser.add_argument(
        "--repo",
        default=".",
        help="Path to the repo checkout (default: current directory).",
    )
    parser.add_argument(
        "--skip-clean-check",
        action="store_true",
        help=argparse.SUPPRESS,
    )
    return parser.parse_args(argv)


def have_git_lfs() -> bool:
    """Return True iff `git lfs version` succeeds."""
    if shutil.which("git") is None:
        return False
    try:
        result = subprocess.run(
            ["git", "lfs", "version"],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


def current_branch(repo: Path) -> str | None:
    """Return the current branch name, or None on detached HEAD / error."""
    try:
        result = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    name = result.stdout.strip()
    if not name or name == "HEAD":
        return None
    return name


def worktree_is_clean(repo: Path) -> bool:
    """Return True iff `git status --porcelain` is empty."""
    try:
        result = subprocess.run(
            ["git", "-C", str(repo), "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0 and result.stdout.strip() == ""


def dir_size_bytes(path: Path) -> int:
    """Recursive du-style byte count. Returns 0 if path does not exist."""
    if not path.exists():
        return 0
    total = 0
    for entry in path.rglob("*"):
        try:
            if entry.is_file():
                total += entry.stat().st_size
        except OSError:
            # Concurrent gc or symlink loop: skip silently rather than
            # fail the summary.
            continue
    return total


def fmt_bytes(n: int) -> str:
    """Human-friendly byte size."""
    units = ["B", "KiB", "MiB", "GiB", "TiB"]
    val = float(n)
    idx = 0
    while val >= 1024.0 and idx < len(units) - 1:
        val /= 1024.0
        idx += 1
    return f"{val:.1f} {units[idx]}"


def preflight(args: argparse.Namespace) -> int:
    """Run all read-only checks. Return 0 on success, non-zero exit code on failure."""
    repo = Path(args.repo).resolve()
    if not (repo / ".git").exists():
        print(f"ERROR: {repo} is not a git checkout.", file=sys.stderr)
        return EXIT_USAGE

    if not have_git_lfs():
        print(
            "ERROR: `git lfs version` failed. Install Git LFS first:\n"
            "  macOS:   brew install git-lfs\n"
            "  Linux:   sudo apt install git-lfs   (or distro equivalent)\n"
            "  Windows: winget install GitHub.GitLFS\n"
            "Then run `git lfs install` once per machine.",
            file=sys.stderr,
        )
        return EXIT_NO_LFS

    branch = current_branch(repo)
    if branch != args.branch:
        print(
            f"ERROR: current branch is {branch!r}, expected {args.branch!r}. "
            f"Run `git checkout {args.branch}` first.",
            file=sys.stderr,
        )
        return EXIT_WRONG_BRANCH

    if not args.skip_clean_check and not worktree_is_clean(repo):
        print(
            "ERROR: working tree is dirty. Commit or stash changes before "
            "running the migration (git lfs migrate import rewrites every "
            "reachable commit).",
            file=sys.stderr,
        )
        return EXIT_DIRTY

    return EXIT_OK


def run_migration(args: argparse.Namespace) -> int:
    """Execute the actual `git lfs migrate import`. Returns exit code."""
    repo = Path(args.repo).resolve()
    git_dir = repo / ".git"
    before = dir_size_bytes(git_dir)
    print(f"[lfs-migrate] .git size before: {fmt_bytes(before)}")
    print(f"[lfs-migrate] include patterns: {args.include}")
    print("[lfs-migrate] running: git lfs migrate import --everything ...")

    cmd = [
        "git",
        "-C",
        str(repo),
        "lfs",
        "migrate",
        "import",
        f"--include={args.include}",
        "--everything",
    ]
    try:
        result = subprocess.run(cmd, check=False)
    except (OSError, subprocess.SubprocessError) as exc:
        print(f"ERROR: failed to invoke git lfs migrate: {exc}", file=sys.stderr)
        return EXIT_MIGRATE_FAILED
    if result.returncode != 0:
        print(
            f"ERROR: git lfs migrate import exited {result.returncode}",
            file=sys.stderr,
        )
        return EXIT_MIGRATE_FAILED

    after = dir_size_bytes(git_dir)
    print(f"[lfs-migrate] .git size after:  {fmt_bytes(after)}")
    delta = after - before
    sign = "+" if delta >= 0 else "-"
    print(f"[lfs-migrate] delta:            {sign}{fmt_bytes(abs(delta))}")

    # Count LFS-tracked files after the migration.
    try:
        lsfiles = subprocess.run(
            ["git", "-C", str(repo), "lfs", "ls-files"],
            capture_output=True,
            text=True,
            check=False,
        )
        n_lfs = sum(1 for line in lsfiles.stdout.splitlines() if line.strip())
        print(f"[lfs-migrate] LFS-tracked files: {n_lfs}")
    except (OSError, subprocess.SubprocessError):
        print("[lfs-migrate] (could not run `git lfs ls-files` to count)")

    print()
    print("Next steps (NOT performed by this script):")
    print("  1. Inspect rewritten history:")
    print("       git log --oneline -20")
    print("       git lfs ls-files | head")
    print("  2. If satisfied, push LFS objects then ref:")
    print("       git lfs push --all origin manifest")
    print("       git push --force-with-lease origin manifest")
    print("  3. Add `.gitattributes` per docs/MANIFEST_LFS_MIGRATION.md §2.6.")
    return EXIT_OK


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)

    if args.dry_run:
        print("[lfs-migrate] DRY RUN — no git operations will be executed.")
        print(f"[lfs-migrate] repo:    {Path(args.repo).resolve()}")
        print(f"[lfs-migrate] branch:  {args.branch}")
        print(f"[lfs-migrate] include: {args.include}")
        print("[lfs-migrate] would verify: git lfs version, current branch,")
        print("[lfs-migrate]               working-tree cleanliness")
        print(
            "[lfs-migrate] would run:    git lfs migrate import "
            f"--include={args.include} --everything"
        )
        return EXIT_OK

    code = preflight(args)
    if code != EXIT_OK:
        return code
    return run_migration(args)


if __name__ == "__main__":
    sys.exit(main())
