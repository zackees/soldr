"""Fail if Soldr's embedded zccache dependency regains CLI capabilities.

soldr#2899 dropped zccache's `cli` feature from all three consuming crates.
Nothing in the Rust source can hold that line on its own: a `cli`-adjacent
feature can come back through any of three doors — a manifest edit, Cargo's
feature unification pulling one in from a sibling crate, or a transitive
dependency requesting it — and none of those touch a `.rs` file, so the
in-tree source lints cannot see them.

This guard closes all three. It asserts the per-crate manifest requests are
exactly what soldr#2899 settled on, then asserts the *resolved* feature tree
for each crate agrees, then asserts no normal (non-dev, non-build) dependency
path reaches the archive stack that `download-client` used to bring in.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# soldr#2899's resolved answer. `soldr-cli` needs `formatter` for the
# `soldr rustfmt` format cache; nothing else needs any zccache feature.
MANIFEST_FEATURES = {
    "crates/soldr-cache/Cargo.toml": [],
    "crates/soldr-daemon/Cargo.toml": [],
    "crates/soldr-cli/Cargo.toml": ["formatter"],
}

# `cli` expands to all of these; each is independently a way back in.
FORBIDDEN_FEATURES = (
    "cli",
    "daemon-entry",
    "download-client",
    "download",
    "download-protocol",
    "gha",
    "symbols",
)

# Packages that only ever reached soldr through `download-client`. A normal
# dependency edge to any of them means an archive/CLI feature came back.
FORBIDDEN_NORMAL_PACKAGES = (
    "sevenz-rust",
    "sevenz-rust2",
    "lzma-rs",
    "ruzstd",
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
        dev = manifest.get("dev-dependencies", {}).get("zccache")
        if dev is not None:
            failures.append(
                f"{relative}: zccache must not be a dev-dependency; "
                "`test-support` does not declare the tracing-subscriber it uses, "
                "so it only builds when a CLI feature is also on (soldr#2899)"
            )
    return failures


def _check_tree(
    soldr: str,
    package: str,
    required_features: tuple[str, ...],
    forbidden_features: tuple[str, ...],
) -> list[str]:
    result = _run(soldr, "tree", "-p", package, "-e", "features", "-i", "zccache")
    if result.returncode:
        return [f"{package}: could not inspect zccache features:\n{result.stderr}"]
    failures: list[str] = []
    for feature in forbidden_features:
        if f'zccache feature "{feature}"' in result.stdout:
            failures.append(f"{package}: resolved forbidden zccache feature {feature!r}")
    for feature in required_features:
        if f'zccache feature "{feature}"' not in result.stdout:
            failures.append(f"{package}: must resolve zccache/{feature}")
    return failures


def _check_no_normal_archive_stack(soldr: str) -> list[str]:
    result = _run(soldr, "tree", "-p", "soldr-cli", "-e", "normal")
    if result.returncode:
        return [f"soldr-cli: could not inspect normal dependency tree:\n{result.stderr}"]
    failures = []
    for package in FORBIDDEN_NORMAL_PACKAGES:
        if f"{package} v" in result.stdout:
            failures.append(
                f"soldr-cli has a normal dependency path to {package}; "
                "a zccache download/archive feature came back"
            )
    return failures


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", default="soldr", help="Soldr executable to use")
    args = parser.parse_args(argv)

    failures = _check_manifest_features()
    # `formatter` is soldr-cli's alone: neither the cache nor the daemon has a
    # rustfmt path, so resolving it there means unification leaked.
    no_formatter = (*FORBIDDEN_FEATURES, "formatter")
    for package in ("soldr-cache", "soldr-daemon"):
        failures.extend(
            _check_tree(
                args.soldr,
                package,
                required_features=(),
                forbidden_features=no_formatter,
            )
        )
    failures.extend(
        _check_tree(
            args.soldr,
            "soldr-cli",
            required_features=("formatter",),
            forbidden_features=FORBIDDEN_FEATURES,
        )
    )
    failures.extend(_check_no_normal_archive_stack(args.soldr))
    if failures:
        print("zccache feature graph guard failed:", file=sys.stderr)
        print("\n".join(f"- {failure}" for failure in failures), file=sys.stderr)
        return 1
    print("zccache feature graph: embedded-only (formatter on soldr-cli, nothing else)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
