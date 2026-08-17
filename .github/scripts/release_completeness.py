#!/usr/bin/env python3
"""Terminal release-surface completeness gate (soldr#2469 step 1.1).

The 0.9.0 incident: run 31487675391 concluded green while the immutable
GitHub Release was missing 10 of its 17 assets, because the asset-verify
job was skipped (release already immutable) and skipped jobs do not fail
a run. This script is the terminal truth check: a normal release run must
hard-fail unless EVERY public surface is complete for the release ref —

  1. GitHub Release: all archives + wheels + SHA256SUMS derived from
     ci/canonical-targets.json (never a hand-maintained list),
  2. PyPI: every expected wheel filename present for the version,
  3. npm: the version exists for the package.

Verification logic is pure (lists in, failure strings out) so tests
reproduce the 0.9.0 false-green without any network. Network fetchers use
stdlib urllib only.

Usage (CI):
    python3 .github/scripts/release_completeness.py --version v0.9.1
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CONTRACT = REPO_ROOT / "ci" / "canonical-targets.json"
NPM_PACKAGE = "@zackees/soldr"
PYPI_PROJECT = "soldr"
GITHUB_REPO = os.environ.get("GITHUB_REPOSITORY", "zackees/soldr")

# Wheel platform tag per canonical triple. Parity with the contract's
# included targets is unit-tested; a new target must extend this table
# explicitly (the same reviewed-decision property as soldr#2469 step 2.1).
WHEEL_TAGS: dict[str, str] = {
    "x86_64-pc-windows-msvc": "win_amd64",
    "aarch64-pc-windows-msvc": "win_arm64",
    "x86_64-apple-darwin": "macosx_10_12_x86_64",
    "aarch64-apple-darwin": "macosx_11_0_arm64",
    "x86_64-unknown-linux-gnu": "manylinux_2_17_x86_64.manylinux2014_x86_64",
    "aarch64-unknown-linux-gnu": "manylinux_2_17_aarch64.manylinux2014_aarch64",
    "x86_64-unknown-linux-musl": "musllinux_1_2_x86_64",
    "aarch64-unknown-linux-musl": "musllinux_1_2_aarch64",
}


def included_triples(contract_path: Path = CONTRACT) -> list[str]:
    data = json.loads(contract_path.read_text(encoding="utf-8"))
    return [
        entry["triple"]
        for entry in data["targets"]
        if entry["release"]["status"] == "included"
    ]


def build_matrix(contract_path: Path = CONTRACT) -> list[dict[str, str]]:
    """Contract-generated release build matrix (soldr#2469 step 2.1).

    Returns the `strategy.matrix.include` list for release-auto.yml's build
    job, derived from each included target's `release.build` block — the
    workflow's hand-inlined matrix this replaces is exactly what let PR
    #2455 shrink the matrix and the contract together with nothing failing.
    """
    data = json.loads(contract_path.read_text(encoding="utf-8"))
    matrix = []
    for entry in data["targets"]:
        if entry["release"]["status"] != "included":
            continue
        build = entry["release"]["build"]
        matrix.append(
            {
                "name": build["name"],
                "runner": build["runner"],
                "target": entry["triple"],
                "setup_target": build["setup_target"],
                "binary": build["binary"],
            }
        )
    return matrix


def expected_github_assets(tag: str, triples: list[str]) -> list[str]:
    version = tag.lstrip("v")
    assets = [f"soldr-{tag}-{triple}.tar.zst" for triple in triples]
    assets += [
        f"soldr-{version}-py3-none-{WHEEL_TAGS[triple]}.whl" for triple in triples
    ]
    assets.append(f"soldr-{tag}-SHA256SUMS.txt")
    return assets


def expected_pypi_files(tag: str, triples: list[str]) -> list[str]:
    version = tag.lstrip("v")
    return [f"soldr-{version}-py3-none-{WHEEL_TAGS[triple]}.whl" for triple in triples]


def verify_surfaces(
    tag: str,
    triples: list[str],
    github_assets: list[str],
    pypi_files: list[str],
    npm_versions: list[str],
) -> list[str]:
    """Pure completeness check. Returns one failure line per gap."""
    failures = []
    github_present = set(github_assets)
    for asset in expected_github_assets(tag, triples):
        if asset not in github_present:
            failures.append(f"github-release missing asset: {asset}")
    pypi_present = set(pypi_files)
    for wheel in expected_pypi_files(tag, triples):
        if wheel not in pypi_present:
            failures.append(f"pypi missing file: {wheel}")
    version = tag.lstrip("v")
    if version not in npm_versions:
        failures.append(f"npm {NPM_PACKAGE} missing version: {version}")
    return failures


def fetch_json(url: str, headers: dict[str, str] | None = None) -> dict:
    request = urllib.request.Request(url, headers=headers or {})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read().decode("utf-8"))


def fetch_github_assets(tag: str) -> list[str]:
    headers = {"Accept": "application/vnd.github+json"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = fetch_json(
        f"https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{tag}",
        headers,
    )
    return [asset["name"] for asset in data.get("assets", [])]


def fetch_pypi_files(version: str) -> list[str]:
    data = fetch_json(f"https://pypi.org/pypi/{PYPI_PROJECT}/{version}/json")
    return [entry["filename"] for entry in data.get("urls", [])]


def fetch_npm_versions() -> list[str]:
    quoted = NPM_PACKAGE.replace("/", "%2F")
    data = fetch_json(f"https://registry.npmjs.org/{quoted}")
    return list(data.get("versions", {}).keys())


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="release tag, e.g. v0.9.1")
    parser.add_argument(
        "--list-expected-github-assets",
        action="store_true",
        help="print the contract-derived GitHub asset names (one per line) "
        "and exit without any network access — the single source the "
        "workflow's inline asset lists were replaced by (soldr#2469 "
        "step 2.2)",
    )
    parser.add_argument(
        "--build-matrix",
        action="store_true",
        help="print the contract-derived release build matrix as a JSON "
        "array for `strategy.matrix.include`, no network access "
        "(soldr#2469 step 2.1)",
    )
    opts = parser.parse_args(argv)
    if opts.build_matrix:
        print(json.dumps(build_matrix(), separators=(",", ":")))
        return 0
    if not opts.version:
        parser.error("--version is required except with --build-matrix")
    tag = opts.version if opts.version.startswith("v") else f"v{opts.version}"

    triples = included_triples()
    if opts.list_expected_github_assets:
        for asset in expected_github_assets(tag, triples):
            print(asset)
        return 0
    failures = verify_surfaces(
        tag,
        triples,
        fetch_github_assets(tag),
        fetch_pypi_files(tag.lstrip("v")),
        fetch_npm_versions(),
    )
    if failures:
        print(f"release {tag} is INCOMPLETE ({len(failures)} gaps):", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        print(
            "A green run must mean a complete public release surface "
            "(soldr#2469 step 1.1). Recovery-only dispatches must be named "
            "and reported as recovery, never as a normal release result.",
            file=sys.stderr,
        )
        return 1
    total = len(expected_github_assets(tag, triples))
    print(
        f"release {tag} complete: {total} GitHub assets, "
        f"{len(expected_pypi_files(tag, triples))} PyPI wheels, npm published"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
