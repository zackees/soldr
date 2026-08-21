#!/usr/bin/env python3
"""Create the release tag and the draft GitHub Release, idempotently.

The two mutating steps of the publish job, extracted from release-auto.yml
(soldr#2469 step 2.2). Both were inline bash: check-then-act, with a
`GITHUB_STEP_SUMMARY` block emitting copy-pasteable recovery commands when the
token cannot perform the mutation (soldr#1252).

Behavior is preserved exactly, including the check-first idempotency that makes
a rerun after a partial success safe -- an existing tag is a skip, and an
existing release is an asset re-upload with `--clobber` rather than a failure.

One thing genuinely changes. The inline steps passed `dist/*` to `gh`, relying
on the shell to expand it; here the glob is expanded in Python. That closes a
real hole: with no matches, bash passes the literal string `dist/*` through and
`gh` fails with a confusing "path not found" deep inside the upload, so an
empty `dist/` looked like a `gh` problem. `release_assets()` raises a named
error instead, and it is tested.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path
from typing import Callable, Sequence

Runner = Callable[[Sequence[str]], subprocess.CompletedProcess]

BLOCKED_NOTE = (
    "Registry publishes are blocked until the GitHub release tag/assets "
    "can be created. See soldr#1252."
)


def default_runner(args: Sequence[str]) -> subprocess.CompletedProcess:
    """Run a command, capturing output; never raises on non-zero."""
    return subprocess.run(list(args), capture_output=True, text=True, check=False)


def release_assets(dist: Path) -> list[str]:
    """Files to upload, sorted for a deterministic command line.

    Raises when empty: the inline `dist/*` silently degraded to a literal
    argument here, which surfaced as an unrelated-looking `gh` error.
    """
    if not dist.is_dir():
        raise FileNotFoundError(f"release asset directory does not exist: {dist}")
    files = sorted(str(path) for path in dist.iterdir() if path.is_file())
    if not files:
        raise FileNotFoundError(f"no release assets found in {dist}")
    return files


def tag_recovery_summary(repo: str, tag: str, sha: str) -> str:
    """Copy-pasteable recovery for a tag the token could not create."""
    return "\n".join(
        [
            "## Manual release recovery needed (tag)",
            "```",
            f"gh api repos/{repo}/git/refs -X POST -f ref=refs/tags/{tag} -f sha={sha}",
            "```",
            "",
        ]
    )


def release_recovery_summary(repo: str, tag: str, sha: str, run_id: str) -> str:
    """Copy-pasteable recovery for a release the token could not create."""
    return "\n".join(
        [
            "## Manual release recovery needed (GitHub release)",
            "```",
            f"gh run download {run_id} --repo {repo} "
            "--pattern 'release-soldr-*' --dir dist",
            f"gh release create {tag} dist/* --repo {repo} "
            f"--generate-notes --target {sha} --title {tag}",
            "```",
            "",
        ]
    )


def append_summary(text: str) -> None:
    """Append to the job summary when GitHub provides one."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(text)


def write_output(name: str, value: str) -> None:
    """Set a step output when GitHub provides the file."""
    path = os.environ.get("GITHUB_OUTPUT")
    if not path:
        return
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(f"{name}={value}\n")


def ensure_tag(repo: str, tag: str, sha: str, run: Runner) -> int:
    """Create `tag` at `sha` unless it already exists."""
    if run(["gh", "api", f"repos/{repo}/git/refs/tags/{tag}"]).returncode == 0:
        print(f"tag {tag} already exists - skipping create")
        return 0
    created = run(
        [
            "gh",
            "api",
            f"repos/{repo}/git/refs",
            "-X",
            "POST",
            "-f",
            f"ref=refs/tags/{tag}",
            "-f",
            f"sha={sha}",
        ]
    )
    if created.returncode == 0:
        return 0
    print(
        f"::error::GITHUB_TOKEN could not create tag {tag} via /git/refs. "
        f"{BLOCKED_NOTE}"
    )
    append_summary(tag_recovery_summary(repo, tag, sha))
    return 1


def create_draft_release(
    repo: str, tag: str, sha: str, *, run_id: str, dist: Path, run: Runner
) -> int:
    """Create the draft release, or re-upload assets if it already exists."""
    assets = release_assets(dist)
    if run(["gh", "release", "view", tag, "--repo", repo]).returncode == 0:
        print(f"release {tag} already exists; uploading/replacing assets")
        uploaded = run(
            ["gh", "release", "upload", tag, *assets, "--repo", repo, "--clobber"]
        )
        if uploaded.returncode != 0:
            # The inline version failed here too -- `set -e` aborted the step
            # on a non-zero `gh release upload`, so `created=true` was never
            # written. What it did not do was say anything: the step died with
            # gh's own message and no recovery block. This emits the same
            # guidance the create path already emitted, for the same reason.
            print(
                f"::error::GITHUB_TOKEN could not re-upload assets for {tag}. "
                f"{BLOCKED_NOTE}"
            )
            write_output("created", "false")
            append_summary(release_recovery_summary(repo, tag, sha, run_id))
            return 1
        write_output("created", "true")
        return 0

    created = run(
        [
            "gh",
            "release",
            "create",
            tag,
            *assets,
            "--repo",
            repo,
            "--draft",
            "--generate-notes",
            "--target",
            sha,
            "--title",
            tag,
        ]
    )
    if created.returncode == 0:
        write_output("created", "true")
        return 0
    write_output("created", "false")
    print(
        f"::error::GITHUB_TOKEN could not create the GitHub release for {tag}. "
        "Registry publishes are blocked until the GitHub release exists with "
        "the expected assets. See soldr#1252."
    )
    append_summary(release_recovery_summary(repo, tag, sha, run_id))
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["ensure-tag", "create-draft"])
    parser.add_argument("--tag", required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument(
        "--repo", default=os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")
    )
    parser.add_argument("--dist", default="dist")
    parser.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    args = parser.parse_args(argv)

    if args.command == "ensure-tag":
        return ensure_tag(args.repo, args.tag, args.sha, default_runner)
    try:
        return create_draft_release(
            args.repo,
            args.tag,
            args.sha,
            run_id=args.run_id,
            dist=Path(args.dist),
            run=default_runner,
        )
    except FileNotFoundError as error:
        print(f"::error::{error}", file=sys.stderr)
        write_output("created", "false")
        return 1


if __name__ == "__main__":
    sys.exit(main())
