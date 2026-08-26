"""Fail if Soldr's embedded zccache dependency regains CLI capabilities.

The three crate manifests express the intended package-level requests, while
Cargo's resolved feature tree proves feature unification did not restore an
upstream CLI, standalone-daemon, download, symbols, or formatter capability
outside Soldr's rustfmt adapter.
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
    "crates/soldr-cli/Cargo.toml": ["formatter", "gha"],
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


def _check_tree(
    soldr: str,
    package: str,
    required_features: tuple[str, ...],
    forbidden_features: tuple[str, ...] = FORBIDDEN_FEATURES,
) -> list[str]:
    result = _run(soldr, "tree", "-p", package, "-e", "features", "-i", "zccache")
    if result.returncode:
        return [f"{package}: could not inspect zccache features:\n{result.stderr}"]
    failures: list[str] = []
    for feature in forbidden_features:
        if f'zccache feature "{feature}"' in result.stdout:
            failures.append(
                f"{package}: resolved forbidden zccache feature {feature!r}"
            )
    for feature in required_features:
        if f'zccache feature "{feature}"' not in result.stdout:
            failures.append(f"{package}: must resolve zccache/{feature}")
    return failures


def _check_no_normal_sevenz(soldr: str) -> list[str]:
    result = _run(soldr, "tree", "-p", "soldr-cli", "-e", "normal")
    if result.returncode:
        return [
            f"soldr-cli: could not inspect normal dependency tree:\n{result.stderr}"
        ]
    if "sevenz-rust v" in result.stdout:
        return [
            f"soldr-cli has a normal dependency path to sevenz-rust:\n{result.stdout}"
        ]
    return []


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", default="soldr", help="Soldr executable to use")
    args = parser.parse_args(argv)

    failures = _check_manifest_features()
    no_formatter = (*FORBIDDEN_FEATURES, "formatter")
    failures.extend(
        _check_tree(
            args.soldr,
            "soldr-cache",
            required_features=(),
            forbidden_features=no_formatter,
        )
    )
    failures.extend(
        _check_tree(
            args.soldr,
            "soldr-daemon",
            required_features=(),
            forbidden_features=no_formatter,
        )
    )
    failures.extend(
        _check_tree(args.soldr, "soldr-cli", required_features=("formatter", "gha"))
    )
    failures.extend(_check_no_normal_sevenz(args.soldr))
    if failures:
        print("zccache feature graph guard failed:", file=sys.stderr)
        print("\n".join(f"- {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("zccache feature graph: embedded-only (formatter + gha only on soldr-cli)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
