"""Unit coverage for ci/macos_recovery_run.py (soldr#3076/#3078).

The per-PR `e2e-macos-x64` lane in `ci.yml` (via `_ci-target-run.yml`)
executes this script's guest script inside a zackees/docker-mac-x64
Recovery guest and verifies the collected results with it. The guest never
runs under test here -- these pin the script's text, the collected-result
parsing, and the post-collection ownership/coverage verification, which are
the pure/subprocess-only surfaces this module exposes.
"""

import json
import subprocess
from pathlib import Path

from conftest import (
    assert_recovery_verify_collected_contract,
    load_script_module,
    write_collected_recovery_summary,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE = load_script_module(
    REPO_ROOT / "ci" / "macos_recovery_run.py", "macos_recovery_run"
)


def test_build_guest_script_is_posix_sh_and_self_contained() -> None:
    script = MODULE.build_guest_script()
    assert script.startswith("#!/bin/sh")
    assert "fetch soldr /tmp/soldr x" in script
    assert "/tmp/soldr --version" in script
    assert "/tmp/soldr --help" in script
    assert 'exit "$FAIL"' in script


def test_build_guest_script_declares_every_check() -> None:
    """Every name in CHECKS must be either a `fetch NAME` call (whose
    generic `fetch()` helper records `fetch_NAME` dynamically) or a literal
    `record NAME pass` call on its success path."""
    script = MODULE.build_guest_script()
    for name in MODULE.CHECKS:
        if name.startswith("fetch_"):
            file_name = name[len("fetch_") :]
            assert f"fetch {file_name} " in script, name
        else:
            assert f"record {name} pass" in script, name


def test_build_guest_script_replay_stages_are_present() -> None:
    script = MODULE.build_guest_script()
    assert "diskutil eraseDisk APFS Work" in script
    assert "/tmp/work" in script
    assert "nextest list \\" in script
    assert "nextest run $REUSE_ARGS \\" in script
    assert '--extract-to "$WORK/extract"' in script
    assert "--partition hash:1/1" in script
    assert "--no-fail-fast" in script
    assert 'TMPDIR="$WORK/tmp"' in script
    assert 'exec "$@"' in script
    assert "RUSTUP_TOOLCHAIN" in script
    assert "SOLDR_TEST_WORKSPACE_ROOT" in script
    assert "SOLDR_TEST_FIXTURES_DIR" in script
    assert "SOLDR_USE_SYSTEM_CMAKE=1" in script


def test_build_guest_script_is_valid_posix_sh_syntax() -> None:
    """`bash -n` catches gross syntax breakage even though this is /bin/sh."""
    script = MODULE.build_guest_script()
    result = subprocess.run(
        ["bash", "-n", "/dev/stdin"],
        input=script,
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr


def test_parse_summary_splits_status_and_detail() -> None:
    text = "arch=pass:x86_64\nversion=fail:not soldr\n"
    results = MODULE.parse_summary(text)
    assert results["arch"] == (True, "x86_64")
    assert results["version"] == (False, "not soldr")


def test_parse_summary_flags_malformed_lines() -> None:
    results = MODULE.parse_summary("garbage line with no equals\n")
    assert results["summary_line_1"][0] is False


def _passing_summary_lines() -> list[str]:
    return [f"{name}=pass:ok" for name in MODULE.CHECKS]


def test_verify_collected_matches_the_shared_recovery_contract(tmp_path: Path) -> None:
    assert_recovery_verify_collected_contract(
        MODULE, tmp_path, passing_lines=_passing_summary_lines()
    )


def test_main_emit_guest_script_writes_the_output_file(tmp_path: Path) -> None:
    output = tmp_path / "recovery-run.sh"
    rc = MODULE.main(["emit-guest-script", "--output", str(output)])
    assert rc == 0
    assert output.read_text(encoding="utf-8").startswith("#!/bin/sh")


def test_main_verify_collected_delegates_to_verify_collected(tmp_path: Path) -> None:
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )
    rc = MODULE.main(
        [
            "verify-collected",
            "--collected",
            str(collected),
            "--guest-exit-code",
            "0",
        ]
    )
    assert rc == 0


# --------------------------------------------------------------------------
# verify_replay_artifacts / --manifest wiring (soldr#3078)
# --------------------------------------------------------------------------

_TARGET = "x86_64-pc-windows-msvc"

_PACKAGE = "demo"
_BINARY = "native"


def _build_fake_repo_root(tmp_path: Path) -> Path:
    """A self-contained fixture repo root, isolated from the real crates/
    tree -- `validate_source_ownership` scans *every* crate it finds, so
    pointing it at the real `REPO_ROOT` with a manifest that only covers one
    dummy classification fails on every real host-sensitive test source the
    manifest does not mention. Mirrors
    `test_target_run_ownership.test_inverse_guard_requires_explicit_classification_but_not_replay`'s
    fixture shape."""
    root = tmp_path / "fake-repo"
    tests_dir = root / "crates" / _PACKAGE / "tests" / _BINARY
    tests_dir.mkdir(parents=True)
    (tests_dir / "main.rs").write_text("mod process;\n", encoding="utf-8")
    (tests_dir / "process.rs").write_text(
        "#[test]\nfn kills_tree() {}\n", encoding="utf-8"
    )
    return root


def _write_manifest(path: Path) -> None:
    path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "policy_issue": "soldr#2999",
                "source_classifications": [
                    {
                        "id": "demo-native",
                        "package": _PACKAGE,
                        "binary": _BINARY,
                        "disposition": "target-replay",
                        "reason": "test fixture",
                        "modules": ["process"],
                    }
                ],
                "replay_selectors": [
                    {
                        "id": "demo-process-module",
                        "source_id": "demo-native",
                        "test_prefix": "process::",
                        "reason": "test fixture",
                    }
                ],
            }
        ),
        encoding="utf-8",
    )


def _write_all_list(path: Path, *, matched: bool) -> None:
    test_name = "process::kills_tree" if matched else "unrelated::not_owned"
    path.write_text(
        json.dumps(
            {
                "test-count": 1,
                "rust-suites": {
                    "suite-0": {
                        "package-name": _PACKAGE,
                        "binary-name": _BINARY,
                        "testcases": {test_name: {"ignored": False}},
                    }
                },
            }
        ),
        encoding="utf-8",
    )


def _write_list_json(path: Path) -> None:
    path.write_text(
        json.dumps(
            {
                "test-count": 1,
                "rust-suites": {
                    "suite-0": {
                        "package-name": _PACKAGE,
                        "binary-name": _BINARY,
                        "testcases": {"process::kills_tree": {"ignored": False}},
                    }
                },
            }
        ),
        encoding="utf-8",
    )


def _write_junit(path: Path) -> None:
    path.write_text(
        '<?xml version="1.0"?>\n'
        '<testsuites><testsuite name="s" tests="1" failures="0" errors="0" '
        'skipped="0"/></testsuites>\n',
        encoding="utf-8",
    )


def test_verify_replay_artifacts_passes_for_a_consistent_replay(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_all_list(collected / "all-list.json", matched=True)
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    rc = MODULE.verify_replay_artifacts(
        collected, manifest=manifest, repo_root=repo_root, target=_TARGET
    )
    assert rc == 0


def test_verify_replay_artifacts_fails_when_all_list_is_missing(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    try:
        MODULE.verify_replay_artifacts(
            collected, manifest=manifest, repo_root=repo_root, target=_TARGET
        )
        raise AssertionError("expected SystemExit")
    except SystemExit as error:
        assert "all-list.json" in str(error)


def test_verify_replay_artifacts_fails_on_a_stale_selector(tmp_path: Path) -> None:
    """A selector matching zero tests in the guest's own inventory is the
    exact staleness case `build_selection` exists to catch -- verified here
    post-collection since the guest had no inventory to check it against."""
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_all_list(collected / "all-list.json", matched=False)
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    try:
        MODULE.verify_replay_artifacts(
            collected, manifest=manifest, repo_root=repo_root, target=_TARGET
        )
        raise AssertionError("expected SystemExit")
    except SystemExit as error:
        assert "ownership" in str(error)


def test_verify_replay_artifacts_fails_when_junit_is_missing(tmp_path: Path) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = tmp_path / "collected"
    collected.mkdir()
    _write_all_list(collected / "all-list.json", matched=True)
    _write_list_json(collected / "list.json")

    try:
        MODULE.verify_replay_artifacts(
            collected, manifest=manifest, repo_root=repo_root, target=_TARGET
        )
        raise AssertionError("expected SystemExit")
    except SystemExit as error:
        assert "coverage summary" in str(error)


def test_main_verify_collected_runs_replay_artifacts_when_manifest_is_given(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    repo_root = _build_fake_repo_root(tmp_path)
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )
    _write_all_list(collected / "all-list.json", matched=True)
    _write_list_json(collected / "list.json")
    _write_junit(collected / "junit.xml")

    rc = MODULE.main(
        [
            "verify-collected",
            "--collected",
            str(collected),
            "--guest-exit-code",
            "0",
            "--manifest",
            str(manifest),
            "--repo-root",
            str(repo_root),
            "--target",
            _TARGET,
        ]
    )
    assert rc == 0


def test_main_verify_collected_requires_repo_root_and_target_with_manifest(
    tmp_path: Path,
) -> None:
    manifest = tmp_path / "manifest.json"
    _write_manifest(manifest)
    collected = write_collected_recovery_summary(
        tmp_path / "collected", _passing_summary_lines()
    )

    try:
        MODULE.main(
            [
                "verify-collected",
                "--collected",
                str(collected),
                "--guest-exit-code",
                "0",
                "--manifest",
                str(manifest),
            ]
        )
        raise AssertionError("expected SystemExit")
    except SystemExit:
        pass
