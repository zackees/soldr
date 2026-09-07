"""Every tracked path must be checkout-able on Windows (soldr#3138).

A path Windows cannot represent does not fail at build time or test time -- it
fails in `actions/checkout`, before a single step of the job runs, on EVERY
Windows lane at once:

    error: invalid path '3.02.'
    The process 'C:\\Program Files\\Git\\bin\\git.exe' failed with exit code 128

That is nine target-run shards plus the PEP 517 smoke going red together, with
an error that names the file but not the rule it broke, in a step nobody
associates with repository contents. It cost a full CI cycle to diagnose once.

The offending file was a zero-byte `3.02.` -- a stray shell-redirect artifact
swept up by `git add -A`. Windows forbids a trailing dot or space in a path
component, forbids `<>:"|?*`, and reserves the legacy DOS device names, so a
file that is unremarkable on Linux can be uncheckoutable on Windows while every
local check stays green.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# Reserved DOS device names: illegal as any path component, with or without an
# extension (`NUL`, `nul.txt`).
DOS_DEVICE_NAMES = {
    "CON",
    "PRN",
    "AUX",
    "NUL",
    *(f"COM{i}" for i in range(1, 10)),
    *(f"LPT{i}" for i in range(1, 10)),
}
ILLEGAL_CHARS = set('<>:"|?*')


def tracked_paths() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [p for p in out.split("\0") if p]


def windows_violation(path: str) -> str | None:
    """Return why *path* cannot be checked out on Windows, or None."""
    for component in path.split("/"):
        if not component:
            continue
        if component != component.rstrip(". "):
            return f"component {component!r} ends with a dot or space"
        bad = sorted(ILLEGAL_CHARS & set(component))
        if bad:
            return f"component {component!r} contains {''.join(bad)!r}"
        if component.split(".")[0].upper() in DOS_DEVICE_NAMES:
            return f"component {component!r} is a reserved DOS device name"
    return None


def test_every_tracked_path_can_be_checked_out_on_windows() -> None:
    violations = [
        f"{path}: {why}"
        for path in tracked_paths()
        if (why := windows_violation(path)) is not None
    ]
    assert not violations, (
        "these tracked paths cannot be checked out on Windows, which fails "
        "actions/checkout on every Windows lane before any step runs:\n  "
        + "\n  ".join(violations)
    )


def test_the_detector_recognises_each_windows_rule() -> None:
    """Pin the rules, so the guard cannot quietly stop detecting anything."""
    assert windows_violation("3.02.") is not None, "trailing dot"
    assert windows_violation("trailing space ") is not None, "trailing space"
    assert windows_violation("dir/nested./file.rs") is not None, "mid-path dot"
    assert windows_violation('has"quote.rs') is not None, "illegal character"
    assert windows_violation("src/NUL") is not None, "reserved device name"
    assert windows_violation("src/nul.txt") is not None, "reserved name with extension"
    # And must not fire on ordinary paths, including dotfiles and versioned dirs.
    for ok in (
        "crates/soldr-cli/src/main.rs",
        ".github/workflows/ci.yml",
        "docs/API.md",
        "dylints/ban_raw_env_flag/Cargo.toml",
        "src/v1.2.3/mod.rs",
    ):
        assert windows_violation(ok) is None, ok
