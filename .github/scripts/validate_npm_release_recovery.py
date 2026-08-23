#!/usr/bin/env python3
"""Validate an immutable release before an npm-only recovery publish."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.parse
import urllib.request
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any

# soldr#2763: the npm recovery lane pins Python 3.13, but this script is also
# run by hand during an incident, where the interpreter is whatever the operator
# has. Fail with an actionable message instead of an ImportError traceback.
try:
    import tomllib as _toml  # 3.11+
except ImportError:  # pragma: no cover -- older Pythons
    try:
        import tomli as _toml  # type: ignore[import,no-redef]
    except ImportError:
        sys.stderr.write(
            "validate_npm_release_recovery.py: needs Python 3.11+ (tomllib) "
            "or `pip install tomli`\n"
        )
        sys.exit(2)


class ValidationError(RuntimeError):
    """The requested npm recovery is not tied to a complete release."""


JsonObject = dict[str, Any]
JsonFetcher = Callable[[str, str | None], JsonObject]
GitRunner = Callable[[Path, Sequence[str]], str]


def fetch_json(url: str, token: str | None = None) -> JsonObject:
    headers = {"Accept": "application/vnd.github+json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=30) as response:
        payload = json.load(response)
    if not isinstance(payload, dict):
        raise ValidationError(f"expected a JSON object from {url}")
    return payload


def git_output(source_dir: Path, arguments: Sequence[str]) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=source_dir,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise ValidationError(f"git {' '.join(arguments)} failed: {detail}")
    return result.stdout.strip()


def _versions(source_dir: Path) -> tuple[str, str, str]:
    package = json.loads((source_dir / "package.json").read_text(encoding="utf-8"))
    with (source_dir / "Cargo.toml").open("rb") as stream:
        cargo = _toml.load(stream)
    with (source_dir / "Cargo.lock").open("rb") as stream:
        lock = _toml.load(stream)

    npm_version = package["version"]
    cargo_version = cargo["workspace"]["package"]["version"]
    cli_versions = [
        item["version"] for item in lock["package"] if item["name"] == "soldr-cli"
    ]
    if len(cli_versions) != 1:
        raise ValidationError("Cargo.lock must contain exactly one soldr-cli package")
    return npm_version, cargo_version, cli_versions[0]


def validate_recovery(
    *,
    repository: str,
    release_ref: str,
    source_dir: Path,
    token: str | None,
    get_json: JsonFetcher = fetch_json,
    run_git: GitRunner = git_output,
) -> str:
    """Return the validated version or raise ``ValidationError``."""
    if not re.fullmatch(
        r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", release_ref
    ):
        raise ValidationError("npm recovery requires an exact stable vX.Y.Z tag")

    npm_version, cargo_version, lock_version = _versions(source_dir)
    version = release_ref.removeprefix("v")
    if {npm_version, cargo_version, lock_version} != {version}:
        raise ValidationError(
            "release tag, package.json, Cargo.toml, and Cargo.lock versions must match"
        )

    head = run_git(source_dir, ["rev-parse", "--verify", "HEAD"])
    tag_commit = run_git(
        source_dir, ["rev-parse", "--verify", f"refs/tags/{release_ref}^{{commit}}"]
    )
    if tag_commit != head:
        raise ValidationError(
            f"{release_ref} resolves to {tag_commit}, not checked-out {head}"
        )

    encoded_ref = urllib.parse.quote(release_ref, safe="")
    release_url = (
        f"https://api.github.com/repos/{repository}/releases/tags/{encoded_ref}"
    )
    release = get_json(release_url, token)
    if release.get("tag_name") != release_ref:
        raise ValidationError("GitHub release tag does not match the requested tag")
    if release.get("draft") is not False:
        raise ValidationError("GitHub release must be published, not draft")
    if release.get("immutable") is not True:
        raise ValidationError("GitHub release must be immutable")
    target = release.get("target_commitish")
    if isinstance(target, str) and re.fullmatch(r"[0-9a-fA-F]{40}", target):
        if target.lower() != head.lower():
            raise ValidationError("GitHub release target does not match the tag commit")

    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ValidationError("GitHub release assets are missing")
    asset_names = {asset.get("name") for asset in assets if isinstance(asset, dict)}
    checksum_name = f"soldr-v{version}-SHA256SUMS.txt"
    if checksum_name not in asset_names:
        raise ValidationError(f"GitHub release is missing {checksum_name}")
    archives = {
        name
        for name in asset_names
        if isinstance(name, str)
        and name.startswith(f"soldr-v{version}-")
        and name.endswith(".tar.zst")
    }
    github_wheels = {
        name
        for name in asset_names
        if isinstance(name, str)
        and name.startswith(f"soldr-{version}-")
        and name.endswith(".whl")
    }
    if not archives or not github_wheels:
        raise ValidationError("GitHub release must contain archives and wheels")

    pypi_url = f"https://pypi.org/pypi/soldr/{version}/json"
    pypi = get_json(pypi_url, None)
    if pypi.get("info", {}).get("version") != version:
        raise ValidationError("PyPI release version does not match the requested tag")
    pypi_wheels = {
        item.get("filename")
        for item in pypi.get("urls", [])
        if isinstance(item, dict) and item.get("packagetype") == "bdist_wheel"
    }
    if github_wheels != pypi_wheels:
        raise ValidationError("GitHub and PyPI wheel sets do not match")

    return version


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--release-ref", required=True)
    parser.add_argument("--source-dir", type=Path, default=Path.cwd())
    args = parser.parse_args()

    version = validate_recovery(
        repository=args.repository,
        release_ref=args.release_ref,
        source_dir=args.source_dir.resolve(),
        token=os.environ.get("GH_TOKEN"),
    )
    print(f"validated immutable npm recovery release v{version}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, KeyError, TypeError, ValidationError, json.JSONDecodeError) as exc:
        raise SystemExit(f"npm release recovery validation failed: {exc}") from exc
