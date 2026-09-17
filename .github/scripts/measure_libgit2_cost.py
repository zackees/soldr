#!/usr/bin/env python3
"""Measure libgit2-sys's native-build cost against the soldr#3046 ~30s bar.

soldr#3046 is a phased follow-up to soldr#3042 (which stopped soldr's own
`git2`/`libgit2-sys` dependency from being rebuilt for every ephemeral
`ci-test` root). Phase 5 step 1 -- this script -- is the measuring stick: is
the `libgit2-sys` build-script-plus-native-`cc` cost still under the issue's
own ~30 second bar for a warm `ci-test` run, after that fix landed? Step 2
(workflow/catalogue/forge wiring to actually avoid the cost) is explicitly
out of scope here; this script writes nothing, builds nothing, and only
reads compile journals that already exist on disk.

ATTRIBUTION RULE, AND WHY IT IS `cwd`-ONLY
    A record is attributed to libgit2-sys when the *final path component* of
    its `cwd` matches `libgit2-sys-<version>` (see `LIBGIT2_PKG_RE`). Cargo
    sets a unit's cwd to its package root, so the `libgit2-sys-<version>`
    package root is a reliable, single-purpose marker.

    Matching on `record.args` instead was tried and measured wrong: every
    downstream unit that links against libgit2 carries
    `-L native=<target-dir>/build/libgit2-sys-<hash>/out` on its own command
    line, so an args-based predicate falsely attributed `git2`, `dylint`,
    `dylint_internal`, `dylint_linting`, `dylint_testing`, and all six
    `ban_*` lint cdylibs to libgit2-sys -- 438 records instead of the
    correct 416 on CI run 35257047535. Getting this wrong silently triples
    the answer, which is why the rule is spelled out here rather than
    reconstructed per phase.

TWO COST METRICS, BOTH PRINTED
    The native `cc` units libgit2-sys spawns (one per C source file) run in
    parallel with each other and with everything else cargo is compiling.
    Summing their `latency_ns` therefore answers a different question than
    asking how much *wall-clock* the lane actually spent on them:

    * `wall_seconds` (sum of `latency_ns`) is the compiler-wall cost --
      comparable to the #3039 baseline table's cost column, and the number
      that matters if the units were ever serialized (e.g. by exclusive
      compiler admission).
    * `union_wall_seconds` (union of each record's derived
      `[ts_ns - latency_ns, ts_ns]` interval, via `union_seconds`) is the
      merged elapsed time the lane actually paid, since overlapping `cc`
      units only cost wall-clock once.

    Neither one is "the" answer on its own, so both are always emitted and
    `--bar-metric` picks which one a caller wants to compare against the
    30 second bar.

WHY `journal_paths`, `by_source`, `by_tree` AND `unit_overlap` ARE REPORTED
    A `build-logs-<target>` artifact is not one build's journal. The upload
    globs `cache/zccache/logs/` AND `cache/zccache/history/**/*.jsonl`, and
    `history/` holds one directory per archived build, so a single artifact
    can carry several builds' journals and a naive total then silently adds
    them together.

    That distinction decides the headline reading. Seeing the same unit count
    as both `miss` and `hit` in one artifact has two very different
    explanations: the lane compiled the tree twice (two target trees, or two
    builds archived side by side), or zccache genuinely served every unit it
    had just missed. So the summary carries the evidence needed to tell them
    apart without re-running anything:

    * `journal_paths` -- exactly which files were read, so a guessed artifact
      subdirectory that resolved to zero journals is visible rather than
      reported as a cost of zero.
    * `by_source` -- attributed hit/miss counts per journal file.
    * `by_tree` -- the same split by `analyze_compile_journal`'s target-tree
      classification (`dylint/tests`, `dylint/libraries`, `stable`, ...).
    * `unit_overlap` -- how many distinct units (a native `cc` record's unit
      name is its source-file stem) appear as a miss, as a hit, and as both.
      `both` near zero means two disjoint sets of work; `both` equal to the
      unit count is the "missed then served in the same artifact" reading.

INPUT
    One or more compile-journal roots: a `build-logs-<target>` CI artifact's
    `cache/zccache` tree, or a local `~/.soldr/cache/zccache` directory (or a
    `history/` subdirectory of either, or an individual
    `compile_journal.jsonl[.<stamp>]` file). Resolution and parsing are
    delegated entirely to `analyze_compile_journal.py` -- this script adds
    only the libgit2-sys filter and the cost rollup on top.

    Passing the *root* of a downloaded artifact is the recommended usage:
    discovery walks the whole tree for `compile_journal*.jsonl*`, so there is
    no need to guess the runner-temp subdirectory the journals were archived
    under, and `journal_paths` then reports what it actually found.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import pathlib
import re
import sys
from collections import Counter
from typing import Any

# Loaded with importlib.util.spec_from_file_location rather than a package
# import: this script and analyze_compile_journal.py both live in
# `.github/scripts/`, which is executed as standalone files by CI, not
# imported as a package. A `sys.path.insert` + plain `import` would work too,
# but flake8's E402 (module-level import not at top of file) would then need
# a per-file exemption in `.flake8` -- a file this task does not own.
_ANALYZER_PATH = (
    pathlib.Path(__file__).resolve().with_name("analyze_compile_journal.py")
)
_SPEC = importlib.util.spec_from_file_location(
    "analyze_compile_journal", _ANALYZER_PATH
)
if _SPEC is None or _SPEC.loader is None:  # pragma: no cover - packaging accident
    raise ImportError(f"cannot load {_ANALYZER_PATH}")
analyze_compile_journal = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = analyze_compile_journal
_SPEC.loader.exec_module(analyze_compile_journal)


LIBGIT2_PKG_RE = re.compile(r"^libgit2-sys-[0-9][^/]*$")
DEFAULT_BAR_SECONDS = 30.0
# How many discovered journal paths the text report prints before eliding.
PATHS_SHOWN = 10

# The verdict metrics, declared once. `measure()` validates against this and
# `main()` hands the same tuple to argparse's `choices`, so the API and the
# CLI cannot drift into accepting different sets -- CLAUDE.md's soldr#2945
# rule: when a behaviour has two entry points, either they resolve their
# inputs through one implementation or they drift. Deliberately excludes the
# `*_records` counts in `totals`: comparing a record count against a bar
# measured in seconds is never what a caller means.
BAR_METRICS = (
    "miss_wall_seconds",
    "miss_union_wall_seconds",
    "hit_wall_seconds",
    "hit_union_wall_seconds",
    "all_wall_seconds",
    "all_union_wall_seconds",
)

_KINDS = ("cc", "rustc")
_OUTCOMES = ("hit", "miss")


def is_libgit2_record(record: Any) -> bool:
    """True when `record.cwd`'s final path component is a libgit2-sys root.

    Attribute ONLY on `cwd` -- see the module docstring's ATTRIBUTION RULE
    section for why an `args`-based predicate over-attributes.
    """
    cwd = (record.cwd or "").replace("\\", "/").rstrip("/")
    if not cwd:
        return False
    return LIBGIT2_PKG_RE.match(cwd.rsplit("/", 1)[-1]) is not None


def union_seconds(intervals: list[tuple[int, int]]) -> float:
    """Merge overlapping `[start_ns, end_ns]` intervals; return total seconds.

    Intervals are built from `analyze_compile_journal._interval(record)`,
    which the caller is expected to have already filtered `None` out of.
    """
    if not intervals:
        return 0.0
    ordered = sorted(intervals)
    merged_ns = 0
    current_start, current_end = ordered[0]
    for start, end in ordered[1:]:
        if start <= current_end:
            current_end = max(current_end, end)
        else:
            merged_ns += current_end - current_start
            current_start, current_end = start, end
    merged_ns += current_end - current_start
    return merged_ns / 1e9


def measure(
    paths: list[str | pathlib.Path],
    *,
    dedupe: bool = True,
    bar_seconds: float = DEFAULT_BAR_SECONDS,
    bar_metric: str = "miss_wall_seconds",
) -> dict[str, Any]:
    """Read `paths`, filter to libgit2-sys records, and roll up cost.

    See the module docstring for the JSON schema this returns (kept stable
    on purpose -- soldr#3046's t2-measure-script-test is written against it).
    """
    if bar_metric not in BAR_METRICS:
        # Validated before any I/O: a typo'd metric should not be discovered
        # only after minutes of journal reading.
        raise ValueError(
            f"unknown bar_metric {bar_metric!r}; must be one of {list(BAR_METRICS)}"
        )

    journal_files = analyze_compile_journal.discover_journal_files(paths)
    # `_read_journals` and `_interval` are underscore-prefixed helpers on
    # analyze_compile_journal, reused deliberately: same directory, same
    # owner, and duplicating journal parsing here is exactly the drift
    # CLAUDE.md's soldr#2945 note warns about.
    # pylint: disable-next=protected-access
    records, malformed, deduped = analyze_compile_journal._read_journals(
        journal_files, dedupe
    )

    attributed = [record for record in records if is_libgit2_record(record)]
    package_dirs = sorted(
        {
            (record.cwd or "").replace("\\", "/").rstrip("/").rsplit("/", 1)[-1]
            for record in attributed
        }
    )

    counts: dict[tuple[str, str], int] = {
        (kind, outcome): 0 for kind in _KINDS for outcome in _OUTCOMES
    }
    wall_seconds: dict[tuple[str, str], float] = {
        (kind, outcome): 0.0 for kind in _KINDS for outcome in _OUTCOMES
    }
    intervals: dict[tuple[str, str], list[tuple[int, int]]] = {
        (kind, outcome): [] for kind in _KINDS for outcome in _OUTCOMES
    }
    miss_reasons: Counter[str] = Counter()
    by_source: dict[str, dict[str, int]] = {}
    by_tree: dict[str, dict[str, int]] = {}
    units: dict[str, set[str]] = {"hit": set(), "miss": set()}

    for record in attributed:
        # Only `hit`/`miss` participate in by_kind/totals; any other outcome
        # (e.g. `link_miss`) is still counted in `attributed_records` above
        # but excluded here -- it is neither a cache hit nor a compile in
        # the strict sense the two cost metrics are answering for.
        if record.outcome not in _OUTCOMES:
            continue
        # pylint: disable-next=protected-access
        is_cc = analyze_compile_journal._is_native_compiler(record.compiler)
        key = ("cc" if is_cc else "rustc", record.outcome)
        counts[key] += 1
        wall_seconds[key] += (record.latency_ns or 0) / 1e9
        # pylint: disable-next=protected-access
        interval = analyze_compile_journal._interval(record)
        if interval is not None:
            intervals[key].append(interval)
        if record.outcome == "miss" and record.miss_reason:
            miss_reasons[record.miss_reason] += 1
        # Provenance, so a total that spans two archived builds cannot be
        # mistaken for one build's cost -- see the module docstring.
        by_source.setdefault(record.source, {"hit": 0, "miss": 0})[record.outcome] += 1
        by_tree.setdefault(record.tree, {"hit": 0, "miss": 0})[record.outcome] += 1
        units[record.outcome].add(record.crate_name)

    by_kind: dict[str, dict[str, dict[str, float]]] = {}
    for kind in _KINDS:
        by_kind[kind] = {}
        for outcome in _OUTCOMES:
            key = (kind, outcome)
            by_kind[kind][outcome] = {
                "count": counts[key],
                "wall_seconds": wall_seconds[key],
                "union_wall_seconds": union_seconds(intervals[key]),
            }

    miss_records = counts[("cc", "miss")] + counts[("rustc", "miss")]
    hit_records = counts[("cc", "hit")] + counts[("rustc", "hit")]
    miss_wall = wall_seconds[("cc", "miss")] + wall_seconds[("rustc", "miss")]
    hit_wall = wall_seconds[("cc", "hit")] + wall_seconds[("rustc", "hit")]
    miss_intervals = intervals[("cc", "miss")] + intervals[("rustc", "miss")]
    hit_intervals = intervals[("cc", "hit")] + intervals[("rustc", "hit")]

    totals = {
        "miss_records": miss_records,
        "hit_records": hit_records,
        "miss_wall_seconds": miss_wall,
        "miss_union_wall_seconds": union_seconds(miss_intervals),
        "hit_wall_seconds": hit_wall,
        "hit_union_wall_seconds": union_seconds(hit_intervals),
        "all_wall_seconds": miss_wall + hit_wall,
        "all_union_wall_seconds": union_seconds(miss_intervals + hit_intervals),
    }

    cc_records = counts[("cc", "hit")] + counts[("cc", "miss")]
    data_available = bool(attributed) and cc_records > 0
    measured_seconds = float(totals[bar_metric])
    under_bar = data_available and measured_seconds <= bar_seconds

    return {
        "journal_files": len(journal_files),
        "journal_paths": [str(path) for path in journal_files],
        "records_total": len(records),
        "records_malformed": malformed,
        "records_deduped": deduped,
        "attributed_records": len(attributed),
        "package_dirs": package_dirs,
        "by_kind": by_kind,
        "by_source": {source: by_source[source] for source in sorted(by_source)},
        "by_tree": {tree: by_tree[tree] for tree in sorted(by_tree)},
        "unit_overlap": {
            "miss_units": len(units["miss"]),
            "hit_units": len(units["hit"]),
            "both": len(units["miss"] & units["hit"]),
        },
        "miss_reasons": dict(sorted(miss_reasons.items())),
        "totals": totals,
        "verdict": {
            "bar_seconds": bar_seconds,
            "metric": bar_metric,
            "measured_seconds": measured_seconds,
            "under_bar": under_bar,
            "data_available": data_available,
        },
    }


def render_text(summary: dict[str, Any]) -> str:
    """Render `summary` (the dict `measure()` returns) as a console report."""
    lines: list[str] = []
    lines.append("== libgit2-sys cost measurement (soldr#3046 step 1) ==")
    lines.append(
        f"journals: {summary['journal_files']} files, "
        f"{summary['records_total']} records read, "
        f"{summary['records_malformed']} malformed, "
        f"{summary['records_deduped']} deduped"
    )
    # Capped, because a `history/` tree carries one journal per archived
    # build and the full list can run to dozens. The JSON payload always
    # carries every path; this is the console courtesy copy.
    shown = summary["journal_paths"][:PATHS_SHOWN]
    for path in shown:
        lines.append(f"  journal: {path}")
    remaining = len(summary["journal_paths"]) - len(shown)
    if remaining > 0:
        lines.append(f"  journal: (+{remaining} more, see --json)")
    lines.append(f"attributed records: {summary['attributed_records']}")
    if summary["package_dirs"]:
        lines.append(f"package dirs: {', '.join(summary['package_dirs'])}")
    else:
        lines.append("package dirs: (none found)")

    lines.append("")
    lines.append("kind/outcome  count  wall_s  union_wall_s")
    for kind in _KINDS:
        for outcome in _OUTCOMES:
            bucket = summary["by_kind"][kind][outcome]
            lines.append(
                f"{kind}/{outcome:<4}  {bucket['count']:>5}  "
                f"{bucket['wall_seconds']:>7.2f}  {bucket['union_wall_seconds']:>7.2f}"
            )

    lines.append("")
    lines.append("attributed hit/miss by target tree:")
    if summary["by_tree"]:
        for tree, split in summary["by_tree"].items():
            lines.append(f"  {tree}: {split['miss']} miss, {split['hit']} hit")
    else:
        lines.append("  (none)")

    lines.append("")
    lines.append("attributed hit/miss by journal file:")
    if summary["by_source"]:
        for source, split in summary["by_source"].items():
            lines.append(f"  {source}: {split['miss']} miss, {split['hit']} hit")
    else:
        lines.append("  (none)")

    overlap = summary["unit_overlap"]
    lines.append("")
    lines.append(
        f"distinct units: {overlap['miss_units']} missed, "
        f"{overlap['hit_units']} hit, {overlap['both']} both"
    )
    if overlap["both"]:
        # The reading that decides whether a catalogue asset is even the
        # right fix: a unit that is missed AND served inside one measurement
        # is cacheable already, so the gap is cross-run reuse, not vendoring.
        lines.append(
            "  NOTE: units in BOTH columns were compiled and also served from "
            "cache within this measurement -- confirm from the two breakdowns "
            "above whether that is one build, or several archived builds "
            "summed together."
        )

    lines.append("")
    lines.append("miss reasons (attributed miss records):")
    if summary["miss_reasons"]:
        for reason, count in summary["miss_reasons"].items():
            lines.append(f"  {reason}: {count}")
    else:
        lines.append("  (none)")

    totals = summary["totals"]
    lines.append("")
    lines.append("totals:")
    lines.append(
        f"  miss: {totals['miss_records']} records, "
        f"wall={totals['miss_wall_seconds']:.2f}s "
        f"union={totals['miss_union_wall_seconds']:.2f}s"
    )
    lines.append(
        f"  hit:  {totals['hit_records']} records, "
        f"wall={totals['hit_wall_seconds']:.2f}s "
        f"union={totals['hit_union_wall_seconds']:.2f}s"
    )
    lines.append(
        f"  all:  wall={totals['all_wall_seconds']:.2f}s "
        f"union={totals['all_union_wall_seconds']:.2f}s"
    )

    verdict = summary["verdict"]
    lines.append("")
    if not verdict["data_available"]:
        lines.append(
            "!! NO libgit2-sys native `cc` records found in this journal -- "
            "the measurement is UNAVAILABLE, not zero. A journal from a host "
            "that never built the dylint tree looks identical to a cost of "
            "zero; do not read the absence of records as a passing score."
        )
    verdict_word = (
        "UNAVAILABLE"
        if not verdict["data_available"]
        else ("UNDER BAR" if verdict["under_bar"] else "OVER BAR")
    )
    lines.append(
        f"verdict: {verdict['metric']} = {verdict['measured_seconds']:.2f}s "
        f"vs bar {verdict['bar_seconds']:.2f}s -> {verdict_word}"
    )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    """CLI entry point. Always returns 0 -- this is a report, not a gate.

    A pass/fail ratchet on this measurement belongs in a script shaped like
    `check_third_party_compiles.py`, not here: soldr#3046 step 1 is only
    asking "what is the number", not "fail CI when the number is bad".
    """
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "paths", nargs="+", help="compile-journal files or directories to scan"
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print the summary as JSON instead of a table",
    )
    parser.add_argument(
        "--json-out", default=None, help="also write the summary JSON to this file"
    )
    parser.add_argument(
        "--bar-seconds",
        type=float,
        default=DEFAULT_BAR_SECONDS,
        help=f"the cost bar in seconds (default {DEFAULT_BAR_SECONDS})",
    )
    parser.add_argument(
        "--bar-metric",
        choices=BAR_METRICS,
        default="miss_wall_seconds",
        help="which totals field the verdict is measured against",
    )
    parser.add_argument(
        "--no-dedupe",
        action="store_true",
        help="do not drop byte-identical journal lines across files",
    )
    args = parser.parse_args(argv)

    summary = measure(
        args.paths,
        dedupe=not args.no_dedupe,
        bar_seconds=args.bar_seconds,
        bar_metric=args.bar_metric,
    )

    if args.json_out:
        # Report-only: a failure to write the artifact must not turn a
        # computed-but-unpublished result into a traceback.
        out_path = pathlib.Path(args.json_out)
        try:
            out_path.parent.mkdir(parents=True, exist_ok=True)
            out_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        except OSError as error:
            print(
                f"measure_libgit2_cost: could not write --json-out "
                f"{args.json_out}: {error}",
                file=sys.stderr,
            )

    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        print(render_text(summary))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
