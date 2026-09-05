#!/usr/bin/env python3
"""Cook the stable dependency tree for one target, as a single CI entry point.

soldr#3043 Phase 2: `_build-and-test.yml` must stay orchestration-only
(CLAUDE.md bans complex inline CI logic), so the `soldr cook` invocation for
the stable host-validation tree -- argv construction, exit-code triage, and
outcome reporting -- lives here instead of in the workflow YAML.

Exit codes are keyed to the constants `soldr cook` itself defines
(crates/soldr-cli/src/cook.rs):

    0   success (cook ran, or classified `built`/`hydrated`/`warm-skip`/
        `restore-declined`; see `classify()`)
    3   COOK_SKIPPED_UNCOOKABLE_WORKSPACE -- a path dependency the cargo-chef
        recipe cannot materialise. This is a hard failure here: per
        soldr#3043 step 3 the fix is to exclude the offending workspace
        member with `-p`, not to relax the guard.
    4   `--require-warm` was passed and the run neither hydrated from a
        prior cook artifact nor warm-skipped Phase 2. Without `--require-warm`
        this case only emits a `::warning` annotation -- the acceptance
        number is not measurable until soldr#3040's analyzer lands.
    5   COOK_ARTIFACT_NOT_INDEXED -- cook built and packed the archive but
        its closing `CookRecord` found no daemon, so no index row names the
        artifact and no later run can hydrate from it (soldr#3117). `soldr
        cook` itself only warns here, because a missing daemon must not fail
        a developer's local cook; in this lane the index row IS the product,
        so an unindexed artifact is a dead mechanism and fails the step.
    N   any other non-zero exit from `soldr cook` itself, propagated as-is.

This is a diagnostic layer over `soldr cook`, not a reimplementation of it:
every classification is read back out of cook's own stderr markers, which are
quoted verbatim below rather than re-derived.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import time
from collections.abc import Callable, Sequence
from pathlib import Path

# soldr cook's own literal markers. Do not invent new spellings here --
# these are exactly the strings the binary emits, and drift silently breaks
# classification without touching cook's own tests.
#
#   HYDRATE_MARKER: emit_hydrate_line(),
#     crates/soldr-cli/src/cargo_front_door/cook_hydrate.rs
#   WARM_SKIP_MARKER: the soldr#621 warm-cook marker check,
#     crates/soldr-cli/src/cook.rs
#   DECISION_SKIP_MARKER: decide_cook_restore()'s Skip branch,
#     crates/soldr-cli/src/cargo_front_door/cook_hydrate.rs
#   UNINDEXED_MARKER: the CookRecord failure branch of
#     index_cooked_artifact_with_packer(), crates/soldr-cli/src/cook.rs
HYDRATE_MARKER = "soldr cook: auto-hydrate activated"
WARM_SKIP_MARKER = "soldr cook: warm-cook detected"
DECISION_SKIP_MARKER = "soldr cook: decision=skip"
UNINDEXED_MARKER = "CookRecord to daemon failed"

# crates/soldr-cli/src/cook.rs: `const COOK_SKIPPED_UNCOOKABLE_WORKSPACE: i32 = 3;`
COOK_SKIPPED_UNCOOKABLE_WORKSPACE = 3
# This script's own exit code for a `--require-warm` violation. Not a soldr
# constant -- soldr cook itself always exits 0 for a `built`/`restore-declined`
# outcome; the failure is this wrapper's opinion, gated behind the flag.
REQUIRE_WARM_FAILURE = 4
# This script's own exit code for a built-but-unindexed archive (soldr#3117).
# Also not a soldr constant: cook exits 0 and warns, for the reason given in
# the module docstring.
COOK_ARTIFACT_NOT_INDEXED = 5

# `--all-targets` is what makes cargo-chef build dev-dependencies, which the
# ci-test stable tree needs (clippy `--all-targets`, nextest `--lib --tests`).
# It is the default here rather than something the workflow must remember to
# pass.
DEFAULT_CHEF_ARGS: tuple[str, ...] = ("--all-targets",)

Runner = Callable[[list[str], Path], subprocess.CompletedProcess[str]]


def default_runner(command: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    """Run `command` from `cwd`, capturing both streams as text."""
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True, check=False)


def build_argv(soldr: str, target: str, chef_args: Sequence[str]) -> list[str]:
    """Build the `soldr cook` argv for the stable tree.

    `soldr cook`'s own parser (`parse_cook_args`, crates/soldr-cli/src/cook.rs)
    recognises only `--release --workspace/--all --keep-recipe --prepare-only
    --cook-only --no-trim --target --profile --recipe-path -p/--package`, and
    REJECTS unknown flags before `--`. Everything cargo-chef itself needs
    (like `--all-targets`) must therefore be forwarded after a literal `--`.

    Never pass `--no-trim`: trimming is what keeps the archive inside the
    2.0 GiB allocation soldr#3043 budgets for it.
    """
    return [soldr, "cook", "--workspace", "--target", target, "--", *chef_args]


def classify(stderr: str) -> tuple[str, str]:
    """Classify a `soldr cook` run from its stderr. Returns (outcome, detail).

    `detail` is empty except for `restore-declined`, where it carries the
    `reason=` field off cook's own decision line.
    """
    if HYDRATE_MARKER in stderr:
        return "hydrated", ""
    if WARM_SKIP_MARKER in stderr:
        return "warm-skip", ""
    if DECISION_SKIP_MARKER in stderr:
        marker = "reason="
        for line in stderr.splitlines():
            if DECISION_SKIP_MARKER not in line:
                continue
            index = line.find(marker)
            if index == -1:
                break
            return "restore-declined", line[index + len(marker) :].strip()
        return "restore-declined", ""
    if UNINDEXED_MARKER in stderr:
        return "built-unindexed", ""
    return "built", ""


def cook_archive_bytes(cache_dir: Path) -> int:
    """Sum `<cache_dir>/cache/cook/**/*.tar.zst`, skipping the `.tmp/` staging dir.

    `cache/cook` is the cook archive root: `cook_cache_dir` is
    `paths.cache.join("cook")`, and `SoldrPaths` sets `cache = root.join("cache")`
    where `root` is `SOLDR_CACHE_DIR`
    (crates/soldr-cache/src/cache_lib/cook_archive.rs line 58,
    crates/soldr-core/src/core/paths.rs line 103). `.tmp/<rand>.tar.zst` is an
    in-flight packer write, not a saved archive, so it does not count.
    """
    cook_dir = cache_dir / "cache" / "cook"
    if not cook_dir.is_dir():
        return 0
    total = 0
    for path in cook_dir.rglob("*.tar.zst"):
        if ".tmp" in path.relative_to(cook_dir).parts:
            continue
        try:
            total += path.stat().st_size
        except OSError:
            continue
    return total


def report_lines(
    target: str,
    outcome: str,
    detail: str,
    elapsed_seconds: float,
    archive_bytes: int | None,
) -> list[str]:
    """Human-readable summary lines, shared between stdout and the step summary."""
    line = f"cook[{target}]: outcome={outcome} elapsed={elapsed_seconds:.1f}s"
    if detail:
        line += f" reason={detail!r}"
    lines = [line]
    if archive_bytes is not None:
        mib = archive_bytes / (1024 * 1024)
        lines.append(
            f"cook[{target}]: cache/cook archive size={mib:.1f} MiB "
            "(counts against soldr#3047's 2.0 GiB allocation)"
        )
    return lines


def append_summary(lines: Sequence[str]) -> None:
    """Append `lines` as a bullet list to `GITHUB_STEP_SUMMARY`, when set."""
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    try:
        with Path(summary).open("a", encoding="utf-8") as handle:
            for line in lines:
                handle.write(f"- {line}\n")
    except OSError as error:
        print(f"run_stable_cook: summary unwritable: {error}", file=sys.stderr)


def main(argv: list[str] | None = None, runner: Runner = default_runner) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--soldr", required=True, help="absolute path to the soldr binary to run"
    )
    parser.add_argument(
        "--target", required=True, help="target triple, e.g. x86_64-unknown-linux-gnu"
    )
    parser.add_argument(
        "--chef-arg",
        dest="chef_args",
        action="append",
        default=None,
        help=(
            "cargo-chef arg forwarded after `--`; repeatable (default: "
            "--all-targets). A value that itself starts with `-` must use "
            "the `--chef-arg=--flag` form -- argparse cannot otherwise tell "
            "it apart from a new option"
        ),
    )
    parser.add_argument(
        "--require-warm",
        action="store_true",
        help="fail (exit 4) when the run did not hydrate or warm-skip",
    )
    parser.add_argument(
        "--cache-dir",
        default=None,
        help="SOLDR_CACHE_DIR, used only to report the cook archive size",
    )
    args = parser.parse_args(argv)

    chef_args = args.chef_args if args.chef_args else list(DEFAULT_CHEF_ARGS)
    command = build_argv(args.soldr, args.target, chef_args)
    repo_root = Path(__file__).resolve().parents[2]

    started = time.monotonic()
    result = runner(command, repo_root)
    elapsed_seconds = time.monotonic() - started

    # Capture-then-print (rather than inheriting the parent's streams) so the
    # step log still holds cook's output in full, in order.
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)

    if result.returncode == COOK_SKIPPED_UNCOOKABLE_WORKSPACE:
        print(
            "::error title=soldr cook::COOK_SKIPPED_UNCOOKABLE_WORKSPACE -- "
            f"cook[{args.target}] was skipped because a path dependency cannot "
            "be materialised by the cargo-chef recipe. Exclude the offending "
            "workspace member with `-p` (soldr#3043 step 3) rather than "
            "relaxing this guard. cook's stderr (echoed above) names the "
            "offending dependency."
        )
        return COOK_SKIPPED_UNCOOKABLE_WORKSPACE
    if result.returncode != 0:
        print(
            f"::error title=soldr cook::cook[{args.target}] exited "
            f"{result.returncode}. Read cook's stderr echoed above. If it "
            f"is cargo-chef rejecting one of the --chef-arg values "
            f"({list(chef_args)!r}), the fix belongs in soldr's argv assembly "
            "(build_chef_cook_args must forward them as bare cargo-chef "
            "options, never after a literal `--`), not in a downgrade flag."
        )
        return result.returncode

    outcome, detail = classify(result.stderr)
    warm = outcome in ("hydrated", "warm-skip")

    if outcome == "built-unindexed":
        print(
            f"::error title=soldr cook::COOK_ARTIFACT_NOT_INDEXED -- cook[{args.target}] "
            "built and packed the archive but its CookRecord found no daemon, so "
            "nothing indexes the artifact and no later run can hydrate from it "
            "(soldr#3117). cook's stderr (echoed above) carries the daemon "
            "error. soldr cook holds the daemon route for its whole run "
            "(cook_route_hold.rs); if that hold failed, its warning is echoed "
            "above too."
        )
    elif warm:
        print(f"::notice title=soldr cook::cook[{args.target}] outcome={outcome}")
    elif args.require_warm:
        print(
            f"::error title=soldr cook::--require-warm set and cook[{args.target}] "
            f"outcome={outcome} (neither hydrated nor warm-skip)"
        )
    else:
        print(
            f"::warning title=soldr cook::cook[{args.target}] outcome={outcome} "
            "(neither hydrated nor warm-skip). Not failing by default -- the "
            "acceptance number is only measurable once soldr#3040's analyzer "
            "lands."
        )

    archive_bytes = cook_archive_bytes(Path(args.cache_dir)) if args.cache_dir else None
    lines = report_lines(args.target, outcome, detail, elapsed_seconds, archive_bytes)
    for line in lines:
        print(line)
    append_summary(lines)

    if outcome == "built-unindexed":
        return COOK_ARTIFACT_NOT_INDEXED
    if args.require_warm and not warm:
        return REQUIRE_WARM_FAILURE
    return 0


if __name__ == "__main__":
    sys.exit(main())
