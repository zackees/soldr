#!/usr/bin/env python3
"""Report-only ratchet on third-party compile cost (zackees/soldr#3039).

soldr#3039 is a multi-phase effort to cut how much of a soldr build
recompiles third-party crates that a working cache should have served from
history. soldr#3040 ("measure before you optimize") lands the analyzer
first (`analyze_compile_journal.py`) and this ratchet on top of it, so every
later phase of #3039 is judged against a number this script already prints
today, rather than a number invented for the PR that claims the win.

THE THRESHOLDS START DELIBERATELY UNREACHABLE
    `--max-misses` and `--max-dirty-third-party` are wired into CI at values
    far above anything a real build produces. That is not a mistake: no
    #3039 phase has landed yet, so a reachable threshold would go red on the
    very PR that adds this script, before any optimization exists to hold
    it. Each phase of #3039 LOWERS its threshold in its own PR -- that lower
    number is how a phase proves the win it claims, and it is also what
    catches a later regression once the phase is real.

METRIC DEFINITIONS (OWNED BY analyze_compile_journal.py)
    Restated here only so a threshold reads beside the number it gates. If
    the two descriptions ever disagree, that module is right and this one
    is stale -- the definitions are not re-derived here, only quoted.

    * A third-party COMPILE is a third-party compile-journal record with
      outcome `miss` (the strict definition `analyze_compile_journal.py`
      documents as `cost.third_party_miss_records` -- a `hit` is not a
      compile, and `link_miss` is a separate, cheaper event that does not
      spend this budget). This script prefers that strict count when the
      analyzed summary carries a `cost` block, and falls back to the
      broader `third_party.misses` (`miss | link_miss`) grouping only when
      it does not -- e.g. a hand-written `--summary-json` from an older
      report step. Which definition a given run used is always printed
      alongside the number, so a later phase never has to guess.
    * A DIRTY THIRD-PARTY UNIT is a cargo `fingerprint dirty for <crate>
      v<version> ...` log line whose crate name is not a workspace member or
      a `dylints/*` lint library. `CARGO_LOG=cargo::core::compiler::
      fingerprint=info` is what produces those lines, and it is exported by
      `.github/workflows/_build-and-test.yml`, **not** by the `ci-test`
      verb -- nothing in `crates/` sets `CARGO_LOG`, so a local
      `soldr ci-test` emits no dirty lines unless you export it yourself.
      Cargo's `Fresh` status is NOT observable at that log level either (it
      needs `-v`, which the host validation run does not pass), so a run
      with no cargo log at all skips this half of the gate rather than
      silently reporting a false zero.

WHAT THIS DOES NOT DO
    This does not compute anything itself. It loads the summary
    `analyze_compile_journal.analyze()` already produces (or a
    `--summary-json` a prior report step already wrote) and compares two
    numbers in it against two thresholds. All the parsing, classification,
    and derivation lives in that module; duplicating any of it here is
    exactly the drift CLAUDE.md's soldr#2945 note warns about.

UNREADABLE INPUT IS A WIRING PROBLEM, NOT A BUILD FAILURE
    An unreadable/malformed `--summary-json`, or one that is not the JSON
    object this script expects, prints a diagnostic and then falls back to
    the positional journal paths -- the CI report step is
    `continue-on-error: true`, so it can die before writing its file while
    the journals are still on disk, and skipping exactly the run that went
    wrong is the opposite of what a gate is for. That fallback is a real
    evaluation and CAN exit 1 on a genuine breach.

    Only when there is nothing left to read -- no usable summary AND no
    journals discovered -- does this print "nothing to check" and return 0
    without calling `evaluate()`. A CI plumbing accident must never be
    indistinguishable from a real regression, so a threshold is never
    applied to numbers that were never measured.

USAGE
    python3 .github/scripts/check_third_party_compiles.py \\
        ~/.soldr/cache/zccache/history/ --cargo-logs build.log \\
        --lockfiles-from-repo --max-misses 100000 --max-dirty-third-party 100000
    # or gate on a summary a report step already computed:
    python3 .github/scripts/check_third_party_compiles.py \\
        --summary-json logs/compile-journal-analysis.json \\
        --max-misses 100000 --max-dirty-third-party 100000
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import pathlib
import sys

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


def _as_dict(value: object) -> dict:
    return value if isinstance(value, dict) else {}


def _as_int(value: object, default: int = 0) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else default


def _fmt_bucket(value: object) -> str:
    return "n/a" if value is None else str(value)


def evaluate(
    summary: dict, max_misses: int, max_dirty: int
) -> "tuple[bool, list[str]]":
    """Compare an analyzer summary against the two ratchet thresholds.

    Returns `(ok, lines)`: `ok` is True only when every threshold that was
    actually evaluated holds; `lines` is the full set of printable lines,
    in the order a reader should see them (numbers first, then a FAIL
    summary and top dirty reasons when `ok` is False).

    The dirty-unit check is skipped -- not failed -- whenever
    `cargo.available` is falsy: with no cargo fingerprint log captured
    there is nothing to ratchet, and treating "nothing captured" as
    "nothing dirty" would silently pass a lane that has less signal, not
    more third-party caching.
    """
    lines: list[str] = []

    cost = _as_dict(summary.get("cost"))
    if isinstance(cost.get("third_party_miss_records"), int) and not isinstance(
        cost.get("third_party_miss_records"), bool
    ):
        actual_misses = cost["third_party_miss_records"]
        miss_label = "outcome==miss"
    else:
        third_party = _as_dict(summary.get("third_party"))
        actual_misses = _as_int(third_party.get("misses"))
        miss_label = "miss|link_miss fallback"

    misses_ok = actual_misses <= max_misses
    lines.append(
        f"check_third_party_compiles: third-party misses {actual_misses} "
        f"(max {max_misses}, {miss_label}) — {'OK' if misses_ok else 'FAIL'}"
    )

    cargo = _as_dict(summary.get("cargo"))
    dirty_ok = True
    if cargo.get("available"):
        dirty_units = _as_int(cargo.get("third_party_dirty_units"))
        dirty_ok = dirty_units <= max_dirty
        lines.append(
            f"check_third_party_compiles: third-party dirty units {dirty_units} "
            f"(max {max_dirty}) — {'OK' if dirty_ok else 'FAIL'}"
        )
    else:
        lines.append(
            "check_third_party_compiles: third-party dirty units check skipped "
            "— no cargo fingerprint log was captured. "
            "CARGO_LOG=cargo::core::compiler::fingerprint=info is what produces "
            "the `fingerprint dirty for` lines this check reads; cargo's `Fresh` "
            "status is not observable at that log level."
        )

    buckets = _as_dict(summary.get("buckets"))
    lines.append(
        "check_third_party_compiles: fresh="
        f"{_fmt_bucket(buckets.get('fresh'))} "
        f"hit={_fmt_bucket(buckets.get('compiling_hit'))} "
        f"miss={_fmt_bucket(buckets.get('compiling_miss'))} "
        f"other={_fmt_bucket(buckets.get('compiling_other'))} "
        f"no-record={_fmt_bucket(buckets.get('compiling_no_record'))}"
    )

    ok = misses_ok and dirty_ok
    if not ok:
        lines.append(
            "check_third_party_compiles: FAIL — a third-party compile ratchet "
            "was exceeded. See zackees/soldr#3039."
        )
        dirty_reasons = _as_dict(cargo.get("dirty_reasons"))
        int_reasons = {
            reason: count
            for reason, count in dirty_reasons.items()
            if isinstance(count, int)
        }
        if int_reasons:
            lines.append("check_third_party_compiles: top dirty reasons:")
            top = sorted(int_reasons.items(), key=lambda item: (-item[1], item[0]))[:5]
            for reason, count in top:
                lines.append(f"  - {reason}: {count}")

    return ok, lines


def _load_summary_json(path: str) -> "tuple[dict | None, str | None]":
    """Read `path` as a JSON summary object.

    Returns `(summary, error_line)`. Exactly one of the two is non-None: a
    read/parse failure or a non-object payload produces an `error_line`
    describing why the file was not usable, never an exception.
    """
    try:
        text = pathlib.Path(path).read_text(encoding="utf-8")
        loaded = json.loads(text)
    except (OSError, ValueError) as error:
        # ValueError covers both `json.JSONDecodeError` and the
        # `UnicodeDecodeError` a truncated/binary artifact raises from
        # `read_text` -- neither is an `OSError`, and both are exactly the
        # kind of unreadable input this guard must never crash on.
        return (
            None,
            f"check_third_party_compiles: could not read --summary-json {path}: {error}",
        )
    if not isinstance(loaded, dict):
        return (
            None,
            f"check_third_party_compiles: --summary-json {path} is not a JSON "
            "object; ignoring it",
        )
    return loaded, None


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "paths", nargs="*", help="compile-journal files or directories to scan"
    )
    parser.add_argument(
        "--cargo-logs",
        action="append",
        default=[],
        help="cargo/nextest build-output file or directory (repeatable)",
    )
    parser.add_argument(
        "--lockfile",
        action="append",
        default=[],
        help="Cargo.lock file to join against (repeatable)",
    )
    parser.add_argument(
        "--lockfiles-from-repo",
        action="store_true",
        help="add <repo-root>/Cargo.lock and every dylints/*/Cargo.lock that exists",
    )
    parser.add_argument(
        "--repo-root",
        default=None,
        help="repository root (default: this script's repo)",
    )
    parser.add_argument(
        "--max-misses",
        type=int,
        required=True,
        help="max allowed third-party compile misses",
    )
    parser.add_argument(
        "--max-dirty-third-party",
        type=int,
        required=True,
        help="max allowed third-party cargo dirty units",
    )
    parser.add_argument(
        "--summary-json",
        default=None,
        help="load an already-computed analyzer summary instead of scanning journals",
    )
    parser.add_argument(
        "--json-out",
        default=None,
        help="write the summary that was evaluated to this file",
    )
    args = parser.parse_args(argv)

    summary: dict | None = None
    if args.summary_json:
        summary, error_line = _load_summary_json(args.summary_json)
        if error_line:
            print(error_line)

    if summary is None:
        journal_files = analyze_compile_journal.discover_journal_files(args.paths)
        if not journal_files:
            print(
                "check_third_party_compiles: nothing to check — no compile "
                f"journals found under {args.paths!r} and no usable "
                "--summary-json was given. This is a wiring problem, not a "
                "build failure."
            )
            return 0

        repo_root = pathlib.Path(args.repo_root) if args.repo_root else None
        lockfiles: list[str | pathlib.Path] = list(args.lockfile)
        if args.lockfiles_from_repo:
            lockfiles.extend(analyze_compile_journal.repo_lockfiles(repo_root))

        summary = analyze_compile_journal.analyze(
            args.paths,
            cargo_log_paths=args.cargo_logs,
            repo_root=repo_root,
            lockfiles=lockfiles,
            dedupe=True,
        )

    if args.json_out:
        # Report-only: never let a failure to write the artifact turn a
        # computed-but-unpublished result into a traceback.
        out_path = pathlib.Path(args.json_out)
        try:
            out_path.parent.mkdir(parents=True, exist_ok=True)
            out_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        except OSError as error:
            print(
                f"check_third_party_compiles: could not write --json-out "
                f"{args.json_out}: {error}"
            )

    assert (
        summary is not None
    )  # either loaded from --summary-json or just computed above
    ok, lines = evaluate(summary, args.max_misses, args.max_dirty_third_party)
    for line in lines:
        print(line)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
