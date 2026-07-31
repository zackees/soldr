"""The toolchain-home guard must catch a managed-home leak, and only that (soldr#1799).

The bug it guards (#1768): soldr's private managed `RUSTUP_HOME`/`CARGO_HOME`
leaking onto a *host*-resolved tool. Nothing fails — it just flips which rustc
runs, invalidating cargo fingerprints and zccache keys so warm builds silently
recompile everything. The build log records `home_origin` next to the resolved
`binary`, and the invariant is that `managed` is only legitimate when the
binary physically lives inside a managed home.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_toolchain_homes.py"
)

MANAGED = "/home/runner/.soldr"


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_toolchain_homes")


def _log(*rows: "tuple[str, str]") -> str:
    body = "".join(
        f'  <toolchain home_origin="{origin}" binary="{binary}" />\n'
        for origin, binary in rows
    )
    return f"<build>\n{body}</build>\n"


def test_parses_every_toolchain_row(guard):
    rows = guard.parse_rows(
        _log(("caller", "/usr/bin/cargo"), ("managed", f"{MANAGED}/cargo/bin/cargo"))
    )
    assert rows == [
        ("caller", "/usr/bin/cargo"),
        ("managed", f"{MANAGED}/cargo/bin/cargo"),
    ]


def test_a_managed_binary_under_the_managed_root_is_fine(guard):
    rows = guard.parse_rows(_log(("managed", f"{MANAGED}/cargo/bin/cargo")))
    assert guard.violations(rows, [MANAGED]) == []


def test_managed_homes_on_a_host_binary_is_the_leak(guard):
    # The #1768 bug: a host-resolved cargo executing under soldr's managed
    # homes. This is the one thing the guard exists to catch.
    rows = guard.parse_rows(_log(("managed", "/usr/bin/cargo")))
    found = guard.violations(rows, [MANAGED])
    assert len(found) == 1
    assert "/usr/bin/cargo" in found[0]


def test_caller_and_repo_local_are_never_constrained(guard):
    # Both mean the caller's own homes were used; they differ only in
    # reporting, so a host path under either is correct, not a violation.
    rows = guard.parse_rows(
        _log(
            ("caller", "/usr/bin/cargo"), ("repo-local", "/work/proj/.cargo/bin/cargo")
        )
    )
    assert guard.violations(rows, [MANAGED]) == []


def test_an_unknown_origin_does_not_fail_the_build(guard):
    # A newer soldr may add a discriminant. The guard must not redden a build
    # over a value it has not learned.
    rows = guard.parse_rows(_log(("something-new", "/usr/bin/cargo")))
    assert guard.violations(rows, [MANAGED]) == []


def test_windows_extended_length_prefix_is_normalized(guard):
    # Real logs on Windows record the canonicalized path, which carries the
    # `\\?\` prefix; a naive prefix compare would call this a leak.
    rows = guard.parse_rows(
        _log(("managed", r"\\?\C:\Users\me\.soldr\cargo\bin\cargo.exe"))
    )
    assert guard.violations(rows, [r"C:\Users\me\.soldr"]) == []


def test_windows_case_and_separator_differences_are_tolerated(guard):
    rows = guard.parse_rows(
        _log(("managed", r"C:\USERS\Me\.Soldr\cargo\bin\cargo.exe"))
    )
    assert guard.violations(rows, [r"c:/users/me/.soldr"]) == []


def test_a_sibling_directory_is_not_inside_the_root(guard):
    # `/home/runner/.soldr-dev` must not count as inside `/home/runner/.soldr`;
    # a plain `startswith` without the separator would wrongly pass it.
    rows = guard.parse_rows(_log(("managed", "/home/runner/.soldr-dev/x/cargo")))
    assert len(guard.violations(rows, [MANAGED])) == 1


def test_main_fails_on_a_leak_and_names_it(guard, tmp_path, capsys):
    log = tmp_path / "build.xml"
    log.write_text(_log(("managed", "/usr/bin/cargo")), encoding="utf-8")
    assert guard.main([str(log), "--managed-root", MANAGED]) == 1
    out = capsys.readouterr().out
    assert "leaked" in out
    assert "/usr/bin/cargo" in out


def test_main_passes_on_a_clean_log(guard, tmp_path, capsys):
    log = tmp_path / "build.xml"
    log.write_text(_log(("caller", "/usr/bin/cargo")), encoding="utf-8")
    assert guard.main([str(log), "--managed-root", MANAGED]) == 0
    assert "OK" in capsys.readouterr().out


def test_missing_logs_or_roots_do_not_fail_the_build(guard, tmp_path, capsys):
    # Plumbing gaps are not build failures; the guard must not become a
    # mysterious red on its own wiring.
    assert guard.main([str(tmp_path / "nope.xml"), "--managed-root", MANAGED]) == 0
    assert guard.main([str(tmp_path)]) == 0


# --- flip detection (soldr#1799 "flag the known causes") --------------------


def test_repo_key_strips_the_timestamp_prefix(guard):
    assert guard.repo_key("20260731T022249Z-c-users-me-dev-soldr.xml") == (
        "c-users-me-dev-soldr"
    )
    # A name that is not timestamp-prefixed must still yield a stable key
    # rather than losing its first segment.
    assert guard.repo_key("build.xml") == "build"


def test_a_toolchain_change_between_consecutive_builds_is_flagged(guard):
    logs = [
        ("20260101T000000Z-repo-a.xml", [("caller", "/usr/bin/cargo")]),
        ("20260101T000100Z-repo-a.xml", [("managed", "/home/me/.soldr/bin/cargo")]),
    ]
    flips = guard.find_flips(logs)
    assert len(flips) == 1
    assert "home_origin caller -> managed" in flips[0]
    assert "binary /usr/bin/cargo -> /home/me/.soldr/bin/cargo" in flips[0]


def test_a_stable_toolchain_reports_no_flip(guard):
    logs = [
        ("20260101T000000Z-repo-a.xml", [("caller", "/usr/bin/cargo")]),
        ("20260101T000100Z-repo-a.xml", [("caller", "/usr/bin/cargo")]),
    ]
    assert guard.find_flips(logs) == []


def test_different_repositories_are_never_compared(guard):
    # The correctness property. Real logs show repo-local `.cargo/bin/cargo`
    # in one repo and soldr's managed rustup toolchain in another; interleaved
    # builds would otherwise report a flip on every alternation and bury the
    # real signal.
    logs = [
        ("20260101T000000Z-repo-a.xml", [("repo-local", "/a/.cargo/bin/cargo")]),
        ("20260101T000100Z-repo-b.xml", [("managed", "/home/me/.soldr/bin/cargo")]),
        ("20260101T000200Z-repo-a.xml", [("repo-local", "/a/.cargo/bin/cargo")]),
        ("20260101T000300Z-repo-b.xml", [("managed", "/home/me/.soldr/bin/cargo")]),
    ]
    assert guard.find_flips(logs) == []


def test_flips_are_ordered_chronologically_regardless_of_input_order(guard):
    # Filenames carry a UTC timestamp prefix, so sorting by name is
    # chronological; the caller must not have to pre-sort.
    logs = [
        ("20260101T000200Z-repo-a.xml", [("managed", "/m/cargo")]),
        ("20260101T000000Z-repo-a.xml", [("caller", "/usr/bin/cargo")]),
    ]
    flips = guard.find_flips(logs)
    assert len(flips) == 1
    assert "caller -> managed" in flips[0]


def test_report_flips_never_changes_the_exit_code(guard, tmp_path, capsys):
    # Diagnostic, not a gate: a flip is normal when someone deliberately
    # switches toolchains, so it must not redden a build.
    (tmp_path / "20260101T000000Z-repo-a.xml").write_text(
        _log(("caller", "/usr/bin/cargo")), encoding="utf-8"
    )
    (tmp_path / "20260101T000100Z-repo-a.xml").write_text(
        _log(("managed", f"{MANAGED}/bin/cargo")), encoding="utf-8"
    )
    code = guard.main([str(tmp_path), "--managed-root", MANAGED, "--report-flips"])
    assert code == 0
    assert "changed between consecutive builds" in capsys.readouterr().out
