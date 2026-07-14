from __future__ import annotations

import importlib.util
from pathlib import Path


SCRIPT = Path(__file__).with_name("windows_e2e_policy.py")
SPEC = importlib.util.spec_from_file_location("windows_e2e_policy", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
policy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(policy)


def test_pushes_always_run_windows_e2e() -> None:
    decision = policy.decide_windows_e2e(
        event_name="push",
        labels=["fast-build"],
        changed_paths=["docs/diagram.png"],
    )
    assert decision.run is True
    assert "push" in decision.reason


def test_unlabelled_pull_request_runs_windows_e2e() -> None:
    decision = policy.decide_windows_e2e(
        event_name="pull_request",
        labels=[],
        changed_paths=["docs/diagram.png"],
    )
    assert decision.run is True
    assert "fast-build" in decision.reason


def test_fast_build_skips_docs_and_repository_metadata_only() -> None:
    decision = policy.decide_windows_e2e(
        event_name="pull_request",
        labels=["fast-build"],
        changed_paths=[
            "README.md",
            "docs/ci/windows-cache.svg",
            ".github/ISSUE_TEMPLATE/bug.yml",
        ],
    )
    assert decision.run is False
    assert "low-risk" in decision.reason


def test_fast_build_cannot_skip_windows_sensitive_code() -> None:
    representative_paths = [
        "crates/soldr-cli/src/blessed_build.rs",
        "crates/soldr-cache/src/cache.rs",
        ".github/workflows/_ci-cross-build-linux.yml",
        ".github/scripts/windows_msvc_cache_roundtrip.py",
        "Cargo.toml",
        "rust-toolchain.toml",
    ]
    for changed_path in representative_paths:
        decision = policy.decide_windows_e2e(
            event_name="pull_request",
            labels=["fast-build"],
            changed_paths=["docs/context.md", changed_path],
        )
        assert decision.run is True, changed_path
        assert changed_path in decision.reason


def test_empty_or_unclassifiable_diff_fails_safe() -> None:
    decision = policy.decide_windows_e2e(
        event_name="pull_request",
        labels=["fast-build"],
        changed_paths=[],
    )
    assert decision.run is True
    assert "empty" in decision.reason
