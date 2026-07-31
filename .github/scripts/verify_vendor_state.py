#!/usr/bin/env python3
"""CI enforcement for `docs/VENDORING.md` discipline.

Three checks gate every soldr PR that touches `_vender/`:

1. If `crates/soldr-cli/Cargo.toml` carries a `path = "_vender/...`
   zccache dep, then `_vender/zccache/.vendor-state` MUST exist.
2. `.vendor-state.deadline` MUST be in the future.
3. Every `[[deltas]]` entry whose `soldr_commit` is older than 7 days
   MUST have a non-null `upstream_pr` URL.

Failing any check fails the build. Bumping the deadline requires
a comment on `driving_issue` explaining why — the script doesn't
enforce *that* part (the meta-issue review does), but the deadline
gate catches the silent-creep case.

Usage:
  python3 .github/scripts/verify_vendor_state.py [--repo-root PATH]

Exit codes:
  0 — all checks pass
  1 — at least one check failed (message printed to stderr)
  2 — script encountered an unexpected error (missing files, etc.)

Run-anywhere: no external deps beyond stdlib + tomllib (Python 3.11+).
Falls back to `tomli` if available for older Pythons.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import re
import subprocess
import sys
from pathlib import Path

try:
    import tomllib as _toml  # 3.11+
except ImportError:  # pragma: no cover — older Pythons
    try:
        import tomli as _toml  # type: ignore[import,no-redef]
    except ImportError:
        sys.stderr.write(
            "verify_vendor_state.py: needs Python 3.11+ (tomllib) "
            "or `pip install tomli`\n"
        )
        sys.exit(2)


VENDOR_STATE_PATH = Path("_vender/zccache/.vendor-state")
CARGO_TOML_PATH = Path("crates/soldr-cli/Cargo.toml")
DELTA_GRACE_DAYS = 7


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path("."),
        help="Path to the soldr repo root (default: current dir).",
    )
    return parser.parse_args()


class VendorStateError(Exception):
    """Unrecoverable problem reading the inputs — exit code 2, never 0.

    Raised rather than returning a default, because every default here
    is a wrong answer: `False` silently disables the gate and `True`
    fails every unrelated PR."""


_DEP_TABLE_KEYS = ("dependencies", "dev-dependencies", "build-dependencies")


def _iter_dep_tables(doc: dict):
    """Every dependency table a zccache dep could legally live in,
    including the `[target.'cfg(...)'.dependencies]` forms."""
    for key in _DEP_TABLE_KEYS:
        table = doc.get(key)
        if isinstance(table, dict):
            yield table
    targets = doc.get("target")
    if isinstance(targets, dict):
        for cfg_table in targets.values():
            if not isinstance(cfg_table, dict):
                continue
            for key in _DEP_TABLE_KEYS:
                table = cfg_table.get(key)
                if isinstance(table, dict):
                    yield table


def _is_vendor_path(path: str) -> bool:
    """True for any relative path that descends through `_vender/`,
    at whatever depth the consuming crate happens to sit."""
    parts = [p for p in path.replace("\\", "/").split("/") if p not in ("", ".")]
    return "_vender" in parts


def soldr_cargo_uses_vendor(cargo_toml_path: Path) -> bool:
    """True when `crates/soldr-cli/Cargo.toml` carries a vendored
    `path = ...` zccache dep.

    Parsed as TOML rather than pattern-matched. The previous regex
    required the literal bytes `path = "_vender/`, so re-spacing the
    dep (`path="../../_vender/...`, a tab, an extra space, or the
    `[dependencies.zccache]` table form) made this return False --
    which skips check 1 and makes the script report "all vendor-state
    checks pass". A formatting change silently disabling a CI gate is
    the worst failure mode available to it, and nothing downstream
    would have noticed."""
    if not cargo_toml_path.is_file():
        return False
    try:
        doc = _toml.loads(cargo_toml_path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise VendorStateError(f"failed to parse {cargo_toml_path}: {exc}") from exc
    for table in _iter_dep_tables(doc):
        spec = table.get("zccache")
        if not isinstance(spec, dict):
            continue
        path = spec.get("path")
        if isinstance(path, str) and _is_vendor_path(path):
            return True
    return False


def load_vendor_state(path: Path) -> dict:
    if not path.is_file():
        return {}
    return _toml.loads(path.read_text(encoding="utf-8"))


def iso_to_dt(value: str) -> _dt.datetime:
    """Parse an ISO-8601 timestamp into an aware UTC datetime."""
    cleaned = value.strip()
    if cleaned.endswith("Z"):
        cleaned = cleaned[:-1] + "+00:00"
    parsed = _dt.datetime.fromisoformat(cleaned)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=_dt.timezone.utc)
    return parsed


def soldr_commit_age_days(repo_root: Path, soldr_commit: str) -> float | None:
    """Days between now and the soldr-side commit that introduced a
    delta. Returns None when the commit is not present locally
    (shallow clone, history rewrite, etc.). Callers treat unknown
    age as `inf days` — pessimistic — so missing history still
    fails the 7-day gate."""
    try:
        out = subprocess.check_output(
            ["git", "-C", str(repo_root), "show", "-s", "--format=%cI", soldr_commit],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except subprocess.CalledProcessError:
        return None
    if not out:
        return None
    committed = iso_to_dt(out)
    now = _dt.datetime.now(_dt.timezone.utc)
    return (now - committed).total_seconds() / 86400.0


def vendor_is_git_submodule(repo_root: Path) -> bool:
    """True when `_vender/zccache` is a registered git submodule.

    Submodules track an external repo by commit pointer (no in-tree
    code copy), so the flat-vendor deadline + delta-tracking discipline
    does not apply — bumping the submodule is a single `git add` of the
    pointer, not a code merge with drift risk. The presence of
    `_vender/zccache` in `.gitmodules` is the signal we use.
    """
    gitmodules = repo_root / ".gitmodules"
    if not gitmodules.is_file():
        return False
    text = gitmodules.read_text(encoding="utf-8")
    # Look for `path = _vender/zccache` (canonical) or whitespace
    # variants. We don't validate the URL since the submodule could
    # legitimately be a fork (forked-during-iteration, etc.).
    return bool(
        re.search(r"^\s*path\s*=\s*_vender/zccache\s*$", text, flags=re.MULTILINE)
    )


def check_vendor_active_means_state_exists(
    cargo_toml_path: Path, vendor_state_path: Path, repo_root: Path
) -> list[str]:
    errors: list[str] = []
    if soldr_cargo_uses_vendor(cargo_toml_path):
        # Submodule path: tracked by commit pointer, no .vendor-state
        # contract applies (per docs/VENDORING.md "Submodule mode").
        if vendor_is_git_submodule(repo_root):
            return errors
        if not vendor_state_path.is_file():
            errors.append(
                f"Check 1 failed: {cargo_toml_path} declares a vendored zccache "
                f"(path = ../../_vender/...) but {vendor_state_path} is missing "
                f"and `_vender/zccache` is not a registered git submodule. "
                f"Either restore the .vendor-state metadata, switch to submodule "
                f"mode (add to .gitmodules), or switch the dep back to a released "
                f"git/crates.io pin."
            )
    return errors


def check_deadline_in_future(state: dict, state_path: Path) -> list[str]:
    errors: list[str] = []
    deadline_raw = state.get("deadline")
    if deadline_raw is None:
        errors.append(
            f"Check 2 failed: {state_path} is missing the `deadline` field. "
            f"Set it to an ISO-8601 UTC timestamp at most 2 weeks from "
            f"`synced_at` (per docs/VENDORING.md)."
        )
        return errors
    if isinstance(deadline_raw, _dt.datetime):
        deadline = deadline_raw
        if deadline.tzinfo is None:
            deadline = deadline.replace(tzinfo=_dt.timezone.utc)
    else:
        try:
            deadline = iso_to_dt(str(deadline_raw))
        except ValueError as exc:
            errors.append(
                f"Check 2 failed: {state_path} `deadline` value "
                f"{deadline_raw!r} is not a valid ISO-8601 timestamp ({exc})."
            )
            return errors
    now = _dt.datetime.now(_dt.timezone.utc)
    if deadline <= now:
        errors.append(
            f"Check 2 failed: {state_path} `deadline` ({deadline.isoformat()}) "
            f"is in the past. Either retire the vendor per docs/VENDORING.md "
            f'"Ending the vendor" or bump the deadline with a comment on '
            f"`driving_issue` explaining why the original target slipped."
        )
    return errors


def check_delta_pr_within_grace(
    state: dict, state_path: Path, repo_root: Path
) -> list[str]:
    errors: list[str] = []
    deltas = state.get("deltas", [])
    if not isinstance(deltas, list):
        errors.append(f"Check 3 failed: {state_path} `deltas` is not an array.")
        return errors
    for i, delta in enumerate(deltas):
        if not isinstance(delta, dict):
            errors.append(f"Check 3 failed: {state_path} `deltas[{i}]` is not a table.")
            continue
        soldr_commit = delta.get("soldr_commit")
        upstream_pr = delta.get("upstream_pr")
        summary = delta.get("summary", "<no summary>")
        if upstream_pr is not None:
            # The fast path — once it's set, it stays set.
            continue
        # upstream_pr is null/missing. Check how long the soldr-side
        # commit has been waiting.
        if not soldr_commit:
            errors.append(
                f"Check 3 failed: {state_path} `deltas[{i}]` ({summary!r}) "
                f"has no `soldr_commit` and no `upstream_pr`. Fill at least "
                f"one within {DELTA_GRACE_DAYS} days of landing."
            )
            continue
        age = soldr_commit_age_days(repo_root, soldr_commit)
        if age is None:
            errors.append(
                f"Check 3 failed: {state_path} `deltas[{i}]` ({summary!r}) "
                f"references soldr commit {soldr_commit} which is not present "
                f"in the local history. Confirm the commit landed and "
                f"populate `upstream_pr` if it was open >{DELTA_GRACE_DAYS} "
                f"days ago."
            )
            continue
        if age > DELTA_GRACE_DAYS:
            errors.append(
                f"Check 3 failed: {state_path} `deltas[{i}]` ({summary!r}) "
                f"references soldr commit {soldr_commit} which landed "
                f"{age:.1f} days ago. `upstream_pr` must be set within "
                f"{DELTA_GRACE_DAYS} days. Open the upstream PR and update "
                f"the entry."
            )
    return errors


def main() -> int:
    args = parse_args()
    repo_root: Path = args.repo_root.resolve()
    cargo_toml_path = repo_root / CARGO_TOML_PATH
    vendor_state_path = repo_root / VENDOR_STATE_PATH

    errors: list[str] = []
    try:
        errors.extend(
            check_vendor_active_means_state_exists(
                cargo_toml_path, vendor_state_path, repo_root
            )
        )
        uses_vendor = soldr_cargo_uses_vendor(cargo_toml_path)
    except VendorStateError as exc:
        sys.stderr.write(f"verify_vendor_state.py: {exc}\n")
        return 2

    # In submodule mode there's no .vendor-state to validate; the
    # submodule pointer is the discipline.
    if uses_vendor and vendor_is_git_submodule(repo_root):
        sys.stdout.write(
            "verify_vendor_state.py: _vender/zccache is a git submodule — "
            "skipping deadline + delta-PR checks (submodule mode)\n"
        )
        return 0

    if vendor_state_path.is_file():
        try:
            state = load_vendor_state(vendor_state_path)
        except Exception as exc:  # pragma: no cover — corrupt TOML
            sys.stderr.write(
                f"verify_vendor_state.py: failed to parse {vendor_state_path}: "
                f"{exc}\n"
            )
            return 2
        errors.extend(check_deadline_in_future(state, vendor_state_path))
        errors.extend(check_delta_pr_within_grace(state, vendor_state_path, repo_root))

    if errors:
        sys.stderr.write("\n".join(errors) + "\n")
        return 1
    sys.stdout.write("verify_vendor_state.py: all vendor-state checks pass\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
