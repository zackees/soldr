#!/usr/bin/env python3
"""soldr's local gate (zackees/ci.yml#166, GATE-001..004).

One command that is a superset of the remote quick gate, so a PR passes CI
on its first push:

    uv run --no-project --python 3.13 python ci/local_gate.py             # every lane
    uv run --no-project --python 3.13 python ci/local_gate.py --lane lint # = the remote Lint job
    uv run --no-project --python 3.13 python ci/local_gate.py --list

Do not call this directly before pushing; call it through the attesting
wrapper, which runs it on a clean tree and stamps HEAD with a tree-bound
`Local-Gate:` trailer that CI's `ci-mode` job verifies (see local-gate.toml):

    uvx --from git+https://github.com/zackees/ci.yml@<CI_LINT_REF> ci-lint local-gate run

Lanes:

- `lint`: every check the remote `Lint` job runs. The Lint job runs exactly
  `ci/local_gate.py --lane lint` and nothing else (GATE-001), so this list
  is the single source of truth for it.
- `rust`: `soldr lint rust` on the host -- rustfmt, Clippy for the host and
  every declared target, and every Dylint library. Local only (the remote
  `build-linux-x64` job runs the same stages inside `soldr ci-test`).
- `tests`: soldr's own test suite, in bosn's isolated container
  (`bosn run --task test`), never on the host (GATE-005, soldr#3516).

Native-only lanes (macOS, Windows, linux-arm64 target-runs) cannot run here
and stay remote; they are the residual first-push risk.

Checks in a lane run in parallel; each one's output is shown only when it
fails, then a timing table. Exit 1 when any check fails.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# The zackees/ci.yml commit whose ci_lint this repository uses. ci.yml's
# `ci-mode` job checks out the same SHA; tests/test_local_gate.py keeps the
# two in step.
CI_LINT_REF = "052f27afa101e07d7714d27a1bb98904a26f37ef"
PY = ("uv", "run", "--no-project", "--python", "3.13")
PY_DIRS = (
    "src",
    "tests",
    ".github/scripts",
    ".github/actions",
    "ci",
    "tools",
    ".claude/hooks",
)
RUFF = (*PY, "--with", "ruff>=0.12,<0.13")
PYLINT = (
    *PY,
    "--with",
    "pylint>=4.0,<5",
    "--with",
    "pytest>=8.0",
    "--with",
    "pyyaml>=6,<7",
)


@dataclass(frozen=True)
class Check:
    name: str
    argv: tuple[str, ...]
    lane: str
    # Diff ratchets compare against the PR base: they run locally and on
    # pull_request events, and are skipped on a push to main (nothing to diff).
    needs_base: bool = False
    # Run after every parallel check, alone: `soldr ci-test` saturates every
    # core by itself.
    exclusive: bool = False
    # Started first, so the longest checks overlap the short guards instead
    # of trailing them.
    slow: bool = False
    # Oldest version of argv[0] that runs this check correctly, read from
    # `<tool> --version`; an older tool fails fast with an upgrade hint.
    min_version: tuple[int, ...] | None = None


def _base_ref() -> str:
    base = os.environ.get("GITHUB_BASE_REF") or "main"
    return f"origin/{base}"


def script(name: str, *extra: str) -> tuple[str, ...]:
    return (*PY, *extra, "python", f".github/scripts/{name}")


def checks() -> list[Check]:
    base = _base_ref()
    lint = [
        # Must compare against the MERGE BASE, not the base tip. A branch
        # behind main differs from the tip in files it never touched, so a
        # tip comparison attributes main's own changes to this PR -- caught
        # while testing this step, which flagged a file the PR did not touch.
        #
        # merge-base needs real history, which the default shallow
        # pull_request checkout does not have (the first version of this step
        # skipped itself into uselessness for exactly that reason), hence
        # fetch-depth: 0 on the checkout above.
        Check(
            "Line-ceiling ratchet",
            (*script("loc_ratchet.py"), "--base-ref", base),
            "lint",
            needs_base=True,
        ),
        # soldr#3388 (prevention half of meta soldr#3389) — don't-grow ratchet
        # for swallowed child-process stdio in Python/shell/workflow YAML, plus
        # the check that every `ban_swallowed_child_stdio` allow/expect carries
        # a reason. Same merge-base-ratchet shape as the line-ceiling step
        # above, reusing its `origin/$GITHUB_BASE_REF` fetched by that step.
        Check(
            "Swallowed-stdio ratchet",
            (*script("swallowed_stdio_ratchet.py"), "--base-ref", base),
            "lint",
            needs_base=True,
        ),
        # soldr#2493 — the 1000-line absolute ceiling for production sources.
        # Unlike the 1500-line ratchet above, this is a hard threshold: every
        # production file under crates/*/src must be at or under 1000 lines.
        Check(
            "1000-line production ceiling",
            script("loc_ceiling.py"),
            "lint",
            needs_base=True,
        ),
        # soldr#2753 — the `.proto` files are the documented source of truth for
        # the wire format, but soldr runs no prost-build, so nothing read them
        # and they had drifted: wire.proto referenced a message it never defined
        # (invalid protobuf), and the rust_plan mirror understated its tag space
        # by two fields. Compares (message, field, tag) triples on both sides.
        Check("Protobuf schema drift", script("check_proto_drift.py"), "lint"),
        # soldr#2752 Rule A — the manifest half of the `soldr-deps` gateway,
        # landed first because it needs no facade and no source churn. Adding a
        # third-party dependency now means amending a checked-in inventory in
        # the same PR, so the edge shows up in review instead of as one line in
        # a manifest nobody diffs. Fails in both directions, like loc_ratchet:
        # an unlisted dependency, and a listed one that is gone.
        Check(
            "Third-party dependency inventory",
            script("check_dependency_inventory.py"),
            "lint",
        ),
        # soldr#2937 (phase 5 of soldr#2931) — linked test products are never
        # cacheable. Fails when a normal workflow puts one in a cross-run store,
        # or restores a broad target/ snapshot downstream of cook.
        Check(
            "Cache ownership policy",
            script("check_cache_ownership.py", "--with", "pyyaml"),
            "lint",
        ),
        # soldr#3318: ci.yml is the only PR entry point. Structural YAML parsing
        # rejects both PR event kinds outside this file before any Rust work.
        Check(
            "Single pull-request workflow entry point",
            script("check_pr_workflow_triggers.py", "--with", "pyyaml"),
            "lint",
        ),
        # zackees/ci.yml#6: a cache saved in PR context carries `pr-<N>` in its
        # key so the ci-pre janitor can attribute and delete it.
        Check(
            "PR-context cache keys carry pr-<N>",
            script("check_pr_cache_keys.py", "--with", "pyyaml"),
            "lint",
        ),
        # soldr#2360/#2363 — the broker-fronted daemon design deliberately
        # keeps the wire at protocol_v2/client_v2, broken in place; a
        # protocol_v3/client_v3 module would mean two wire majors coexisting,
        # which defeats the minimum-version floor the design relies on to force
        # upgrades. Cheap static grep, no build required.
        Check("No protocol_v3 policy", script("no_protocol_v3.py"), "lint"),
        # soldr#1981 — `--release` costs thin-LTO + single-CU codegen, and paying
        # that on a job whose output never ships is pure waste. soldr#1982 removed
        # the offenders; this keeps them gone. Runs before the toolchain setup so
        # a regression fails in seconds rather than after a full build.
        Check(
            "Release-profile policy", script("verify_release_profile_policy.py"), "lint"
        ),
        # soldr#2442 slice 4 — tree-level raw process-spawn guard: a new raw
        # `.spawn()` site in any production crate fails here by name until it
        # is routed through a sanctioned module or allowlisted with a
        # justification. Complements the soldr-daemon dylint boundary.
        Check("Spawn-path guard", script("spawn_path_guard.py"), "lint"),
        Check(
            "Nextest bare-Cargo guard", script("check_nextest_bare_cargo.py"), "lint"
        ),
        # soldr#2763 -- the v0.9.3 release died on macOS ARM64 because a release
        # script ran under the runner image's own python3, which predated the
        # 3.10 API it used. The interpreter floor was a per-file convention that
        # nothing enforced. A ratchet, like loc_ratchet: the 23 jobs already in
        # that state are baselined, and no new one may join them.
        #
        # Routed through `uv run` because the guard needs PyYAML, and because a
        # guard about pinning interpreters should not be exempt from its own
        # rule. That is also why it sits *after* the uv setup above rather than
        # with the other guards earlier in the job -- placed before it, the step died on
        # `uv: command not found`.
        Check(
            "Workflow Python interpreter pin",
            script("check_workflow_python_pin.py", "--with", "pyyaml"),
            "lint",
        ),
        # soldr#2945 — the lint libraries are now the authority on Dylint's
        # nightly, but nothing checked that the nightly they declare has drivers
        # published. Soldr fetches a prebuilt driver and refuses to build one
        # (binary-or-exit, #2432/#2484), so re-pinning `dylints/*/rust-
        # toolchain.toml` to an undriven nightly compiles, tests, lints and
        # merges — then breaks Dylint on every host until someone runs it. This
        # asserts the exact `<version>-<nightly>` driver asset exists for every
        # release-included canonical triple, not just the CI host's. Skipped, not
        # failed, when the catalogue is unreachable — same policy as above.
        Check(
            "Dylint driver published for every shipped triple",
            script("check_dylint_driver_assets.py"),
            "lint",
        ),
        # soldr#3284: Dylint remains authoritative but expensive; this cheap
        # source-level tripwire must run even when the ci-test lane is skipped.
        Check(
            "Enforce platform cfg boundary",
            script("platform_cfg_boundary_ratchet.py"),
            "lint",
            slow=True,
        ),
        # Rust compiler validation is centralized in the native host lane.
        # Its single `soldr ci-test` invocation (soldr#2868) leaves this job
        # responsible for non-Rust policy, Python, and JavaScript checks.
        Check(
            "Check npm package metadata",
            ("node", "scripts/test-npm-package.js"),
            "lint",
        ),
        Check(
            "Verify direct CI job timeouts", script("verify_ci_job_timeouts.py"), "lint"
        ),
        # Three lanes assert the glibc floor of a binary soldr builds, and
        # they arrived in three separate PRs with nothing tying them
        # together. `release-auto.yml` is excluded on purpose — its ceiling
        # also covers crgx / cargo-chef, which soldr fetches prebuilt at
        # 2.39, so it legitimately differs (soldr#2170).
        Check(
            "Verify the per-PR glibc ceilings agree",
            script("check_glibc_ceilings.py"),
            "lint",
        ),
        # A `paths:` filter that matches nothing leaves its workflow silently
        # dark: it never runs, so it can never go red. `crates/soldr-cli/src/
        # cache_lib/**` moved to crates/soldr-cache in #1490 Phase 4, and
        # cook-size-gate kept watching the old location.
        # soldr#3018: a `#` inside a folded `run: >` block is not a comment --
        # YAML folds it onto one line and the shell then drops every argument
        # after it. That silently disabled `--grace-seconds 2700` on the macOS
        # queue watchdog, which ran at its 900s default for as long as nobody
        # noticed, and every failure looked like the flaky lane it was watching.
        Check(
            "Verify no folded run block hides arguments behind a comment",
            script("check_run_block_comments.py"),
            "lint",
        ),
        Check(
            "Verify workflow path filters still match",
            script("verify_workflow_paths.py"),
            "lint",
        ),
        Check(
            "Local gate mirrors the remote quick gate (GATE-001/002)",
            (
                "uvx",
                "--from",
                f"git+https://github.com/zackees/ci.yml@{CI_LINT_REF}",
                "--with",
                "pyyaml",
                "ci-lint",
                "local-gate",
                "lint",
                "--repo",
                ".",
            ),
            "lint",
        ),
        # Check mode, not --fix (zackees/ci.yml#166): a gate that rewrote files
        # could not attest the tree it was given (`ci-lint local-gate run`
        # refuses that), and on CI the old `--fix`/format-in-place steps hid
        # unformatted code instead of failing it -- four test files on main
        # were not ruff-formatted when this gate was introduced.
        # soldr#2102. `./lint` runs six Python tools and CI ran none of them, so
        # lint drift accumulated unnoticed for months. flake8 is the one that can
        # be enforced today: with `.flake8` delegating line length to the formatter it
        # reports 0 violations across every Python directory, so this is a
        # regression guard rather than a new policy.
        #
        # isort and mypy still report on main and are NOT wired here --
        # each needs a decision (which width, whether to fix or scope mypy's 34
        # errors). soldr#2102 tracks them. Adding a gate that fails on main would
        # block every PR for a problem it did not cause.
        # ruff was not in CI at all -- it ran only from `./lint`, locally, so
        # nothing enforced it. #2102 widened its rule selection (bugbear and
        # friends) and it passes on `src tests`, so it is enforced here now.
        #
        # `.github/scripts` and `ci` were left out at first because they had 10
        # findings, and fixing them was owed a reviewed diff rather than a
        # drive-by. That diff happened, so the scope is now all four directories
        # and the two that were unenforced are the ones most worth enforcing:
        # they are the scripts CI itself runs.
        # Bounded, like the other four. `ruff>=0.4` was open-ended, so CI
        # resolved a newer ruff than `./install` gives contributors and
        # failed on RUF043 -- a rule that did not exist in the tested
        # version. An unbounded linter is a CI break waiting on someone
        # else's release date.
        Check(
            "Python lint (ruff)", (*RUFF, "ruff", "check", "--no-fix", *PY_DIRS), "lint"
        ),
        # Pinned, not floating. An unpinned `--with flake8` means a future
        # release that adds a check turns this red on code nobody touched,
        # and the failure would look like the PR's fault. `>=7.3,<8` follows
        # the `maturin>=1.7,<2` style already in pyproject: patches flow,
        # a major cannot land unannounced. soldr#2102 covers the wider
        # problem -- isort, pylint and mypy are installed unpinned by
        # `./install` and declared nowhere, which is why contributors format
        # differently from one another.
        Check(
            "Python lint (flake8)",
            (*PY, "--with", "flake8>=7.3,<8", "python", "-m", "flake8", *PY_DIRS),
            "lint",
        ),
        # Ruff's formatter uses Black-compatible style. Keep its width at the
        # previous formatter's 88 columns while Ruff lint retains its own
        # 100-column limit. CI and ./lint use the same bounded Ruff version.
        Check(
            "Python format (ruff)",
            (*RUFF, "ruff", "format", "--check", "--line-length", "88", *PY_DIRS),
            "lint",
        ),
        Check(
            "Python import order (isort)",
            (
                *PY,
                "--with",
                "isort>=6.0,<7",
                "python",
                "-m",
                "isort",
                "--check-only",
                "--profile",
                "black",
                *PY_DIRS,
            ),
            "lint",
        ),
        # mypy over both the shipped package and the tests. `src/` was cleaned
        # in #2107 (four annotation defects, none behavioural); `tests/` is
        # clean as of this change, so the whole tree is checked and this stays a
        # regression guard rather than a new policy.
        # The sixth and last tool from `./lint`, and the one the issue is named
        # for: pylint reported 319 findings, so `./lint` (which runs under
        # `set -e`) could never reach its final line.
        #
        # 315 of the 319 were in `tests/` and were pytest idiom, not defects --
        # fixtures shadowing their own names, tests reaching into the private
        # functions they exist to test. `src/` had four, all deliberate, all now
        # carrying an inline disable that states the reason at the site.
        #
        # So the two surfaces are checked with two rulesets. `src/` runs pylint's
        # DEFAULT configuration -- nothing disabled -- and scores 10.00/10. Only
        # `tests/` gets the relaxations, and they live in `tests/.pylintrc` with
        # a per-entry rationale. Passing that file explicitly rather than relying
        # on pylint's directory discovery means it applies where it is named and
        # nowhere else.
        # pytest in the ephemeral env for the same reason as the mypy step
        # below: 22 test files import it, and pylint reports import-error for
        # what it cannot resolve.
        #
        # pyyaml for the same reason, on the three invocations that reach a
        # file importing it (five under tests/, plus check_workflow_python_pin
        # under .github/scripts). mypy already carries types-PyYAML; pylint
        # needs the real package, because E0401 is about importability rather
        # than typing. This was masked for as long as it existed: the Python
        # tests step runs first, and it had been failing on a collection error
        # since #2823, so pylint never got to report.
        # pylint gets `.github/actions/setup-soldr` rather than the parent
        # `.github/actions`: ensure_soldr.py imports its sibling module, which
        # only resolves when pylint has that leaf directory on its path.
        # Pointed at the parent it reports E0401 for an import that is fine.
        # The other five tools resolve it either way and take the parent.
        # soldr#2120: the CI scripts themselves, on pylint's default rules.
        # Two surfaces here for the same reason src/ and tests/ are split:
        # `test_*.py` in this directory are pytest suites, so they take the
        # tests/ ruleset, and everything else takes the default one.
        Check(
            "Python lint (pylint src)",
            (*PYLINT, "python", "-m", "pylint", "src"),
            "lint",
        ),
        Check(
            "Python lint (pylint tests)",
            (*PYLINT, "python", "-m", "pylint", "--rcfile=tests/.pylintrc", "tests"),
            "lint",
            slow=True,
        ),
        Check(
            "Python lint (pylint CI scripts)",
            (
                *PYLINT,
                "python",
                "-m",
                "pylint",
                ".github/scripts",
                ".github/actions/setup-soldr",
                "ci",
                "tools",
                ".claude/hooks",
                "--ignore-patterns=test_.*\\.py",
            ),
            "lint",
            slow=True,
        ),
        Check(
            "Python lint (pylint CI script tests)",
            (
                *PYLINT,
                "python",
                "-m",
                "pylint",
                "--rcfile=tests/.pylintrc",
                *_script_tests(),
            ),
            "lint",
        ),
        # All four directories. `.github/scripts` and `ci` are the scripts CI
        # itself runs, so a type error in them breaks a lane rather than a
        # developer's editor. Every finding was fixed at the source rather than
        # suppressed (a `None` that could reach `.get`, a list indexed with a
        # str, an over-tight `dict[str, object]` on a `json.loads` payload).
        #
        # `ci/` was held back in #2119 because four of its findings were
        # `fcntl.flock` "has no attribute" -- which looked like a Windows-only
        # artifact that would make the check pass or fail depending on who ran
        # it. It was not: `perf_local.py` already branches correctly at runtime,
        # but on `os.name == "nt"`, and mypy narrows `sys.platform`, not
        # `os.name`. Switching to the idiom mypy understands prunes the dead
        # branch on each platform and the findings disappear on both, so no
        # platform-scoping is needed here after all.
        # pytest is in the ephemeral env because the tests import it, and
        # mypy cannot resolve an import it cannot see. This passed locally
        # without it only because `uv run` picked up the project .venv,
        # which CI does not have -- the same environment-parity trap that
        # bit the flake8 step in #2103.
        # types-PyYAML: tests/test_wheel_lane_policy.py imports yaml (already
        # importable in this env -- the Python tests step passes) but mypy
        # needs the stub package to type it.
        Check(
            "Python types (mypy)",
            (
                *PY,
                "--with",
                "mypy>=1.18,<1.20",
                "--with",
                "pytest>=8.0",
                "--with",
                "types-PyYAML",
                "python",
                "-m",
                "mypy",
                *PY_DIRS,
            ),
            "lint",
            slow=True,
        ),
        # soldr#2013 — nothing ran `tests/` on CI, so it decayed silently: a test
        # asserting on a workflow #1982 deleted, two lints that scanned nothing
        # and reported clean (#2008), and a manifest-coupled assertion (#2004).
        # A deletion PR cannot learn it orphaned a test; a lint cannot learn it
        # stopped scanning. 281 tests, ~15s, and no Rust toolchain needed — so it
        # runs here, before the expensive setup below.
        # `.github/scripts/` is included deliberately: six test files live
        # beside the scripts they cover (target-run summary, the Windows
        # e2e/MSVC-cache policies, catalogued-asset download, nextest fetch,
        # toolchain asset query) and this step used to run `tests/` only, so
        # those 29 tests never executed in CI. Tests that exist but never run
        # are worse than none -- they rot silently while implying coverage.
        Check(
            "Python tests",
            (
                *PY,
                "--with",
                "pytest",
                "--with",
                "pyyaml",
                "python",
                "-m",
                "pytest",
                "tests/",
                ".github/scripts/",
                "-q",
            ),
            "lint",
            slow=True,
        ),
    ]
    # ci-test's lint and dependency-policy stages, command for command
    # (`soldr ci-test --explain-plan`), run by the host's own soldr. Using
    # soldr as a tool on the host is safe; running soldr's *tests* there is
    # not (the `tests` lane, below). Exactly CI's commands, no stricter: a
    # broader `soldr lint deps` (full `cargo deny check`) fails on main for
    # license/advisory policy CI does not enforce, and a gate main cannot
    # pass attests nothing.
    rust = [
        Check("rustfmt", ("soldr", "cargo", "fmt", "--all", "--", "--check"), "rust"),
        Check("soldr lint ci", ("soldr", "lint", "ci"), "rust"),
        Check("cargo deny (bans)", ("soldr", "cargo", "deny", "check", "bans"), "rust"),
        Check("cargo audit", ("soldr", "cargo", "audit"), "rust"),
        Check("cargo machete", ("soldr", "cargo", "machete"), "rust"),
        Check(
            "clippy",
            (
                "soldr",
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ),
            "rust",
            exclusive=True,
        ),
        Check(
            "dylint",
            ("soldr", "cargo", "dylint", "--all", "--", "--workspace", "--all-targets"),
            "rust",
            exclusive=True,
        ),
    ]
    # zackees/ci.yml#168 (GATE-005), soldr#3516: soldr's test suite starts
    # soldr daemons and touches soldr state roots, so it never runs on the
    # developer host -- the nextest run-wrapper refuses unless CI=true or
    # SOLDR_TEST_ISOLATED=1. Locally it runs in bosn's isolated container,
    # whose image sets the marker.
    tests = [
        Check(
            "soldr tests (isolated, bosn)",
            ("bosn", "run", "--task", "test"),
            "tests",
            exclusive=True,
            # zackees/bosn#317: older bosn reaps a task whose output outruns
            # its event queue -- `ci-test`'s warning burst died at 390 s.
            min_version=(0, 1, 5),
        )
    ]
    return lint + rust + tests


def _script_tests() -> tuple[str, ...]:
    return tuple(
        sorted(
            str(p.relative_to(ROOT))
            for p in (ROOT / ".github" / "scripts").glob("test_*.py")
        )
    )


@dataclass(frozen=True)
class Result:
    check: Check
    code: int
    seconds: float
    output: str


def tool_version(tool: str) -> tuple[int, ...] | None:
    proc = subprocess.run(
        [tool, "--version"], capture_output=True, text=True, check=False
    )
    match = re.search(r"(\d+)\.(\d+)\.(\d+)", proc.stdout + proc.stderr)
    return tuple(int(part) for part in match.groups()) if match else None


def _run(check: Check) -> Result:
    start = time.monotonic()
    tool = shutil.which(check.argv[0])
    if tool is None:
        return Result(check, 127, 0.0, f"{check.argv[0]}: not found on PATH")
    if check.min_version is not None:
        found = tool_version(tool)
        if found is None or found < check.min_version:
            want = ".".join(map(str, check.min_version))
            return Result(
                check,
                1,
                0.0,
                f"{check.argv[0]} {found or 'unknown'} is older than {want}; "
                f"upgrade it: uv tool upgrade {check.argv[0]}",
            )
    proc = subprocess.run(
        list(check.argv),
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    return Result(check, proc.returncode, time.monotonic() - start, proc.stdout)


# Paths whose change can affect the `rust` or `tests` lanes. A diff touching
# none of them (Python guards, workflows, docs) skips both lanes locally; the
# remote jobs still run everything, so this only trims the author's loop.
RUST_INPUTS = (
    "crates/",
    "dylints/",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/",
    "clippy.toml",
    "deny.toml",
    ".config/",
    ".github/scripts/nextest",
    "docker/",
    "bosn.toml",
    "tests/fixtures/",
)


def changed_paths() -> list[str] | None:
    """Files this branch changes against its merge base, or None if unknown."""
    base = subprocess.run(
        ["git", "merge-base", _base_ref(), "HEAD"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if base.returncode != 0:
        return None
    diff = subprocess.run(
        ["git", "diff", "--name-only", base.stdout.strip(), "HEAD"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    return diff.stdout.split() if diff.returncode == 0 else None


def _fetch_base() -> None:
    """The diff ratchets need the base branch; fetch it, never fatally."""
    ref = _base_ref().removeprefix("origin/")
    subprocess.run(
        ["git", "fetch", "--no-tags", "--quiet", "origin", ref],
        cwd=ROOT,
        check=False,
        capture_output=True,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--lane", choices=("all", "lint", "rust", "tests"), default="all"
    )
    parser.add_argument("--list", action="store_true", help="print the checks and exit")
    parser.add_argument("--jobs", type=int, default=min(8, os.cpu_count() or 2))
    parser.add_argument(
        "--no-scope",
        action="store_true",
        help="run the rust and tests lanes even when no Rust input changed",
    )
    args = parser.parse_args(argv)

    event = os.environ.get("GITHUB_EVENT_NAME", "")
    selected = [c for c in checks() if args.lane in ("all", c.lane)]
    if event and event != "pull_request":
        selected = [c for c in selected if not c.needs_base]
    if args.list:
        for check in selected:
            print(f"[{check.lane}] {check.name}: {' '.join(check.argv)}")
        return 0
    if any(c.needs_base for c in selected) or args.lane == "all":
        _fetch_base()
    if args.lane == "all" and not args.no_scope:
        changed = changed_paths()
        if changed is not None and not any(p.startswith(RUST_INPUTS) for p in changed):
            skipped = [c for c in selected if c.lane in ("rust", "tests")]
            selected = [c for c in selected if c not in skipped]
            for check in skipped:
                print(f"skip         {check.name} (no Rust input changed)", flush=True)

    start = time.monotonic()
    parallel = sorted(
        (c for c in selected if not c.exclusive), key=lambda c: not c.slow
    )
    results: list[Result] = []
    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        for future in as_completed([pool.submit(_run, c) for c in parallel]):
            result = future.result()
            results.append(result)
            print(
                f"{'ok  ' if result.code == 0 else 'FAIL'} {result.seconds:6.1f}s  {result.check.name}",
                flush=True,
            )
    for check in (c for c in selected if c.exclusive):
        result = _run(check)
        results.append(result)
        print(
            f"{'ok  ' if result.code == 0 else 'FAIL'} {result.seconds:6.1f}s  {check.name}",
            flush=True,
        )

    failed = [r for r in results if r.code != 0]
    for result in failed:
        print(f"\n===== FAIL: {result.check.name} (exit {result.code}) =====")
        print(f"$ {' '.join(result.check.argv)}")
        print(result.output.rstrip()[-20000:])
    total = time.monotonic() - start
    print(
        f"\nlocal gate ({args.lane}): {len(results) - len(failed)}/{len(results)} passed in {total:.0f}s"
    )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
