#!/usr/bin/env python3
"""Fail CI when the repository's GitHub Actions cache exceeds its budget.

soldr#3047, Phase B of soldr#3039. GitHub evicts the oldest, least-recently-used
entries once a repository's Actions cache crosses the 10 GB it documents as the
per-repository ceiling, and it does so silently: no job goes red, the run whose
warm cache disappeared underneath it just gets slower. The manifest's
`budget.total_max_bytes` is 9 GiB -- the sum of the family allocations -- and
`budget.fail_total_bytes` is 9.5 GiB (10,200,547,328), half a GiB of headroom
so a family briefly over its own allocation does not fail the whole gate
before the next `--prune` sweep catches up. Both numbers live in
`ci/cache-ownership.json`; this script reads them and hard-codes neither.

`zackees/soldr`'s cache had grown to 44.23 GiB across 143 entries by
2026-09-01 (`tests/fixtures/actions-cache/listing-2026-09-01.json`, the RED
acceptance fixture for this guard) -- more than four times the ceiling -- and
nothing in CI could tell a reviewer that from a passing lane. The only signal
was slower builds nobody could attribute to a cause.

## What is checked

`ci/cache-ownership.json` carries a `budget` object: a `total_max_bytes`
allocation, a `fail_total_bytes` hard ceiling, and a `families` map. Each
family declares the `key_prefixes` it owns, a `max_bytes` allocation, and a
rationale. Every live cache entry is assigned to the family whose
`key_prefixes` entry is the LONGEST match on that entry's key -- longest,
so a family that owns a specific sub-namespace is not shadowed by a sibling
family's shorter, more general prefix.

Three things fail the gate:

* **An unregistered key.** An entry matching no family's `key_prefixes` names
  a producer nobody declared. The manifest is authoritative, not descriptive:
  a new cache-writing step must register its prefix in the same PR that adds
  it, or this guard has no way to tell a reviewed addition from cache-key
  drift (a resurrected `v0-rust-cross-build-*` key, say).
* **A family over its allocation.** Bytes used under one family's prefixes
  exceed that family's declared `max_bytes`.
* **The total over `fail_total_bytes`.** Even when every family individually
  fits, the sum across all of them may not exceed the hard ceiling.

## Network policy

SKIP, DO NOT FAIL, when the live source is unavailable: `gh` missing, a
non-zero exit, or a response that does not parse as JSON. This mirrors
`check_dylint_driver_assets.py` -- a guard that goes red because a fork PR has
no `gh` token or GitHub Actions cache API rate limit teaches people to ignore
it. `--from-json` bypasses the network entirely (used for the acceptance
fixture) and its own read failures are real failures, not skips, because the
caller chose that exact path.

## Pruning

`--prune` lists (never deletes without `--apply`) three classes of
reclaimable entry: keys under a `RETIRED_PREFIXES` namespace whose producer no
longer runs, entries on a ref other than `refs/heads/main` (a PR's caches are
never restored by another PR), and `v0-rust-*` entries on `refs/heads/main`
that have been superseded by a newer generation of the same shared-key
lineage. Pruning needs cache ids to call `gh cache delete`, so it requires the
live source; `--from-json` fixtures carry no ids.

Usage:
    python .github/scripts/check_cache_budget.py [options]
Options:
    --manifest PATH   budget manifest (default: ci/cache-ownership.json)
    --from-json PATH  read a cache listing from this file instead of `gh`
    --repo OWNER/NAME repository to query (default: zackees/soldr)
    --prune           report deletion candidates (dry run)
    --apply           with --prune, actually delete the candidates
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys
from dataclasses import dataclass

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"
DEFAULT_REPO = "zackees/soldr"

GIB = 1024**3

# Retired Swatinem/rust-cache shared-key namespaces: nothing writes these any
# more, so any entry still carrying one of these prefixes is pure waste.
RETIRED_PREFIXES: tuple[str, ...] = (
    # `cross-build-<target>-v7` in _ci-cross-build-linux.yml. Retired by
    # soldr#3047: soldr#2996 had already made Tier 1 `soldr cook` the surviving
    # implementation of the dependency-graph cache on exactly this lane, so the
    # rust-cache step beside it was a second implementation of one tier.
    "v0-rust-cross-build-",
    # `ws-dev-<target>` in _build-and-test.yml's ci-test plan. Retired by
    # soldr#3047 (2.15 GiB across 5 entries at a 0% hit rate).
    "v0-rust-ws-dev-",
    # `pep517-<name>-ci-release-v2` for the PEP 517 wheel build, in both
    # ci.yml and _ci-target-run.yml. Retired by soldr#3047.
    "v0-rust-pep517-",
    # ci.yml's bootstrap-driver shared-key was renamed from
    # `bootstrap-soldr-linux-gnu-ci-bootstrap-v2` to
    # `bootstrap-soldr-linux-gnu-dev-v1` (see the `shared-key:` step in
    # ci.yml). The `-v2` generation is orphaned: nothing writes it any more.
    "v0-rust-bootstrap-soldr-linux-gnu-ci-bootstrap-",
    # The same rename on the sibling *binary* cache, which is `actions/cache`
    # rather than rust-cache. ci.yml now writes
    # `bootstrap-soldr-blessed-linux-gnu-dev-v1-<sha>` and
    # `_ci-cross-build-linux.yml` writes
    # `bootstrap-soldr-blessed-linux-gnu-<sha>`; a `<sha>` is hex, so neither
    # can ever produce the `-ci-bootstrap-` segment. It was 117 MB of the
    # bootstrap-driver-binary family in the 2026-09-01 listing -- a family
    # whose key embeds `github.sha` and therefore cannot be main-gated, so
    # pruning dead generations is the only lever it has.
    "bootstrap-soldr-blessed-linux-gnu-ci-bootstrap-",
    # `bootstrap-e2e-<target>` in _bootstrap-e2e.yml, retired by soldr#3121
    # to fund cook on the MSVC/aarch64-gnu cross lanes.
    "v0-rust-bootstrap-e2e-",
)


@dataclass(frozen=True)
class CacheEntry:
    """One `gh cache list` row, trimmed to the fields this guard reads."""

    key: str
    ref: str
    size_bytes: int
    id: str | None = None
    created_at: str | None = None


# ---------------------------------------------------------------------------
# Loading a listing
# ---------------------------------------------------------------------------


def normalize_entries(raw: list[object]) -> list[CacheEntry]:
    """Turn raw JSON rows (fixture or live) into `CacheEntry` objects.

    A malformed row (missing/mistyped key, ref or size) is dropped rather than
    raising -- one bad row from `gh` should not take the whole budget check
    down with it.
    """
    entries: list[CacheEntry] = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        key = item.get("key")
        ref = item.get("ref")
        size = item.get("sizeInBytes")
        if (
            not isinstance(key, str)
            or not isinstance(ref, str)
            or not isinstance(size, int)
        ):
            continue
        # `gh cache list --json id` emits the id as a JSON NUMBER, not a
        # string. Accepting only `str` here silently dropped every live id,
        # so `--prune --apply` reported "no cache id, cannot delete" for all
        # 21 candidates on 2026-09-04 and reclaimed nothing.
        entry_id = item.get("id")
        if isinstance(entry_id, bool) or not isinstance(entry_id, (int, str)):
            entry_id = None
        created_at = item.get("createdAt")
        entries.append(
            CacheEntry(
                key=key,
                ref=ref,
                size_bytes=size,
                id=str(entry_id) if entry_id is not None else None,
                created_at=created_at if isinstance(created_at, str) else None,
            )
        )
    return entries


def load_from_json(path: pathlib.Path) -> tuple[list[object], int | None]:
    """Read the `{"usage_bytes", "entries": [...]}` fixture shape."""
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError(f"{path} must contain a JSON object")
    entries = payload.get("entries")
    if not isinstance(entries, list):
        raise ValueError(f"{path} has no 'entries' array")
    usage = payload.get("usage_bytes")
    return entries, usage if isinstance(usage, int) else None


def run_gh(args: list[str]) -> str:
    """Run `gh <args>` and return stdout. Raises on any failure.

    Kept as its own function so tests can monkeypatch exactly this call to
    simulate a missing/broken `gh` without touching the network.
    """
    result = subprocess.run(["gh", *args], capture_output=True, text=True, check=True)
    return result.stdout


def fetch_live_entries(repo: str) -> list[object]:
    """`gh cache list`, paged high enough to see the whole repository cache.

    GitHub pages this API at 100 by default, so the explicit `--limit 1000`
    is required, not cosmetic -- a repository with more than 100 entries
    would otherwise silently see only the first page.
    """
    stdout = run_gh(
        [
            "cache",
            "list",
            "--repo",
            repo,
            "--limit",
            "1000",
            "--json",
            "id,key,ref,sizeInBytes,createdAt",
        ]
    )
    payload = json.loads(stdout)
    if not isinstance(payload, list):
        raise ValueError("gh cache list did not return a JSON array")
    return payload


def fetch_live_usage_bytes(repo: str) -> int | None:
    """`actions/cache/usage`'s reported total, or `None` if it cannot be read.

    Best-effort and separate from `fetch_live_entries`: a broken usage call
    does not invalidate a listing that came back fine, it just means the
    table prints without the API's own cross-check number.
    """
    try:
        stdout = run_gh(["api", f"repos/{repo}/actions/cache/usage"])
        payload = json.loads(stdout)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
        return None
    if isinstance(payload, dict):
        value = payload.get("active_caches_size_in_bytes")
        if isinstance(value, int):
            return value
    return None


# ---------------------------------------------------------------------------
# Family assignment
# ---------------------------------------------------------------------------


def family_for(key: str, families: dict[str, object]) -> str | None:
    """The family owning `key`: the LONGEST matching `key_prefixes` entry."""
    best_id: str | None = None
    best_len = -1
    for family_id, spec in families.items():
        if not isinstance(spec, dict):
            continue
        for prefix in spec.get("key_prefixes") or []:
            if (
                isinstance(prefix, str)
                and key.startswith(prefix)
                and len(prefix) > best_len
            ):
                best_len = len(prefix)
                best_id = family_id
    return best_id


def group_by_family(
    entries: list[CacheEntry], families: dict[str, object]
) -> tuple[dict[str, list[CacheEntry]], list[CacheEntry]]:
    """`(family_id -> its entries, entries matching no family)`."""
    grouped: dict[str, list[CacheEntry]] = {family_id: [] for family_id in families}
    unmatched: list[CacheEntry] = []
    for entry in entries:
        family_id = family_for(entry.key, families)
        if family_id is None:
            unmatched.append(entry)
        else:
            grouped[family_id].append(entry)
    return grouped, unmatched


# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------


def load_manifest(path: pathlib.Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def budget_problems(
    manifest_path: pathlib.Path, manifest: dict, entries: list[CacheEntry]
) -> list[str]:
    """Every budget failure for `entries` under `manifest['budget']`."""
    if not isinstance(manifest, dict):
        return [f"{manifest_path} must contain a JSON object"]
    budget = manifest.get("budget")
    if not isinstance(budget, dict):
        return [f"{manifest_path} has no object-valued 'budget'"]

    problems: list[str] = []

    total_max_bytes = budget.get("total_max_bytes")
    if not isinstance(total_max_bytes, int):
        problems.append(f"{manifest_path} budget.total_max_bytes must be an integer")

    fail_total_bytes = budget.get("fail_total_bytes")
    if not isinstance(fail_total_bytes, int):
        problems.append(f"{manifest_path} budget.fail_total_bytes must be an integer")

    families = budget.get("families")
    if not isinstance(families, dict) or not families:
        problems.append(f"{manifest_path} budget.families must be a non-empty object")
        return problems

    grouped, unmatched = group_by_family(entries, families)

    for entry in sorted(unmatched, key=lambda e: (e.key, e.ref)):
        problems.append(
            f"unregistered cache entry key={entry.key!r} ref={entry.ref!r}: an "
            "unregistered producer may not appear; register its key prefix "
            f"under a family in {manifest_path} budget.families in the same "
            "PR that adds the producer."
        )

    for family_id, spec in sorted(families.items()):
        if not isinstance(spec, dict):
            problems.append(f"family {family_id!r} is not an object")
            continue
        max_bytes = spec.get("max_bytes")
        if not isinstance(max_bytes, int):
            problems.append(f"family {family_id!r} has no integer 'max_bytes'")
            continue
        used = sum(e.size_bytes for e in grouped.get(family_id, []))
        if used > max_bytes:
            problems.append(
                f"family {family_id!r} uses {used / GIB:.2f} GiB, over its "
                f"{max_bytes / GIB:.2f} GiB budget"
            )

    if isinstance(fail_total_bytes, int):
        total_bytes = sum(e.size_bytes for e in entries)
        if total_bytes > fail_total_bytes:
            problems.append(
                f"total repository Actions-cache usage {total_bytes / GIB:.2f} "
                f"GiB exceeds fail_total_bytes {fail_total_bytes / GIB:.2f} GiB"
            )

    return problems


def check(manifest_path: pathlib.Path, entries: list[CacheEntry]) -> list[str]:
    """Every budget failure, as actionable lines. Empty means it holds."""
    try:
        manifest = load_manifest(manifest_path)
    except (OSError, json.JSONDecodeError) as error:
        return [f"cannot read {manifest_path}: {error}"]
    return budget_problems(manifest_path, manifest, entries)


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def build_table(
    budget: dict, entries: list[CacheEntry], usage_bytes: int | None
) -> str:
    """The pass-or-fail table printed on every run."""
    families = budget.get("families")
    if not isinstance(families, dict):
        families = {}
    grouped, unmatched = group_by_family(entries, families)
    total_bytes = sum(e.size_bytes for e in entries)
    total_max_bytes = budget.get("total_max_bytes")

    rows = []
    for family_id, family_entries in grouped.items():
        spec = families.get(family_id)
        max_bytes = spec.get("max_bytes") if isinstance(spec, dict) else None
        used = sum(e.size_bytes for e in family_entries)
        rows.append((family_id, len(family_entries), used, max_bytes))
    rows.sort(key=lambda row: row[2], reverse=True)

    lines = [f"{'family':<42} {'count':>6} {'used GiB':>10} {'alloc GiB':>10} {'%':>7}"]
    for family_id, count, used, max_bytes in rows:
        alloc_gib = max_bytes / GIB if isinstance(max_bytes, int) else float("nan")
        pct = (
            (used / max_bytes * 100)
            if isinstance(max_bytes, int) and max_bytes
            else 0.0
        )
        lines.append(
            f"{family_id:<42} {count:>6} {used / GIB:>10.2f} {alloc_gib:>10.2f} {pct:>6.1f}%"
        )
    total_alloc_gib = (
        total_max_bytes / GIB if isinstance(total_max_bytes, int) else float("nan")
    )
    lines.append(
        f"{'TOTAL':<42} {len(entries):>6} {total_bytes / GIB:>10.2f} {total_alloc_gib:>10.2f}"
    )
    if unmatched:
        noun = "entry" if len(unmatched) == 1 else "entries"
        lines.append(f"  ({len(unmatched)} unregistered {noun} not shown above)")
    if usage_bytes is not None:
        lines.append(f"actions/cache/usage reports {usage_bytes / GIB:.2f} GiB active")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Pruning
# ---------------------------------------------------------------------------


# Key families whose trailing `-<segment>` is a per-run generation of one
# shared lineage: `v0-rust-*` ends in a rust-cache hash, `zccache-unit-*`
# ends in the GitHub run id (soldr#3041 keys it that way so every host-lane
# run saves a fresh generation and restores the newest by prefix). Only the
# newest generation of a lineage is ever restored, so the rest is dead
# weight -- 1.3 GiB per host-lane run on main (soldr#3102).
GENERATION_KEY_PREFIXES = ("v0-rust-", "zccache-unit-")


def strip_shared_key_hash(key: str) -> str:
    """Drop a generation key's trailing `-<hash|run id>` segment."""
    if not key.startswith(GENERATION_KEY_PREFIXES):
        return key
    index = key.rfind("-")
    return key[:index] if index != -1 else key


def prune_candidates(entries: list[CacheEntry]) -> list[CacheEntry]:
    """Entries safe to delete: retired prefix, non-main ref, or superseded.

    Each entry is classified into exactly the first rule it matches, so the
    reclaimed-bytes total never double-counts one entry.
    """
    candidates: list[CacheEntry] = []
    remaining: list[CacheEntry] = []
    for entry in entries:
        if entry.key.startswith(RETIRED_PREFIXES):
            candidates.append(entry)
        else:
            remaining.append(entry)

    on_main: list[CacheEntry] = []
    for entry in remaining:
        if entry.ref != "refs/heads/main":
            candidates.append(entry)
        else:
            on_main.append(entry)

    groups: dict[str, list[CacheEntry]] = {}
    for entry in on_main:
        if not entry.key.startswith(GENERATION_KEY_PREFIXES):
            continue
        groups.setdefault(strip_shared_key_hash(entry.key), []).append(entry)

    for group_entries in groups.values():
        if len(group_entries) <= 1:
            continue
        newest = max(group_entries, key=lambda e: e.created_at or "")
        for entry in group_entries:
            if entry is not newest:
                candidates.append(entry)

    return candidates


def apply_prune(candidates: list[CacheEntry], repo: str) -> list[str]:
    """Delete every candidate with a cache id. Returns failure lines."""
    failures: list[str] = []
    for entry in candidates:
        if entry.id is None:
            failures.append(f"{entry.key}: no cache id, cannot delete")
            continue
        try:
            run_gh(["cache", "delete", entry.id, "--repo", repo])
        except (OSError, subprocess.CalledProcessError) as error:
            failures.append(f"{entry.key} ({entry.id}): {error}")
    return failures


# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=pathlib.Path,
        default=MANIFEST,
        help="budget manifest (default: ci/cache-ownership.json)",
    )
    parser.add_argument(
        "--from-json",
        type=pathlib.Path,
        default=None,
        help="read a cache listing from this file instead of calling gh",
    )
    parser.add_argument(
        "--repo",
        default=DEFAULT_REPO,
        help=f"repository to query (default: {DEFAULT_REPO})",
    )
    parser.add_argument(
        "--prune",
        action="store_true",
        help="report deletion candidates (dry run unless --apply is also given)",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="with --prune, actually delete the candidates",
    )
    args = parser.parse_args(argv)

    if args.prune and args.from_json is not None:
        print(
            "error: --prune requires the live source; --from-json fixtures "
            "carry no cache ids to delete"
        )
        return 1

    usage_bytes: int | None
    if args.from_json is not None:
        try:
            raw_entries, usage_bytes = load_from_json(args.from_json)
        except (OSError, json.JSONDecodeError, ValueError) as error:
            print(f"error: cannot read {args.from_json}: {error}")
            return 1
        entries = normalize_entries(raw_entries)
    else:
        try:
            raw_entries = fetch_live_entries(args.repo)
        except (
            OSError,
            subprocess.CalledProcessError,
            json.JSONDecodeError,
            ValueError,
        ) as error:
            print(f"check_cache_budget: skipped ({error})")
            return 0
        entries = normalize_entries(raw_entries)
        usage_bytes = fetch_live_usage_bytes(args.repo)

    try:
        manifest = load_manifest(args.manifest)
    except (OSError, json.JSONDecodeError) as error:
        print(f"error: cannot read {args.manifest}: {error}")
        return 1

    problems = budget_problems(args.manifest, manifest, entries)

    budget = manifest.get("budget") if isinstance(manifest, dict) else None
    if isinstance(budget, dict) and isinstance(budget.get("families"), dict):
        print(build_table(budget, entries, usage_bytes))
        print()

    if args.prune:
        candidates = prune_candidates(entries)
        reclaimed = sum(e.size_bytes for e in candidates)
        print(
            f"prune: {len(candidates)} candidate(s), {reclaimed / GIB:.2f} GiB reclaimable"
        )
        for entry in candidates:
            print(f"  {entry.key} ({entry.ref}) {entry.size_bytes / GIB:.3f} GiB")
        if args.apply:
            failures = apply_prune(candidates, args.repo)
            for failure in failures:
                print(f"  failed to delete {failure}")
        print()

    if problems:
        print("error: repository Actions-cache budget exceeded (soldr#3047):")
        for problem in problems:
            print(f"  {problem}")
        return 1

    print("check_cache_budget: repository Actions-cache usage is within budget.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
