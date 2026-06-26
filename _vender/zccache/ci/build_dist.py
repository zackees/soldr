#!/usr/bin/env python3
"""Build native binaries via GitHub Actions and assemble dist/ for packaging.

Triggers the build.yml workflow on GitHub Actions runners, waits for
completion, downloads artifacts, and organizes them into dist/ ready
for Python wheel packaging.

Usage:
    uv run python ci/build_dist.py [--ref main] [--repo owner/repo] [--timeout 1800]
    uv run python ci/build_dist.py --skip-build   # just re-download latest artifacts
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

# Map GitHub Actions artifact names to platform wheel tags
TARGETS = {
    "binaries-x86_64-unknown-linux-musl": {
        "wheel_plat": "musllinux_1_2_x86_64",
        "subdir": "linux-x86_64",
    },
    "binaries-aarch64-unknown-linux-musl": {
        "wheel_plat": "musllinux_1_2_aarch64",
        "subdir": "linux-aarch64",
    },
    "binaries-x86_64-unknown-linux-gnu": {
        "wheel_plat": "manylinux_2_17_x86_64",
        "subdir": "linux-x86_64-gnu",
    },
    "binaries-aarch64-unknown-linux-gnu": {
        "wheel_plat": "manylinux_2_17_aarch64",
        "subdir": "linux-aarch64-gnu",
    },
    "binaries-x86_64-apple-darwin": {
        "wheel_plat": "macosx_10_12_x86_64",
        "subdir": "macos-x86_64",
    },
    "binaries-aarch64-apple-darwin": {
        "wheel_plat": "macosx_11_0_arm64",
        "subdir": "macos-aarch64",
    },
    "binaries-x86_64-pc-windows-msvc": {
        "wheel_plat": "win_amd64",
        "subdir": "windows-x86_64",
    },
    "binaries-aarch64-pc-windows-msvc": {
        "wheel_plat": "win_arm64",
        "subdir": "windows-arm64",
    },
}

WORKFLOW_FILE = "build.yml"
DIST_DIR = Path("dist")


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    """Run a command, print it, and return the result."""
    print(f"  $ {' '.join(cmd)}", file=sys.stderr)
    return subprocess.run(cmd, check=True, **kwargs)


def run_capture(cmd: list[str]) -> str:
    """Run a command and return stripped stdout."""
    result = run(cmd, capture_output=True, text=True)
    return result.stdout.strip()


def detect_repo() -> str:
    """Detect the GitHub repo from git remote."""
    try:
        url = run_capture(["git", "remote", "get-url", "origin"])
    except subprocess.CalledProcessError:
        print("ERROR: No git remote 'origin' found.", file=sys.stderr)
        sys.exit(1)

    # Handle SSH: git@github.com:owner/repo.git
    if url.startswith("git@"):
        url = url.split(":", 1)[1]
    # Handle HTTPS: https://github.com/owner/repo.git
    elif "github.com" in url:
        url = url.split("github.com/", 1)[1]

    return url.removesuffix(".git")


def trigger_workflow(repo: str, ref: str) -> int:
    """Trigger build.yml and return the run ID."""
    print(f"\n==> Triggering build workflow on {repo} @ {ref}", file=sys.stderr)

    # Get current run count to detect the new run
    existing = run_capture([
        "gh", "run", "list",
        "--repo", repo,
        "--workflow", WORKFLOW_FILE,
        "--limit", "1",
        "--json", "databaseId",
    ])
    existing_ids = {r["databaseId"] for r in json.loads(existing)} if existing else set()

    # Trigger the workflow
    run(["gh", "workflow", "run", WORKFLOW_FILE, "--repo", repo, "--ref", ref])

    # Poll for the new run to appear (takes a few seconds)
    print("  Waiting for run to appear...", file=sys.stderr)
    for _ in range(30):
        time.sleep(2)
        result = run_capture([
            "gh", "run", "list",
            "--repo", repo,
            "--workflow", WORKFLOW_FILE,
            "--limit", "5",
            "--json", "databaseId,status",
        ])
        runs = json.loads(result)
        for r in runs:
            if r["databaseId"] not in existing_ids:
                run_id = r["databaseId"]
                print(f"  Run started: {run_id}", file=sys.stderr)
                return run_id

    print("ERROR: Timed out waiting for workflow run to appear.", file=sys.stderr)
    sys.exit(1)


def download_failed_logs(repo: str, run_id: int) -> list[Path]:
    """Download logs for failed jobs, organized per target.

    Returns a list of log file paths that were saved.
    """
    print(f"\n==> Downloading logs for failed jobs in run {run_id}", file=sys.stderr)

    logs_dir = DIST_DIR / "logs"
    logs_dir.mkdir(parents=True, exist_ok=True)

    # Identify which jobs failed
    try:
        jobs_raw = run_capture([
            "gh", "run", "view", str(run_id),
            "--repo", repo,
            "--json", "jobs",
        ])
        jobs = json.loads(jobs_raw).get("jobs", [])
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        jobs = []

    failed_jobs: dict[str, str] = {}  # job name -> target triple
    for job in jobs:
        if job.get("conclusion") == "failure":
            name = job.get("name", "")
            # Extract target triple from job name like "Build (x86_64-unknown-linux-musl)"
            m = re.search(r"\(([^)]+)\)", name)
            target = m.group(1) if m else name
            failed_jobs[name] = target

    if not failed_jobs:
        print("  No failed jobs found in run metadata.", file=sys.stderr)
        return []

    print(f"  Failed targets: {', '.join(failed_jobs.values())}", file=sys.stderr)

    # Download the failed logs (tab-delimited: job\tstep\tlog_line)
    try:
        result = subprocess.run(
            ["gh", "run", "view", str(run_id), "--repo", repo, "--log-failed"],
            capture_output=True, text=True, timeout=120,
        )
        raw_output = result.stdout or result.stderr or ""
    except (subprocess.TimeoutExpired, FileNotFoundError) as e:
        print(f"  WARNING: Could not download logs: {e}", file=sys.stderr)
        return []

    # Group log lines by job name
    per_job: dict[str, list[str]] = {name: [] for name in failed_jobs}
    for line in raw_output.splitlines():
        parts = line.split("\t", 2)
        if len(parts) >= 2:
            job_name = parts[0]
            log_line = parts[2] if len(parts) == 3 else parts[1]
            if job_name in per_job:
                per_job[job_name].append(log_line)
            else:
                # Fuzzy match — gh sometimes abbreviates job names
                for known in per_job:
                    if known.startswith(job_name) or job_name.startswith(known):
                        per_job[known].append(log_line)
                        break

    # Save per-target log files and display
    saved: list[Path] = []
    preview_lines = 30
    for job_name, target in failed_jobs.items():
        lines = per_job.get(job_name, [])
        log_file = logs_dir / f"failed-{target}-{run_id}.log"
        log_file.write_text("\n".join(lines) + "\n" if lines else "(no log output)\n", encoding="utf-8")
        saved.append(log_file)

        print(f"\n  --- {target} ({len(lines)} lines) ---", file=sys.stderr)
        print(f"  Log: {log_file}", file=sys.stderr)
        if len(lines) > preview_lines:
            print(f"  ... (showing last {preview_lines} of {len(lines)} lines)", file=sys.stderr)
            for l in lines[-preview_lines:]:
                print(f"  | {l}", file=sys.stderr)
        else:
            for l in lines:
                print(f"  | {l}", file=sys.stderr)

    return saved


def wait_for_run(repo: str, run_id: int, timeout: int) -> None:
    """Wait for a workflow run to complete."""
    print(f"\n==> Waiting for run {run_id} to complete (timeout: {timeout}s)", file=sys.stderr)
    print(f"  Watch live: https://github.com/{repo}/actions/runs/{run_id}", file=sys.stderr)

    deadline = time.time() + timeout
    while time.time() < deadline:
        result = run_capture([
            "gh", "run", "view", str(run_id),
            "--repo", repo,
            "--json", "status,conclusion",
        ])
        data = json.loads(result)
        status = data["status"]
        conclusion = data.get("conclusion", "")

        if status == "completed":
            if conclusion == "success":
                print(f"  Run completed successfully.", file=sys.stderr)
                return
            else:
                print(f"ERROR: Run finished with conclusion: {conclusion}", file=sys.stderr)
                print(f"  See: https://github.com/{repo}/actions/runs/{run_id}", file=sys.stderr)
                download_failed_logs(repo, run_id)
                sys.exit(1)

        secs = int(time.time() - (deadline - timeout))
        print(f"  Status: {status} (elapsed: ~{secs}s)", file=sys.stderr)
        time.sleep(15)

    print(f"ERROR: Timed out after {timeout}s.", file=sys.stderr)
    sys.exit(1)


def find_latest_run(repo: str) -> int:
    """Find the latest successful build run."""
    result = run_capture([
        "gh", "run", "list",
        "--repo", repo,
        "--workflow", WORKFLOW_FILE,
        "--status", "success",
        "--limit", "1",
        "--json", "databaseId",
    ])
    runs = json.loads(result)
    if not runs:
        print("ERROR: No successful build runs found.", file=sys.stderr)
        sys.exit(1)
    return runs[0]["databaseId"]


def download_artifacts(repo: str, run_id: int) -> None:
    """Download artifacts and organize into dist/."""
    print(f"\n==> Downloading artifacts from run {run_id}", file=sys.stderr)

    # Clean and recreate dist/
    if DIST_DIR.exists():
        shutil.rmtree(DIST_DIR)
    DIST_DIR.mkdir()

    # Download all artifacts into a temp directory
    tmp = DIST_DIR / "_tmp"
    tmp.mkdir()
    run([
        "gh", "run", "download", str(run_id),
        "--repo", repo,
        "--dir", str(tmp),
    ])

    # Organize into platform subdirectories
    found_targets = 0
    for artifact_name, info in TARGETS.items():
        src = tmp / artifact_name
        if not src.exists():
            print(f"  WARNING: Missing artifact {artifact_name}", file=sys.stderr)
            continue

        dest = DIST_DIR / info["subdir"]
        dest.mkdir(parents=True, exist_ok=True)

        for f in src.iterdir():
            target = dest / f.name
            shutil.copy2(f, target)
            # Ensure executables are marked executable on Unix
            if not f.name.endswith(".exe"):
                target.chmod(0o755)
            size_mb = target.stat().st_size / (1024 * 1024)
            print(f"  {info['subdir']}/{f.name} ({size_mb:.1f} MB)", file=sys.stderr)

        found_targets += 1

    # Clean up temp
    shutil.rmtree(tmp)

    # Write a manifest for downstream tooling
    manifest = {
        target_info["subdir"]: {
            "wheel_plat": target_info["wheel_plat"],
            "binaries": [
                f.name for f in (DIST_DIR / target_info["subdir"]).iterdir()
            ] if (DIST_DIR / target_info["subdir"]).exists() else [],
        }
        for target_info in TARGETS.values()
    }
    manifest_path = DIST_DIR / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")

    print(f"\n==> dist/ ready: {found_targets}/{len(TARGETS)} platforms", file=sys.stderr)
    if found_targets < len(TARGETS):
        print("  Some platforms are missing — check the workflow logs.", file=sys.stderr)


def print_summary() -> None:
    """Print the dist/ tree."""
    print("\n==> dist/ layout:", file=sys.stderr)
    for p in sorted(DIST_DIR.rglob("*")):
        if p.is_file():
            rel = p.relative_to(DIST_DIR)
            size_mb = p.stat().st_size / (1024 * 1024)
            print(f"  {rel}  ({size_mb:.1f} MB)", file=sys.stderr)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Build native binaries via GitHub Actions and assemble dist/"
    )
    parser.add_argument(
        "--ref", default="main",
        help="Git ref to build (branch, tag, or SHA). Default: main",
    )
    parser.add_argument(
        "--repo",
        help="GitHub repo (owner/repo). Auto-detected from git remote if omitted.",
    )
    parser.add_argument(
        "--timeout", type=int, default=1800,
        help="Max seconds to wait for the build. Default: 1800 (30 min)",
    )
    parser.add_argument(
        "--skip-build", action="store_true",
        help="Skip triggering a new build; download artifacts from the latest successful run.",
    )
    parser.add_argument(
        "--run-id", type=int,
        help="Download artifacts from a specific run ID instead of triggering a new one.",
    )
    args = parser.parse_args()

    # Verify gh is available
    try:
        run_capture(["gh", "--version"])
    except FileNotFoundError:
        print("ERROR: 'gh' (GitHub CLI) is not installed.", file=sys.stderr)
        print("  Install: https://cli.github.com/", file=sys.stderr)
        sys.exit(1)

    repo = args.repo or detect_repo()
    print(f"Repository: {repo}", file=sys.stderr)

    if args.run_id:
        run_id = args.run_id
    elif args.skip_build:
        run_id = find_latest_run(repo)
        print(f"Using latest successful run: {run_id}", file=sys.stderr)
    else:
        run_id = trigger_workflow(repo, args.ref)
        wait_for_run(repo, run_id, args.timeout)

    download_artifacts(repo, run_id)
    print_summary()


if __name__ == "__main__":
    main()
