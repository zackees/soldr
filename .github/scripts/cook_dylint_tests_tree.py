#!/usr/bin/env python3
"""Pre-cook the `target/dylint/tests` third-party dependency layer (soldr#3042).

`soldr dylint cook --tree tests` pre-compiles the `dylint_testing` ->
`compiletest_rs` / `git2` / `libgit2-sys` layer, plus `dylint`'s own build
script, so those compiles do not happen inside the Dylint UI-test branch while
Fresh Nextest is running. PR #3038 failed three different ways in exactly that
window: a fixture timeout, a 14 GiB compiler shed, and an `ETXTBSY` exec race
on `dylint` v6.0.3's just-linked build script.

This persists NOTHING across runs. It fills an in-run target tree; the
cross-run saving comes from the Tier-2 object store (soldr#3041).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path


def lint_roots(repo_root: Path, lints_dir: str = "dylints") -> list[Path]:
    """Every immediate subdirectory of `<repo_root>/<lints_dir>` with a `Cargo.toml`.

    Each lint crate is its OWN cargo workspace with its own `Cargo.lock` (see
    `dylints/*/Cargo.toml`, which carries a bare `[workspace]`), which is why
    the cook runs once per crate rather than once at the repo root.
    """
    base = repo_root / lints_dir
    if not base.is_dir():
        return []
    return sorted(
        entry
        for entry in base.iterdir()
        if entry.is_dir() and (entry / "Cargo.toml").is_file()
    )


def cook_command(soldr: Path, target_root: Path) -> list[str]:
    """The `soldr dylint cook` invocation for one lint crate's tests tree.

    `--target-root` is required because cwd is `dylints/<lint>`, whose own
    `target/` must stay empty (`.github/scripts/verify_dylint_target_dirs.py`
    fails otherwise) while the tree that must be filled is the repo's.
    """
    return [
        str(soldr),
        "dylint",
        "cook",
        "--tree",
        "tests",
        "--tests",
        "--target-root",
        str(target_root),
        "--json",
    ]


def cook_env(base: Mapping[str, str], soldr: Path) -> dict[str, str]:
    """The environment the cook runs under, mirroring a real Dylint stage.

    Mirrors what `crates/soldr-cli/src/ci_test/execute.rs` (`spawn`, the
    `stage.domain.starts_with("dylint-")` branch) gives every Dylint stage.
    `SOLDR_LINKER=default` is load-bearing because soldr's linker injection
    rewrites RUSTFLAGS and RUSTFLAGS are in every unit's fingerprint, so a
    cook without it produces artifacts cargo rejects as stale. `CARGO_TARGET_DIR`
    is dropped so it cannot fight `--target-root`.
    """
    env = dict(base)
    env["SOLDR_RUSTC_WRAPPER"] = str(soldr)
    env["SOLDR_LINKER"] = "default"
    env["SOLDR_NO_GC_TARGET"] = "1"
    for key in ("CARGO_BUILD_JOBS", "SOLDR_JOBS", "CARGO_TARGET_DIR"):
        env.pop(key, None)
    return env


def parse_outcome(stdout: str) -> str:
    """The `"outcome"` field of the last JSON object line of `stdout`.

    Scans backwards rather than reading only the final line: `soldr dylint
    cook --json` prints its payload last, but the child Cargo it drives is
    relayed through the same stream, so a stray trailing line must not turn
    a real result into `unknown`. A run with no JSON object at all yields
    `unknown`, which is reported but never fails the step -- the child's
    exit status is what decides that.
    """
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict) and "outcome" in payload:
            return str(payload["outcome"])
    return "unknown"


def main(argv: list[str] | None = None) -> int:
    """Cook the tests tree for every lint crate, stopping at the first failure."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", required=True, type=Path)
    parser.add_argument("--target-root", required=True, type=Path)
    parser.add_argument(
        "--repo-root", type=Path, default=Path(__file__).resolve().parents[2]
    )
    parser.add_argument("--lints-dir", default="dylints")
    args = parser.parse_args(argv)

    roots = lint_roots(args.repo_root, args.lints_dir)
    if not roots:
        # Fail rather than succeed at nothing. A wrong `--repo-root` (or a
        # renamed `dylints/`) would otherwise make this step a silent no-op,
        # and the only symptom would be the contention it exists to remove
        # quietly returning to the Dylint UI-test / Fresh Nextest window --
        # a green step and a slower, flakier `ci-test`.
        print(
            "cook_dylint_tests_tree: no lint crates under "
            f"{args.repo_root / args.lints_dir}",
            file=sys.stderr,
        )
        return 1
    env = cook_env(os.environ, args.soldr)

    # Run sequentially: every lint crate shares one target directory and
    # `dylint_cook` takes an exclusive lock on it, so parallelism here buys
    # nothing and reintroduces the contention this script exists to remove.
    #
    # Only stdout is captured, and only because the `--json` payload has to be
    # parsed out of it. stderr is deliberately inherited so Cargo's progress
    # streams live: these are multi-minute compiles, and a step that prints
    # nothing until it finishes tells you nothing at all if the runner kills
    # it first.
    for lint_root in roots:
        result = subprocess.run(
            cook_command(args.soldr, args.target_root),
            cwd=lint_root,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            check=False,
        )
        outcome = parse_outcome(result.stdout)
        print(f"dylint tests-tree cook: {lint_root.name} -> {outcome}", flush=True)
        if result.returncode != 0:
            sys.stdout.write(result.stdout)
            print(
                f"cook_dylint_tests_tree: {lint_root.name} exited "
                f"{result.returncode}",
                flush=True,
            )
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
