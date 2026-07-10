#!/usr/bin/env python3
"""Assert that a thin-v2 ``manifest.v2.json`` is well-formed and on-disk.

This script is the second half of the thin-v2 verifier (issue #237). The
companion ``assert_thin_noop.py`` inspects cargo stdout to confirm Cargo's
fresh/dirty decision. That tells us cargo was correct, but it does not
prove the Phase 1 contract: that soldr-cli emitted a ``manifest.v2.json``
next to the bundle that truthfully enumerates the files present and never
re-lists any of the artifact categories thin-v2 is supposed to drop
(``.rlib``, ``.rmeta``, incremental DB, build-script binaries, etc.).

If a future regression silently stops emitting the manifest, or starts
re-listing dropped categories, this script fails the gate.

Schema (mirrors ``ThinSliceManifest`` in
``crates/soldr-cli/src/main.rs``):

    {
      "schema_version": 2,
      "cache_profile": "thin-v2",
      "bundle_root": "<absolute path>",
      "generated_at_unix_seconds": 1700000000,
      "files": [
        { "path": "rel/forward/slashed.json", "size_bytes": 12 },
        ...
      ]
    }

Exit codes:

- ``0``: manifest is valid and matches the bundle.
- ``1``: hard validation failure (missing manifest, drift, dropped-category
  hit, bad schema, etc.). Human-readable reason printed to stderr.
- ``2``: usage / I/O error before validation could start.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

# Patterns that must NEVER appear in a thin-v2 manifest. These are the
# artifact classes the slice is explicitly supposed to drop; cargo
# repopulates them through zccache on demand. Anything matching here in the
# manifest means the prune policy regressed.
#
# Patterns are evaluated against the manifest's forward-slashed relative
# path. We use simple regex over the full string rather than fnmatch so the
# directory boundaries are explicit.
_DROPPED_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
    ("incremental directory", re.compile(r"(^|/)incremental/")),
    ("rlib output", re.compile(r"\.rlib$")),
    ("rmeta output", re.compile(r"\.rmeta$")),
    ("split debug-info (.dwo)", re.compile(r"\.dwo$")),
    ("Windows pdb", re.compile(r"\.pdb$")),
    ("macOS dSYM bundle", re.compile(r"\.dSYM(/|$)")),
    ("build-script binary (Unix)", re.compile(r"(^|/)build-script-build$")),
    ("build-script binary (Windows)", re.compile(r"(^|/)build-script-build\.exe$")),
    (
        "fingerprint diagnostic JSON",
        re.compile(r"(^|/)\.fingerprint/[^/]+/[^/]+\.json$"),
    ),
]


def _format_drop_hits(hits: list[tuple[str, str]]) -> str:
    """Render dropped-pattern hits as a multi-line bulleted string."""
    lines = []
    for pattern_label, path in hits:
        lines.append(f"    - {path}  (matches {pattern_label})")
    return "\n".join(lines)


def _to_local_path(bundle_dir: Path, rel_posix: str) -> Path:
    """Resolve a forward-slashed manifest path to the OS-native bundle path."""
    parts = [p for p in rel_posix.split("/") if p not in ("", ".")]
    return bundle_dir.joinpath(*parts) if parts else bundle_dir


def _walk_bundle_files(bundle_dir: Path) -> list[str]:
    """Return forward-slashed relative paths of all files under ``bundle_dir``.

    The manifest itself (``manifest.v2.json``) is excluded so a strict-mode
    check does not trivially fail on the file we just read. Mirrors
    ``walk_bundle_files`` in ``crates/soldr-cli/src/main.rs`` which also
    drops the manifest from its own listing.
    """
    out: list[str] = []
    for path in bundle_dir.rglob("*"):
        if not path.is_file():
            continue
        rel = path.relative_to(bundle_dir).as_posix()
        if rel == "manifest.v2.json":
            continue
        out.append(rel)
    return out


def _validate_schema(manifest: object) -> list[str]:
    """Return a list of schema errors; empty list means OK."""
    errors: list[str] = []
    if not isinstance(manifest, dict):
        return [f"manifest root must be a JSON object, got {type(manifest).__name__}"]

    required: dict[str, type | tuple[type, ...]] = {
        "schema_version": int,
        "cache_profile": str,
        "bundle_root": str,
        "generated_at_unix_seconds": int,
        "files": list,
    }
    for key, expected_type in required.items():
        if key not in manifest:
            errors.append(f"missing required key: {key!r}")
            continue
        if not isinstance(manifest[key], expected_type):
            errors.append(
                f"key {key!r} has wrong type: expected "
                f"{getattr(expected_type, '__name__', expected_type)}, got "
                f"{type(manifest[key]).__name__}"
            )

    if "schema_version" in manifest and manifest.get("schema_version") != 2:
        errors.append(
            "schema_version must be 2 for thin-v2 manifests; "
            f"got {manifest.get('schema_version')!r}"
        )

    files = manifest.get("files")
    if isinstance(files, list):
        for idx, entry in enumerate(files):
            if not isinstance(entry, dict):
                errors.append(f"files[{idx}] is not an object")
                continue
            if "path" not in entry or not isinstance(entry["path"], str):
                errors.append(f"files[{idx}] missing string 'path'")
                continue
            # size_bytes is optional / may be null per the Rust schema.
            if "size_bytes" in entry and entry["size_bytes"] is not None:
                if not isinstance(entry["size_bytes"], int):
                    errors.append(
                        f"files[{idx}] size_bytes must be int or null, "
                        f"got {type(entry['size_bytes']).__name__}"
                    )

    return errors


def assert_manifest(
    manifest_path: Path,
    bundle_dir: Path,
    *,
    strict: bool = False,
) -> list[str]:
    """Validate the manifest. Returns a list of errors (empty == OK)."""
    errors: list[str] = []

    if not manifest_path.is_file():
        return [f"manifest file does not exist: {manifest_path}"]
    if not bundle_dir.is_dir():
        return [f"bundle directory does not exist: {bundle_dir}"]

    try:
        raw = manifest_path.read_text(encoding="utf-8")
    except OSError as exc:
        return [f"could not read manifest {manifest_path}: {exc}"]

    try:
        manifest = json.loads(raw)
    except json.JSONDecodeError as exc:
        return [f"manifest is not valid JSON: {exc}"]

    schema_errors = _validate_schema(manifest)
    errors.extend(schema_errors)
    # If schema is broken at the top level, downstream checks would crash on
    # the wrong shape. Stop early.
    if schema_errors:
        return errors

    files = manifest["files"]
    manifest_rel_paths: list[str] = [entry["path"] for entry in files]

    # 1. No dropped-category paths in the manifest. This is the strict
    #    invariant: thin-v2 must NEVER carry these classes.
    drop_hits: list[tuple[str, str]] = []
    for rel in manifest_rel_paths:
        for label, pattern in _DROPPED_PATTERNS:
            if pattern.search(rel):
                drop_hits.append((label, rel))
                break
    if drop_hits:
        errors.append(
            "manifest references files in dropped artifact classes "
            "(thin-v2 is supposed to prune these):\n" + _format_drop_hits(drop_hits)
        )

    # 2. Every entry in the manifest must exist on disk under bundle_dir.
    missing: list[str] = []
    for rel in manifest_rel_paths:
        local = _to_local_path(bundle_dir, rel)
        if not local.is_file():
            missing.append(rel)
    if missing:
        errors.append(
            f"manifest lists {len(missing)} file(s) that do not exist on disk; "
            f"first few: {missing[:5]}"
        )

    # 3. Strict mode: every file under bundle_dir must be in the manifest.
    if strict:
        on_disk = set(_walk_bundle_files(bundle_dir))
        in_manifest = set(manifest_rel_paths)
        orphans = sorted(on_disk - in_manifest)
        if orphans:
            errors.append(
                f"strict mode: {len(orphans)} file(s) on disk are not listed "
                f"in the manifest; first few: {orphans[:5]}"
            )

    return errors


def _build_argparser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Validate a thin-v2 manifest.v2.json against its bundle directory. "
            "Confirms schema, file presence, and that no dropped-category "
            "artifacts (rlib/rmeta/incremental/dwo/pdb/dSYM/build-script-build) "
            "are listed."
        ),
    )
    parser.add_argument(
        "manifest_path",
        type=Path,
        help="Path to the thin-v2 manifest.v2.json file.",
    )
    parser.add_argument(
        "bundle_dir",
        type=Path,
        help="Path to the bundle directory the manifest enumerates.",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help=(
            "Also fail if any file under bundle_dir is missing from the "
            "manifest (orphan detection). Off by default because some bundle "
            "writers may legitimately leave scratch state behind."
        ),
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_argparser().parse_args(argv)

    errors = assert_manifest(
        args.manifest_path,
        args.bundle_dir,
        strict=args.strict,
    )

    if errors:
        print("assert_thin_manifest: FAIL", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    print(
        f"assert_thin_manifest: OK ({args.manifest_path} matches "
        f"{args.bundle_dir}, strict={args.strict})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
