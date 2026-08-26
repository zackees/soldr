"""Fail if Soldr's embedded zccache dependency regains CLI capabilities.

The three crate manifests express the intended package-level requests, while
Cargo's resolved feature tree proves feature unification did not restore an
upstream CLI, standalone-daemon, download, or symbols capability.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tomllib
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
MANIFEST_FEATURES = {
    "crates/soldr-cache/Cargo.toml": [],
    "crates/soldr-daemon/Cargo.toml": [],
    "crates/soldr-cli/Cargo.toml": ["gha"],
}
FORBIDDEN_FEATURES = (
    "cli",
    "daemon-entry",
    "download-client",
    "download",
    "download-protocol",
    "symbols",
)


def _run(soldr: str, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [soldr, "cargo", *args],
        cwd=REPO_ROOT,
        check=False,
        text=True,
        capture_output=True,
    )


def _check_manifest_features() -> list[str]:
    failures: list[str] = []
    for relative, expected_features in MANIFEST_FEATURES.items():
        manifest = tomllib.loads((REPO_ROOT / relative).read_text(encoding="utf-8"))
        dependency = manifest["dependencies"]["zccache"]
        actual_features = dependency.get("features", [])
        if dependency.get("default-features") is not False:
            failures.append(f"{relative}: zccache must set default-features = false")
        if actual_features != expected_features:
            failures.append(
                f"{relative}: zccache features are {actual_features!r}; "
                f"expected {expected_features!r}"
            )
    return failures


def _check_tree(soldr: str, package: str, require_gha: bool) -> list[str]:
    result = _run(soldr, "tree", "-p", package, "-e", "features", "-i", "zccache")
    if result.returncode:
        return [f"{package}: could not inspect zccache features:\n{result.stderr}"]
    failures: list[str] = []
    for feature in FORBIDDEN_FEATURES:
        if f'zccache feature "{feature}"' in result.stdout:
            failures.append(f"{package}: resolved forbidden zccache feature {feature!r}")
    has_gha = 'zccache feature "gha"' in result.stdout
    if has_gha != require_gha:
        expectation = "must" if require_gha else "must not"
        failures.append(f"{package}: {expectation} resolve zccache/gha")
    return failures


def _check_no_normal_sevenz(soldr: str) -> list[str]:
    result = _run(soldr, "tree", "-p", "soldr-cli", "-e", "normal", "-i", "sevenz-rust")
    if result.returncode == 0:
        return [f"soldr-cli has a normal dependency path to sevenz-rust:\n{result.stdout}"]
    return []


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", default="soldr", help="Soldr executable to use")
    args = parser.parse_args(argv)

    failures = _check_manifest_features()
    failures.extend(_check_tree(args.soldr, "soldr-cache", require_gha=False))
    failures.extend(_check_tree(args.soldr, "soldr-daemon", require_gha=False))
    failures.extend(_check_tree(args.soldr, "soldr-cli", require_gha=True))
    failures.extend(_check_no_normal_sevenz(args.soldr))
    if failures:
        print("zccache feature graph guard failed:", file=sys.stderr)
        print("\n".join(f"- {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("zccache feature graph: embedded-only (gha only on soldr-cli)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
