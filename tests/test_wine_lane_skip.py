"""The wine lane skips only when no changed path can reach its test binaries
(soldr#3704). It runs the unit tests of ci/wine_lane.py CRATES; a change that
touches only workspace crates outside their path-dependency closure cannot
change those binaries, so the lane logs an explicit skip. Anything else --
a root file, Cargo.lock, an unknown path, a closure crate -- runs it."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
WINE = load_script_module(ROOT / "ci" / "wine_lane.py", "soldr_wine_lane")


def test_closure_is_the_real_dependency_closure() -> None:
    closure = WINE.dependency_closure(ROOT, WINE.CRATES)
    assert {
        "soldr-platform",
        "soldr-fetch",
        "soldr-nextest-wrapper",
        "soldr-core",
    } <= closure
    assert "soldr-cli" not in closure and "soldr-daemon" not in closure


def test_change_outside_closure_skips() -> None:
    reason = WINE.skip_reason(
        ROOT, ["crates/soldr-cli/src/lib.rs", "crates/soldr-daemon/src/x.rs"]
    )
    assert reason is not None and "soldr-cli" in reason


def test_closure_crate_change_runs() -> None:
    assert (
        WINE.skip_reason(
            ROOT, ["crates/soldr-cli/src/lib.rs", "crates/soldr-core/src/a.rs"]
        )
        is None
    )


def test_root_inputs_and_unknown_paths_run() -> None:
    for path in (
        "Cargo.lock",
        "Cargo.toml",
        "rust-toolchain.toml",
        "ci/wine_lane.py",
        "docker/wine-test/Dockerfile",
        ".cargo/config.toml",
        "crates/nonexistent/src/a.rs",
    ):
        assert WINE.skip_reason(ROOT, [path]) is None, path


def test_unknown_change_set_runs() -> None:
    assert WINE.skip_reason(ROOT, None) is None
    assert WINE.skip_reason(ROOT, []) is None
