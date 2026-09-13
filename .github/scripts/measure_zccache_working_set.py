#!/usr/bin/env python3
"""Measure how much of the Tier-2 zccache store one gate run uses (soldr#3120).

The `zccache-unit` cache family is 2.00 GiB and one saved store generation is
about 4 GiB, so the Repository Actions cache budget check stays red. Trimming
the store before the save is the proposed fix, and its cost depends on one
number: how many bytes of the store a single gate run actually touches. If that
working set fits under the trim cap, eviction only drops artifacts no later run
can hit. If it does not, every later gate starts partly cold.

zccache keeps artifact access times in its binary index, which is unreadable
from here, so this reads two things instead.

**Working set (read-only, on the real store).** Every file under the store is
stat'ed once per inode. A file counts as touched when its atime, ctime or mtime
is at or after `--since-file`, the epoch recorded right after the restore:

* a hit materialized by hardlink changes the inode's link count, which updates
  its ctime (ext4 on GitHub-hosted runners cannot reflink, so this is the
  normal path);
* a hit materialized by copy reads the file, which updates atime (under
  relatime, because restored files carry atime equal to mtime); and
* a miss publishes a new file, which sets mtime.

A reflink materialization touches none of these, so "nothing touched" is
reported as unmeasurable rather than as an empty working set.

**Trial trim (on a real copy).** This follows the method in the owner's
comment on soldr#3120. The store is copied, not hardlinked, because the index
is rewritten in place. `soldr gc maintain --root <copy> --json` then runs one
Full pass under `ZCCACHE_CACHE_SIZE_BYTES`. The copy is always removed, and
the saved store is never modified.

Report-only: always exits 0 once arguments parse.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import time

GIB = 1024**3
FAMILY = "zccache-unit"
# `stable-cook-*` shares the zccache-unit family (ci/cache-ownership.json);
# soldr#3120 sized its share at about 0.3 GiB, leaving about 1.7 GiB for the store.
DEFAULT_RESERVE_BYTES = 322_122_547
COPY_HEADROOM_BYTES = GIB // 2
TRIAL_TIMEOUT_SECONDS = 900


@dataclasses.dataclass
class Walk:
    total_files: int = 0
    total_bytes: int = 0
    touched_files: int = 0
    touched_bytes: int = 0
    # version directory -> [bytes, touched bytes]
    by_version: dict[str, list[int]] = dataclasses.field(default_factory=dict)


def version_bucket(store: pathlib.Path, path: pathlib.Path) -> str:
    """The zccache version directory (`v1.13.22`) a store file lives under."""
    for part in path.relative_to(store).parts[:-1]:
        if part.startswith("v") and part[1:2].isdigit():
            return part
    return "(unversioned)"


def walk_store(store: pathlib.Path, since: float) -> Walk:
    walk = Walk()
    seen: set[tuple[int, int]] = set()
    for dirpath, _dirnames, filenames in os.walk(store):
        for name in filenames:
            path = pathlib.Path(dirpath) / name
            try:
                info = path.lstat()
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(info.st_mode):
                continue
            inode = (info.st_dev, info.st_ino)
            if inode in seen:
                continue
            seen.add(inode)
            touched = max(info.st_atime, info.st_ctime, info.st_mtime) >= since
            bucket = walk.by_version.setdefault(version_bucket(store, path), [0, 0])
            walk.total_files += 1
            walk.total_bytes += info.st_size
            bucket[0] += info.st_size
            if touched:
                walk.touched_files += 1
                walk.touched_bytes += info.st_size
                bucket[1] += info.st_size
    return walk


def working_set_verdict(walk: Walk, cap_bytes: int) -> str:
    if walk.total_files == 0:
        return "store is empty: nothing to measure"
    if walk.touched_files == 0:
        return (
            "no file touched since the restore marker: unmeasurable "
            "(reflink materialization, noatime, or no compile ran)"
        )
    if walk.touched_files == walk.total_files:
        return (
            "every file touched since the marker: the marker predates the "
            "restore, so this is not a working set"
        )
    if walk.touched_bytes <= cap_bytes:
        return "working set fits under the trim cap"
    return (
        "working set exceeds the trim cap: trimming would start later gates partly cold"
    )


def default_cap_bytes(manifest_path: pathlib.Path, reserve_bytes: int) -> int:
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    family_max = int(manifest["budget"]["families"][FAMILY]["max_bytes"])
    return family_max - reserve_bytes


def read_since(path: pathlib.Path) -> float | None:
    try:
        return float(path.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return None


def parse_status(stdout: str) -> dict | None:
    """The `gc maintain --json` status object, tolerating surrounding lines."""
    start, end = stdout.find("{"), stdout.rfind("}")
    if start < 0 or end < start:
        return None
    try:
        return json.loads(stdout[start : end + 1])
    except json.JSONDecodeError:
        return None


def trial_trim(
    store: pathlib.Path,
    store_bytes: int,
    soldr: pathlib.Path,
    cap_bytes: int,
    scratch_root: pathlib.Path,
) -> dict:
    """Trim a real copy of `store` and return what happened; never raises."""
    try:
        free = shutil.disk_usage(scratch_root).free
    except OSError as error:
        return {"skipped": f"scratch root unavailable: {error}"}
    if free < store_bytes + COPY_HEADROOM_BYTES:
        return {
            "skipped": f"{free} bytes free under {scratch_root}, need {store_bytes + COPY_HEADROOM_BYTES}"
        }
    root = scratch_root / f"zccache-trial-{os.getpid()}"
    copy = root / "cache" / "zccache" / "daemon-state"
    try:
        started = time.monotonic()
        shutil.copytree(store, copy, symlinks=True)
        copy_seconds = time.monotonic() - started
        env = dict(os.environ)
        env.pop("ZCCACHE_CACHE_SIZE_PERCENT", None)
        env.pop("ZCCACHE_CACHE_DIR", None)
        env["ZCCACHE_CACHE_SIZE_BYTES"] = str(cap_bytes)
        env["SOLDR_CACHE_DIR"] = str(root)
        started = time.monotonic()
        proc = subprocess.run(
            [str(soldr), "gc", "maintain", "--root", str(root), "--json"],
            capture_output=True,
            text=True,
            env=env,
            timeout=TRIAL_TIMEOUT_SECONDS,
            check=False,
        )
        status = parse_status(proc.stdout)
        return {
            "copy_seconds": round(copy_seconds, 1),
            "maintain_seconds": round(time.monotonic() - started, 1),
            "exit_code": proc.returncode,
            "report": (status or {}).get("zccache"),
            "deferred_reason": (status or {}).get("deferred_reason"),
            "stderr_tail": proc.stderr[-800:],
        }
    except (OSError, subprocess.SubprocessError) as error:
        return {"skipped": f"trial failed: {error}"}
    finally:
        shutil.rmtree(root, ignore_errors=True)


def gib(value: int) -> str:
    return f"{value / GIB:.2f} GiB"


def render(walk: Walk | None, verdict: str, cap_bytes: int, trial: dict | None) -> str:
    lines = [
        "## Tier-2 zccache working set (soldr#3120)",
        "",
        f"Trim cap: {gib(cap_bytes)}",
        "",
    ]
    if walk is not None:
        lines += [
            f"Working set: **{gib(walk.touched_bytes)}** of {gib(walk.total_bytes)} "
            f"({walk.touched_files} of {walk.total_files} files touched since the restore)",
            "",
            f"Verdict: {verdict}",
            "",
            "| version dir | bytes | touched |",
            "| --- | ---: | ---: |",
        ]
        for name, (total, touched) in sorted(walk.by_version.items()):
            lines.append(f"| `{name}` | {gib(total)} | {gib(touched)} |")
        lines.append("")
    else:
        lines += [f"Working set: {verdict}", ""]
    if trial is not None:
        if "skipped" in trial:
            lines.append(f"Trial trim: skipped ({trial['skipped']})")
        else:
            report = trial.get("report") or {}
            lines += [
                f"Trial trim on a copy (copy {trial['copy_seconds']} s, "
                f"maintain {trial['maintain_seconds']} s, exit {trial['exit_code']}):",
                "",
                "| field | value |",
                "| --- | ---: |",
            ]
            for field in (
                "pressure",
                "budget_bytes",
                "usage_before_bytes",
                "usage_after_bytes",
                "bytes_reclaimed",
                "artifacts_removed",
                "expired_artifacts_removed",
            ):
                lines.append(f"| {field} | {report.get(field, 'n/a')} |")
            if trial.get("deferred_reason"):
                lines.append(f"\ndeferred: {trial['deferred_reason']}")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--store", type=pathlib.Path, required=True)
    parser.add_argument("--since-file", type=pathlib.Path, required=True)
    parser.add_argument("--soldr", type=pathlib.Path, help="enables the trial trim")
    parser.add_argument("--scratch-root", type=pathlib.Path)
    parser.add_argument("--cap-bytes", type=int)
    parser.add_argument("--reserve-bytes", type=int, default=DEFAULT_RESERVE_BYTES)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parents[2]
        / "ci"
        / "cache-ownership.json",
    )
    args = parser.parse_args(argv)

    cap_bytes = args.cap_bytes or default_cap_bytes(args.manifest, args.reserve_bytes)
    if not args.store.is_dir():
        print(f"no object store at {args.store} -- nothing to measure")
        return 0
    since = read_since(args.since_file)
    walk = None
    if since is None:
        verdict = f"no restore marker at {args.since_file}: working set not measured"
    else:
        walk = walk_store(args.store, since)
        verdict = working_set_verdict(walk, cap_bytes)
    trial = None
    if args.soldr is not None and args.scratch_root is not None:
        store_bytes = (
            walk.total_bytes
            if walk is not None
            else walk_store(args.store, float("inf")).total_bytes
        )
        trial = trial_trim(
            args.store, store_bytes, args.soldr, cap_bytes, args.scratch_root
        )
        if trial.get("stderr_tail") and trial.get("exit_code"):
            print(trial["stderr_tail"], file=sys.stderr)

    text = render(walk, verdict, cap_bytes, trial)
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
