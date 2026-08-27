#!/usr/bin/env python3
"""Tree-level raw process-spawn guard (soldr#2442 slice 4).

The broker-fronted design's containment story depends on knowing every place
soldr creates a child process: the broker is the sole daemon placer, the cargo
front door owns the single sanctioned broker spawn, and everything else is a
tool child (cargo, rustup, installers) that must stay inside its documented
module. The `ban_raw_process_creation` dylint enforces the strict rule inside
`crates/soldr-daemon` (spawns only via `running_process::spawn`); this guard
extends *visibility* to every production crate: a new raw `.spawn()` call site
anywhere under `crates/*/src` fails Lint by name until it is either routed
through an existing sanctioned module or added to the allowlist below with a
justification — the same reviewed-decision property as running-process's own
`ci/spawn_path_guard.py`.

Counting is per file rather than per line so refactors inside an allowlisted
module do not churn the manifest; adding a spawn to a *new* file, or growing a
file's count, is exactly the event that needs review.

Test-only code is out of scope: `*_tests.rs` siblings, `tests/` directories,
and dylint `ui/` fixtures are skipped.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CRATES = REPO_ROOT / "crates"

# `.spawn()` with empty parens is the process-spawn signature:
# `std::process::Command::spawn` and tokio's process spawn take no
# arguments, while `thread::spawn(closure)` / `tokio::spawn(future)` do.
SPAWN = re.compile(r"\.spawn\(\)")

# path (repo-relative, forward slashes) -> (expected count, justification).
# Growing a count or adding a file is a reviewed decision (soldr#2442
# slice 4); shrinking is always allowed and should update the entry.
ALLOWLIST: dict[str, tuple[int, str]] = {
    # Note: the sanctioned front-door broker spawn (broker_spawn.rs) and
    # every soldr-daemon child go through running_process::spawn* and are
    # deliberately NOT in this list — the guard exists so raw spawns cannot
    # appear beside them.
    "crates/soldr-cli/src/cargo_front_door/debug_trace.rs": (
        2,
        "the front door's single spawn choke point (soldr#2546 "
        "spawn_traced, enabled + disabled arms): every cargo child in "
        "output_capture / history_and_timeout routes through it",
    ),
    "crates/soldr-cli/src/cargo_front_door/subcommand_bootstrap.rs": (
        1,
        "bootstrap re-exec of the resolved cargo subcommand binary",
    ),
    "crates/soldr-cli/src/fetch_overlap.rs": (
        1,
        "soldr#1543 dependency prefetch (cargo fetch), sequenced after "
        "toolchain provisioning by soldr#2618",
    ),
    "crates/soldr-cli/src/lint_cmd.rs": (
        1,
        "parallel lint-deps children (soldr re-exec per dependency tool)",
    ),
    "crates/soldr-cli/src/ci_test/execute.rs": (
        1,
        "prescribed ci-test DAG children; every command is a frozen soldr "
        "re-exec and sibling process trees are cancelled on failure",
    ),
    "crates/soldr-cli/src/dylint_driver.rs": (
        1,
        "dylint driver bootstrap through the managed nightly toolchain "
        "(moved out of dylint_toolchain.rs by the soldr#2945 line-ceiling "
        "split; same single version-probe child, unchanged)",
    ),
    "crates/soldr-cli/src/gc/holding_process.rs": (
        1,
        "gc probe child that holds a target/ handle to test liveness",
    ),
    "crates/soldr-cli/src/shim_materialize.rs": (
        1,
        "shim self-verification child (spawns the materialized shim once)",
    ),
    "crates/soldr-core/src/core/installer_watchdog.rs": (
        2,
        "watchdog-supervised installer children (rustup/toolchain installs)",
    ),
    "crates/soldr-core/src/core/mod.rs": (
        1,
        "detached self-relocation helper",
    ),
    "crates/soldr-cache/src/cache_lib/build_active.rs": (
        1,
        "build-activity lease keeper child",
    ),
    "crates/soldr-platform/src/platform_linux/process/spawn.rs": (
        1,
        "the platform boundary's own spawn primitive (linux)",
    ),
    "crates/soldr-platform/src/platform_macos/process/spawn.rs": (
        1,
        "the platform boundary's own spawn primitive (macos)",
    ),
    "crates/soldr-platform/src/platform_win/process/spawn.rs": (
        1,
        "the platform boundary's own spawn primitive (windows)",
    ),
    "crates/soldr-platform/src/platform_linux/process/terminate.rs": (
        1,
        "process-group terminator helper child (linux)",
    ),
    "crates/soldr-daemon/src/daemon/server_runtime.rs": (
        1,
        "false positive: tokio-console subscriber Builder::spawn(), not a "
        "process (the dylint enforces the real rule in this crate)",
    ),
}


def is_production_source(path: Path) -> bool:
    rel = path.relative_to(REPO_ROOT).as_posix()
    if "/src/" not in f"/{rel}":
        return False
    if rel.endswith("_tests.rs") or rel.endswith("/tests.rs"):
        return False
    parts = rel.split("/")
    return "tests" not in parts and "ui" not in parts


def spawn_counts() -> dict[str, int]:
    counts: dict[str, int] = {}
    for path in sorted(CRATES.glob("*/src/**/*.rs")):
        if not is_production_source(path):
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        count = sum(1 for _ in SPAWN.finditer(text))
        if count:
            counts[path.relative_to(REPO_ROOT).as_posix()] = count
    return counts


def violations(counts: dict[str, int]) -> list[str]:
    problems = []
    for rel, count in sorted(counts.items()):
        expected = ALLOWLIST.get(rel)
        if expected is None:
            problems.append(
                f"{rel}: {count} raw .spawn() call(s) in a file with no "
                "allowlist entry — route through a sanctioned module or add "
                "a justified entry (soldr#2442 slice 4)"
            )
        elif count > expected[0]:
            problems.append(
                f"{rel}: raw .spawn() count grew {expected[0]} -> {count} — "
                "a new spawn site needs review (soldr#2442 slice 4)"
            )
    for rel, (allowed, _) in sorted(ALLOWLIST.items()):
        actual = counts.get(rel, 0)
        if actual < allowed:
            problems.append(
                f"{rel}: allowlist expects {allowed} spawn(s) but found "
                f"{actual} — shrink/remove the entry so it cannot mask a "
                "future addition"
            )
    return problems


def main() -> int:
    counts = spawn_counts()
    problems = violations(counts)
    if problems:
        print("spawn-path guard failed:", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    total = sum(counts.values())
    print(
        f"spawn-path guard passed: {total} raw spawn site(s) across "
        f"{len(counts)} allowlisted file(s)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
