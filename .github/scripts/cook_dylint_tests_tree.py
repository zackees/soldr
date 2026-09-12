#!/usr/bin/env python3
"""Pre-cook the `target/dylint/tests` third-party dependency layer (soldr#3042).

`soldr dylint cook --tree tests` compiles the `dylint_testing` ->
`compiletest_rs` / `git2` / `libgit2-sys` layer, plus `dylint`'s own build
script, ahead of the Dylint UI-test branch. Without this step those ~137
units compiled inside that branch while Fresh Nextest execution ran
concurrently -- the exact window that made PR #3038 fail three different
ways: a fixture timeout, a 14 GiB compiler shed, and an `ETXTBSY` exec race
on `dylint` v6.0.3's just-linked build script.

This persists NOTHING across runs. It fills an in-run target tree
(`target/dylint/tests`), which stays banned from every cross-run store
(`ci/cache-ownership.json`); the cross-run saving comes from the Tier-2
object store (soldr#3041), not from anything this script writes.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tomllib
from collections.abc import Mapping, Sequence
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


def dependency_closure(lint_root: Path) -> frozenset[tuple[str, str, str | None]]:
    """The `(name, version, checksum)` set a lint crate's lockfile resolves to.

    The crate's own entry is excluded: it is the one package guaranteed to
    differ between the six, and it contributes nothing to the third-party
    layer this script cooks.
    """
    lock = tomllib.loads((lint_root / "Cargo.lock").read_text(encoding="utf-8"))
    return frozenset(
        (package["name"], package["version"], package.get("checksum"))
        for package in lock.get("package", ())
        if package["name"] != lint_root.name
    )


def diverging_closures(roots: Sequence[Path]) -> list[str]:
    """Lint crates whose dependency closure differs from the first root's.

    The six crates are separate cargo workspaces with separate lockfiles and
    nothing keeping them in step, so they drift apart silently: before
    soldr#3159 twelve shared deps had diverged (`cc` alone at 1.4.0, 1.4.1,
    1.4.2 and 1.4.4), which made the shared tests tree compile four copies of
    `cc` and two of `thiserror` for no reason.

    `tests/test_cook_dylint_tests_tree.py` calls THIS function rather than
    re-deriving the closure, so the guard and the definition cannot drift
    apart the way the lockfiles did.
    """
    if not roots:
        return []
    reference = dependency_closure(roots[0])
    return [root.name for root in roots[1:] if dependency_closure(root) != reference]


def cook_command(soldr: Path, target_root: Path) -> list[str]:
    """The `soldr dylint cook` invocation for the tests-tree dependency layer.

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
    """The environment `soldr dylint cook --tree tests` must run under.

    Mirrors what `crates/soldr-cli/src/ci_test/execute.rs` (`spawn`, the
    `stage.domain.starts_with("dylint-")` branch) gives every Dylint stage.
    `SOLDR_LINKER=default` is load-bearing because soldr's linker injection
    rewrites RUSTFLAGS and RUSTFLAGS are in every unit's fingerprint, so a
    cook without it produces artifacts cargo rejects as stale.
    `CARGO_TARGET_DIR` is dropped so it cannot fight `--target-root`.
    """
    env = dict(base)
    env["SOLDR_RUSTC_WRAPPER"] = str(soldr)
    env["SOLDR_LINKER"] = "default"
    env["SOLDR_NO_GC_TARGET"] = "1"
    for removed in ("CARGO_BUILD_JOBS", "SOLDR_JOBS", "CARGO_TARGET_DIR"):
        env.pop(removed, None)
    return env


def parse_outcome(stdout: str) -> str:
    """The `"outcome"` field of the last non-empty JSON line, or `"unknown"`."""
    for line in reversed(stdout.splitlines()):
        stripped = line.strip()
        if not stripped:
            continue
        try:
            payload = json.loads(stripped)
        except json.JSONDecodeError:
            return "unknown"
        return str(payload.get("outcome", "unknown"))
    return "unknown"


def main(argv: list[str] | None = None) -> int:
    """Cook the tests-tree dependency layer once per lint crate, sequentially."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", required=True, type=Path)
    parser.add_argument("--target-root", required=True, type=Path)
    parser.add_argument(
        "--repo-root", default=Path(__file__).resolve().parents[2], type=Path
    )
    parser.add_argument("--lints-dir", default="dylints")
    args = parser.parse_args(argv)

    # Iterate lint roots sequentially: they share one target directory and
    # `dylint_cook` takes an exclusive lock on it, so parallelism buys
    # nothing and reintroduces the contention this step exists to remove.
    #
    # Every crate reports `miss` rather than `skip`. All six write into the
    # SAME tree, so they share one `.soldr-dylint-cook-v1.json` marker, while
    # each one's cook key hashes its own manifest+lockfile -- so crate N always
    # finds crate N-1's marker and re-runs.
    #
    # This comment used to add that "cargo's own fingerprints make crates 2..6
    # near-no-ops once the shared third-party layer is built by the first."
    # That is measurably false and soldr#3157 caught it: the six invocations
    # cost 207s on the Linux gate. Measured locally against an already-filled
    # tree, crates 2..6 each re-run cargo over all 86 units --
    #
    #   crate 1, cold tree   37.0s  outcome=miss  Compiling=86
    #   crate 1, again        1.2s  outcome=skip  Compiling=0
    #   crate 2               10.1s outcome=miss  Compiling=86, zccache 335/335 HIT
    #
    # -- so the marker DOES short-circuit a repeat of the same crate (1.2s,
    # cargo never invoked), and a different crate does not reuse the first
    # crate's fingerprints even though every rustc invocation is byte-identical
    # (hence the 100% zccache hit rate). The cost is cargo orchestration over a
    # 200-400 unit graph, five times over, not compilation.
    #
    # soldr#3159 proposes cooking once and letting one crate stand in for the
    # rest. Note before attempting it: the `Compiling=86` above means crate 1's
    # tree is NOT fresh for crate 2, so the work relocates into the UI-test
    # stages rather than disappearing.
    #
    # Do not "fix" the repeated miss by collapsing the cook KEY: the two trees
    # and the six crates must keep distinct keys or one cook would satisfy
    # another's marker with the wrong artifacts on disk.
    roots = lint_roots(args.repo_root, args.lints_dir)
    env = cook_env(os.environ, args.soldr)
    for lint_root in roots:
        # stderr is deliberately NOT captured: cargo's progress and its
        # error text go there, and a step that buffers a multi-minute
        # compile shows nothing at all while it runs and nothing useful if
        # the job is cancelled. Only stdout is captured, because the
        # `--json` outcome line has to be parsed out of it.
        result = subprocess.run(
            cook_command(args.soldr, args.target_root),
            cwd=lint_root,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            check=False,
        )
        sys.stdout.write(result.stdout)
        outcome = parse_outcome(result.stdout)
        print(f"dylint tests-tree cook: {lint_root.name} -> {outcome}")
        if result.returncode != 0:
            print(
                f"cook_dylint_tests_tree: {lint_root.name} failed with "
                f"exit code {result.returncode}",
                file=sys.stderr,
            )
            return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
