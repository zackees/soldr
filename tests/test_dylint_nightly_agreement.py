"""Every Dylint nightly pin in the tree must agree (soldr#2817).

soldr fetches a **prebuilt** Dylint driver keyed on `<dylint-version>-<nightly>`
and refuses to build one from source:

    Dylint v6.0.3 is not built for this machine (host: x86_64-unknown-linux-gnu;
    missing or unusable component: dylint-driver for nightly-2026-04-16).
    Soldr will not build Dylint from source.

So a file that pins its own nightly does not merely get a different compiler —
it gets *no driver at all*. `ci/fixtures/dylint-cache` was on
`nightly-2026-04-16` while every real dylint had moved to `nightly-2026-05-28`,
and the Dylint Cache and Cook acceptance lanes failed every scheduled run for
three weeks. Both are scheduled-only, so no PR ever ran them.

This is a source check rather than a lane check for that reason: the lanes that
would have caught it are exactly the ones nobody watches.
"""

from __future__ import annotations

import re
from pathlib import Path

from conftest import (
    DYLINT_BUILD_STEPS,
    DYLINT_NIGHTLY,
    DYLINT_TEST_STEPS,
    load_script_module,
    workflow_step,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
CHANNEL = re.compile(r'^\s*channel\s*=\s*"([^"]+)"', re.MULTILINE)
TARGET_GUARD = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "verify_dylint_target_dirs.py",
    "verify_dylint_target_dirs",
)


def pinned_channel(path: Path) -> str | None:
    match = CHANNEL.search(path.read_text(encoding="utf-8"))
    return match.group(1) if match else None


def dylint_toolchain_files() -> list[Path]:
    """Every first-party Dylint crate's pin, plus the acceptance fixture.

    Vendored copies under a crate's own `.cargo/registry` are excluded — they
    are upstream dependencies' files, not pins this repo controls.
    """
    found = [
        path
        for path in sorted((REPO_ROOT / "dylints").glob("*/rust-toolchain.toml"))
        if ".cargo" not in path.parts
    ]
    fixture = REPO_ROOT / "ci" / "fixtures" / "dylint-cache" / "rust-toolchain.toml"
    if fixture.is_file():
        found.append(fixture)
    return found


def test_the_scan_finds_the_files_it_is_meant_to_guard():
    """A guard that scans nothing reports clean (soldr#2013)."""
    files = dylint_toolchain_files()
    assert len(files) >= 7, f"expected the six dylints plus the fixture, got {files}"
    assert any("fixtures" in str(p) for p in files), "the fixture must be covered"


def test_every_dylint_nightly_pin_agrees():
    pins = {path: pinned_channel(path) for path in dylint_toolchain_files()}
    missing = [str(p) for p, c in pins.items() if c is None]
    assert not missing, f"no channel pinned in: {missing}"

    distinct = sorted(set(pins.values()))
    assert len(distinct) == 1, (
        "Dylint nightly pins disagree, so at least one has no prebuilt driver:\n"
        + "\n".join(
            f"  {path.relative_to(REPO_ROOT).as_posix()}: {channel}"
            for path, channel in sorted(pins.items())
        )
    )


def test_the_ci_dylint_toolchain_matches_the_pins():
    """ci.yml installs the nightly the driver is published for.

    If the workflow and the pins disagree, the lane prepares one driver and the
    build asks for another — which is the same failure one level up.
    """
    pins = {c for c in (pinned_channel(p) for p in dylint_toolchain_files()) if c}
    assert len(pins) == 1
    pinned = pins.pop()

    ci = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert pinned in ci, (
        f"ci.yml does not mention {pinned}; the Dylint steps install a nightly "
        "that the dylint crates do not pin"
    )


def test_ci_dylint_build_and_test_steps_use_one_soldr_cargo_style():
    """The nightly env chooses the toolchain; every lint uses Soldr's cargo front door."""
    ci = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    for name in DYLINT_BUILD_STEPS:
        step = workflow_step(ci, name)
        assert f"RUSTUP_TOOLCHAIN: {DYLINT_NIGHTLY}" in step
        assert "soldr cargo build" in step
        assert "soldr rustup run" not in step
    for name in DYLINT_TEST_STEPS:
        step = workflow_step(ci, name)
        assert f"RUSTUP_TOOLCHAIN: {DYLINT_NIGHTLY}" in step
        assert "soldr cargo test" in step
        assert "soldr rustup run" not in step


def test_ci_dylint_tests_share_one_nightly_keyed_target_directory():
    """Six standalone manifests share test deps without clobbering driver cdylibs."""
    ci = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    shared = f'"${{GITHUB_WORKSPACE}}/target/dylint/tests/{DYLINT_NIGHTLY}"'
    for name in DYLINT_TEST_STEPS:
        step = workflow_step(ci, name)
        assert "--target-dir" in step
        assert shared in step
    assert "verify_dylint_target_dirs.py" in ci


def test_implicit_dylint_target_guard_reports_each_lint_target(tmp_path: Path):
    (tmp_path / "dylints" / "one" / "target").mkdir(parents=True)
    (tmp_path / "dylints" / "two" / "target").mkdir(parents=True)
    (tmp_path / "dylints" / "three").mkdir(parents=True)

    assert TARGET_GUARD.implicit_dylint_target_dirs(tmp_path) == [
        tmp_path / "dylints" / "one" / "target",
        tmp_path / "dylints" / "two" / "target",
    ]
