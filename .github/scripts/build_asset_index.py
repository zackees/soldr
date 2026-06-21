#!/usr/bin/env python3
"""Generate ``asset-index.json`` for the ``manifest`` branch.

The runtime resolver in ``crates/soldr-cli/src/fetch/manifest_lookup.rs``
consults a vendored, sha-bearing asset index hosted on soldr's own
``manifest`` branch:

    https://raw.githubusercontent.com/zackees/soldr/manifest/asset-index.json

The deployed parser (see ``ManifestIndex`` in that file) expects a
deliberately FLAT shape — one row per ``(owner, repo, tag, asset)``::

    {
      "entries": [
        {
          "owner":  "zackees",
          "repo":   "zccache",
          "tag":    "1.12.9",
          "asset":  "zccache-v1.12.9-x86_64-pc-windows-msvc.zip",
          "url":    "https://github.com/.../...zip",
          "sha256": "<64-char lowercase hex>"
        },
        ...
      ]
    }

This script walks a local checkout of the ``manifest`` branch and emits
that JSON. Two data sources contribute entries:

1. **Vendored assets under ``deps/``.** Sha256 is computed directly from
   the file on disk (matches ``crates/soldr-cli/src/fetch/trust.rs::sha256_of``
   exactly: raw bytes through SHA-256, lowercase hex). The companion
   per-tool ``deps/<area>/manifest.json`` already carries the owner / repo
   / tag / sha for each vendored entry; we cross-check that any sha
   pre-recorded there matches the on-disk file and prefer the on-disk
   hash on conflict (the file is the source of truth).

2. **GitHub-released assets whose release ships a ``SHA256SUMS`` asset.**
   Where the existing per-tool manifest (``zccache/manifest.json``, …)
   lists ``SHA256SUMS`` in its raw ``assets`` map, we fetch that single
   small file over HTTP and parse it to attribute a sha256 to every
   sibling asset. Releases without a ``SHA256SUMS`` file are skipped
   silently — there is no other way to attribute a hash without
   downloading the (sometimes multi-GB) archives, which is not nightly
   workload. The resolver gracefully degrades to a cache miss + live
   GitHub Releases API fallback for those.

Determinism: entries are sorted ascending by ``(owner, repo, tag, asset)``
so the diff of ``asset-index.json`` between refreshes is reviewable.

Author: nightly-refreshed in lockstep with ``build_manifest.py`` from
``.github/workflows/refresh-manifest.yml``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Iterable

# Schema version of the emitted ``asset-index.json`` envelope. Bump only
# when the SHAPE of the file changes — the consumer-side dispatch is
# discriminated by the presence of the ``entries`` array (v5/flat) vs.
# ``schema_version`` + ``tools`` keys (v6/nested). The deployed parser
# is the v5 flat shape; this constant exists so a JSON consumer can
# eyeball the schema version without inspecting the structure.
ASSET_INDEX_SCHEMA_VERSION = 5

# Asset filename that, when present in a release's ``assets`` map,
# carries one sha256-per-line for every sibling asset. Every zackees-
# published release (zccache, crgx, …) ships this; cargo-chef and
# cargo-zigbuild / cargo-xwin do NOT, so their releases contribute no
# entries today.
SHA256SUMS_ASSET_NAME = "SHA256SUMS"

# Per-asset filenames inside the SHA256SUMS file itself that we never
# want to index — they're the self-referential checksum file and the
# installer shell scripts, neither of which the runtime resolver pulls
# through the GitHub Releases path.
SHA256SUMS_SKIP_LINES = {"SHA256SUMS", "install.sh", "install.ps1"}


def sha256_of_file(path: Path) -> str:
    """SHA-256 of ``path``'s bytes, lowercase hex.

    Matches ``crates/soldr-cli/src/fetch/trust.rs::sha256_of`` exactly:
    raw bytes through SHA-256, no header, no length prefix, hex-encoded
    in lowercase. The runtime resolver compares this byte-for-byte.
    """
    hasher = hashlib.sha256()
    with path.open("rb") as fh:
        # 1 MiB chunks — the deps tree currently tops out at ~50 MB
        # (apple SDK) so this is one allocation per few iterations and
        # holds no full file in memory.
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def http_get_text(url: str, *, timeout: float = 30.0) -> str | None:
    """Fetch ``url`` and return its body as UTF-8 text.

    Returns ``None`` on any failure (404, network error, decoding
    error). Callers treat that as "no SHA256SUMS for this release" and
    skip silently — the consumer-side resolver degrades to the live
    GitHub Releases API for any entry not present in the index.
    """
    req = urllib.request.Request(url)
    req.add_header("User-Agent", "soldr-asset-index-builder")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if resp.status != 200:
                return None
            data = resp.read()
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return None
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return None


def parse_sha256sums(text: str) -> dict[str, str]:
    """Parse a ``SHA256SUMS`` body into ``{asset_filename: sha256_hex}``.

    Format (per ``sha256sum -b``)::

        <64-char hex>  <filename>
        <64-char hex>  ./<filename>

    The leading ``./`` is stripped. Comments (``#`` prefix) and blank
    lines are ignored. Unrecognized lines are silently dropped — a
    malformed checksum file must not poison the entire refresh.
    """
    out: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        # The format is `<hex><whitespace><name>` — split on the first
        # whitespace run so filenames containing spaces survive (none
        # of the tracked tools do today, but it's free to support).
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        sha, name = parts
        if len(sha) != 64 or not all(c in "0123456789abcdef" for c in sha.lower()):
            continue
        # Strip the optional ``./`` prefix that GNU coreutils emits.
        name = name.removeprefix("./").strip()
        if not name or name in SHA256SUMS_SKIP_LINES:
            continue
        # Drop debug / symbol packages — they aren't the canonical
        # resolver target (matches the same exclusion that
        # build_manifest.py applies in derive_platform_key).
        lower = name.lower()
        if "-debug" in lower or ".debug" in lower or ".pdb" in lower or "-sym" in lower:
            continue
        out[name] = sha.lower()
    return out


def iter_deps_files(manifest_root: Path) -> Iterable[Path]:
    """Yield every regular file under ``<manifest_root>/deps/``.

    The vendored files are the priority of this index (every other
    source — GitHub releases — is a "best effort, sha-if-available"
    add-on). The per-area ``manifest.json`` companion files are emitted
    too: the consumer can still ``GET`` them and the sha guards against
    a midnight content swap on the CDN.
    """
    deps_dir = manifest_root / "deps"
    if not deps_dir.is_dir():
        return
    for path in sorted(deps_dir.rglob("*")):
        if path.is_file():
            yield path


def _entry_sort_key(entry: dict[str, Any]) -> tuple[str, str, str, str]:
    """Stable sort key — the manifest is regenerated every refresh and
    we want byte-identical output across runs whenever the inputs match.
    """
    return (
        entry.get("owner", ""),
        entry.get("repo", ""),
        entry.get("tag", ""),
        entry.get("asset", ""),
    )


def _raw_url_for_deps(repo_owner: str, repo_name: str, rel_posix: str) -> str:
    """Build the LFS-aware CDN URL the vendored asset is served from.

    Uses ``media.githubusercontent.com/media/`` rather than
    ``raw.githubusercontent.com``: the ``/media/`` endpoint follows
    Git-LFS pointer files to the actual binary blob (and falls back
    transparently to the raw content for non-LFS files). This matches
    the URL pattern used by ``zackees/clang-tool-chain-bins`` and lets
    the soldr ``manifest`` branch migrate to LFS without breaking the
    resolver — same URL form works for both pre- and post-LFS state.
    """
    return f"https://media.githubusercontent.com/media/{repo_owner}/{repo_name}/manifest/{rel_posix}"


def collect_deps_entries(
    manifest_root: Path,
    repo_owner: str,
    repo_name: str,
) -> list[dict[str, Any]]:
    """Walk ``<manifest_root>/deps/`` and emit one entry per file.

    For each file, sha256 is computed from on-disk bytes. The companion
    per-area ``manifest.json`` is consulted to pick up the canonical
    ``(owner, repo, tag, asset)`` tuple: when a vendored file is named
    in the manifest's ``assets`` map (e.g. ``sdk.tar.zstd``), the
    ``owner`` / ``repo`` / ``tag`` from that manifest entry are
    attributed to the row. When a vendored file is NOT attributed (the
    per-area ``manifest.json`` itself, for example), it gets a
    self-attributed entry under ``(<repo_owner>, <repo_name>, "manifest", <rel-path>)``
    so a future air-gapped mirror can still sha-verify it.
    """
    entries: list[dict[str, Any]] = []

    # Build a (owner, repo, tag, asset_filename) → entry index from
    # every per-area manifest under deps/. We use this to attribute
    # ownership of vendored files that are explicitly enumerated in a
    # per-area manifest's ``assets`` map.
    attribution: dict[str, tuple[str, str, str]] = {}
    deps_dir = manifest_root / "deps"
    if deps_dir.is_dir():
        for area_manifest in deps_dir.rglob("manifest.json"):
            try:
                payload = json.loads(area_manifest.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if not isinstance(payload, list):
                continue
            area_rel = area_manifest.parent.relative_to(manifest_root).as_posix()
            for release in payload:
                if not isinstance(release, dict):
                    continue
                owner = release.get("owner")
                repo = release.get("repo")
                tag = release.get("tag")
                if not (isinstance(owner, str) and isinstance(repo, str)
                        and isinstance(tag, str)):
                    continue
                assets = release.get("assets")
                if not isinstance(assets, dict):
                    continue
                for asset_name in assets.keys():
                    if not isinstance(asset_name, str):
                        continue
                    # Key by repo-root-relative path so two areas can
                    # both ship a ``manifest.json`` without colliding.
                    rel_key = f"{area_rel}/{asset_name}"
                    attribution[rel_key] = (owner, repo, tag)

    for path in iter_deps_files(manifest_root):
        rel = path.relative_to(manifest_root).as_posix()
        sha = sha256_of_file(path)
        if rel in attribution:
            owner, repo, tag = attribution[rel]
            asset = path.name
        else:
            # Self-attributed: the manifest branch itself owns this
            # file. ``tag="manifest"`` distinguishes the entry from
            # any third-party release.
            owner = repo_owner
            repo = repo_name
            tag = "manifest"
            asset = rel
        entries.append({
            "owner": owner,
            "repo": repo,
            "tag": tag,
            "asset": asset,
            "url": _raw_url_for_deps(repo_owner, repo_name, rel),
            "sha256": sha,
        })
    return entries


def collect_release_entries_for_tool(
    tool_manifest_path: Path,
    *,
    offline: bool = False,
) -> list[dict[str, Any]]:
    """Read one per-tool ``manifest.json`` (flat array of releases) and
    emit one entry per asset for which a sha256 can be attributed.

    Today the only attributable releases are those whose ``assets``
    map contains a ``SHA256SUMS`` file; we fetch that single file and
    parse it for the per-asset hashes. Releases without a SHA256SUMS
    contribute zero entries — the resolver falls through to the live
    GitHub Releases API at runtime for those.

    ``offline=True`` skips the SHA256SUMS HTTP fetch entirely; used by
    the unit test so the build doesn't depend on github.com
    reachability.
    """
    entries: list[dict[str, Any]] = []
    try:
        payload = json.loads(tool_manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return entries
    if not isinstance(payload, list):
        return entries
    for release in payload:
        if not isinstance(release, dict):
            continue
        owner = release.get("owner")
        repo = release.get("repo")
        tag = release.get("tag")
        assets = release.get("assets")
        if not (isinstance(owner, str) and isinstance(repo, str)
                and isinstance(tag, str) and isinstance(assets, dict)):
            continue
        sums_entry = assets.get(SHA256SUMS_ASSET_NAME)
        if not isinstance(sums_entry, dict):
            continue
        sums_url = sums_entry.get("url")
        if not isinstance(sums_url, str) or offline:
            continue
        body = http_get_text(sums_url)
        if body is None:
            print(
                f"  no SHA256SUMS available for {owner}/{repo}@{tag} "
                f"(url={sums_url})",
                file=sys.stderr,
            )
            continue
        sums = parse_sha256sums(body)
        for asset_name, sha in sums.items():
            asset_entry = assets.get(asset_name)
            if not isinstance(asset_entry, dict):
                # The SHA256SUMS named an asset that isn't in the
                # per-tool manifest (e.g. installer scripts). Skip —
                # the resolver only ever requests assets it has a URL
                # for.
                continue
            url = asset_entry.get("url")
            if not isinstance(url, str):
                continue
            entries.append({
                "owner": owner,
                "repo": repo,
                "tag": tag,
                "asset": asset_name,
                "url": url,
                "sha256": sha,
            })
    return entries


def build_asset_index(
    manifest_root: Path,
    *,
    repo_owner: str = "zackees",
    repo_name: str = "soldr",
    offline: bool = False,
) -> dict[str, Any]:
    """Walk ``manifest_root`` and produce the full asset index payload.

    ``offline`` short-circuits the SHA256SUMS HTTP fetch — the deps/
    branch still contributes every vendored entry, but no
    GitHub-Releases entries are produced. Used by the unit test.
    """
    entries: list[dict[str, Any]] = []
    entries.extend(collect_deps_entries(manifest_root, repo_owner, repo_name))

    # Per-tool manifests at the repo root (zccache/, crgx/, etc.).
    for tool_manifest in sorted(manifest_root.glob("*/manifest.json")):
        # Skip per-area deps manifests — they're handled by
        # collect_deps_entries and shouldn't be re-attributed as
        # GitHub-released assets.
        if tool_manifest.parent.name == "deps":
            continue
        if tool_manifest.parent.is_relative_to(manifest_root / "deps"):
            continue
        entries.extend(
            collect_release_entries_for_tool(tool_manifest, offline=offline)
        )

    entries.sort(key=_entry_sort_key)
    # De-duplicate. If two paths attributed the same (owner, repo, tag,
    # asset) — e.g. a vendored file also named in a per-tool manifest's
    # assets — we keep the first (deps-derived) entry, which carries
    # the on-disk sha. The second source's sha is a downloaded archive,
    # and if it disagrees we want the on-disk value to win.
    seen: set[tuple[str, str, str, str]] = set()
    deduped: list[dict[str, Any]] = []
    for entry in entries:
        key = _entry_sort_key(entry)
        if key in seen:
            continue
        seen.add(key)
        deduped.append(entry)

    return {
        "schema_version": ASSET_INDEX_SCHEMA_VERSION,
        "entries": deduped,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest-checkout",
        type=Path,
        required=True,
        help=(
            "Path to a local checkout of the soldr `manifest` branch "
            "(the orphan branch that hosts per-tool manifest.json files "
            "and the vendored `deps/` tree)."
        ),
    )
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="Path to write the generated asset-index.json to.",
    )
    parser.add_argument(
        "--repo-owner",
        default="zackees",
        help="GitHub owner that hosts the manifest branch (default: zackees).",
    )
    parser.add_argument(
        "--repo-name",
        default="soldr",
        help="GitHub repo name that hosts the manifest branch (default: soldr).",
    )
    parser.add_argument(
        "--offline",
        action="store_true",
        help=(
            "Skip the SHA256SUMS HTTP fetch step. Only the vendored "
            "deps/ entries are emitted. Use for air-gapped runs and "
            "for the script's own unit test."
        ),
    )
    args = parser.parse_args(argv)

    manifest_root = args.manifest_checkout.resolve()
    if not manifest_root.is_dir():
        print(
            f"error: --manifest-checkout {manifest_root} is not a directory",
            file=sys.stderr,
        )
        return 2

    index = build_asset_index(
        manifest_root,
        repo_owner=args.repo_owner,
        repo_name=args.repo_name,
        offline=args.offline,
    )

    output_path = args.output.resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    # Trailing newline matches build_manifest.py — POSIX text-file
    # convention + cleaner `git diff`.
    payload = json.dumps(index, indent=2, sort_keys=False) + "\n"
    output_path.write_text(payload, encoding="utf-8")

    print(
        f"asset-index: wrote {output_path} "
        f"({len(index['entries'])} entries, "
        f"schema_version={index['schema_version']})",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
