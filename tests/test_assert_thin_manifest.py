"""Tests for the thin-v2 manifest verifier (issue #237).

Covers schema validation, file-presence checks, dropped-category detection,
strict-mode orphan detection, and the CLI subprocess surface. Fixtures are
synthesized inline with ``tempfile`` so the suite never needs to invoke
cargo or soldr.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "assert_thin_manifest.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("assert_thin_manifest", SCRIPT_PATH)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["assert_thin_manifest"] = module
    spec.loader.exec_module(module)
    return module


@pytest.fixture(scope="module")
def mod():
    return _load_module()


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------


def _write_file(path: Path, body: bytes = b"x") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(body)


def _make_manifest(
    bundle_dir: Path,
    files: list[tuple[str, int]],
    *,
    cache_profile: str = "thin-v2",
    schema_version: int = 2,
) -> Path:
    """Write a thin-v2-shaped manifest at ``bundle_dir/manifest.v2.json``.

    ``files`` is a list of ``(rel_posix_path, size_bytes)`` pairs. The
    payload mirrors ``ThinSliceManifest`` in
    ``crates/soldr-cli/src/main.rs``.
    """
    manifest = {
        "schema_version": schema_version,
        "cache_profile": cache_profile,
        "bundle_root": str(bundle_dir),
        "generated_at_unix_seconds": int(time.time()),
        "files": [{"path": rel, "size_bytes": size} for rel, size in files],
    }
    manifest_path = bundle_dir / "manifest.v2.json"
    manifest_path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    return manifest_path


def _populate_bundle(bundle_dir: Path, rel_paths: list[str]) -> None:
    """Create empty placeholder files for each ``rel_path`` under ``bundle_dir``."""
    for rel in rel_paths:
        _write_file(bundle_dir / rel)


# ---------------------------------------------------------------------------
# happy path
# ---------------------------------------------------------------------------


def test_happy_path_matches_bundle(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    files = [
        "debug/.fingerprint/serde-abc/invoked.timestamp",
        "debug/deps/serde-abc.d",
        "debug/build/ring-xyz/output",
    ]
    _populate_bundle(bundle, files)
    manifest_path = _make_manifest(bundle, [(p, 1) for p in files])

    errors = mod.assert_manifest(manifest_path, bundle)
    assert errors == [], errors


def test_happy_path_strict_mode(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    files = [
        "debug/.fingerprint/serde-abc/dep-lib-serde",
        "debug/deps/serde-abc.d",
    ]
    _populate_bundle(bundle, files)
    manifest_path = _make_manifest(bundle, [(p, 1) for p in files])

    errors = mod.assert_manifest(manifest_path, bundle, strict=True)
    assert errors == [], errors


def test_empty_manifest_ok_in_non_strict(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    manifest_path = _make_manifest(bundle, [])

    errors = mod.assert_manifest(manifest_path, bundle)
    assert errors == [], errors


# ---------------------------------------------------------------------------
# negative path: missing / malformed manifest
# ---------------------------------------------------------------------------


def test_missing_manifest_file(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()

    errors = mod.assert_manifest(bundle / "manifest.v2.json", bundle)
    assert errors
    assert any("does not exist" in e for e in errors)


def test_missing_bundle_dir(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    manifest_path = _make_manifest(bundle, [])
    # Now remove the bundle dir to simulate a stale manifest path.
    manifest_path.unlink()
    bundle.rmdir()

    errors = mod.assert_manifest(
        tmp_path / "no-bundle" / "manifest.v2.json", tmp_path / "no-bundle"
    )
    assert errors


def test_invalid_json_manifest(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    manifest_path = bundle / "manifest.v2.json"
    manifest_path.write_text("{not valid json", encoding="utf-8")

    errors = mod.assert_manifest(manifest_path, bundle)
    assert any("not valid JSON" in e for e in errors)


def test_wrong_schema_version(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    manifest_path = _make_manifest(bundle, [], schema_version=1)

    errors = mod.assert_manifest(manifest_path, bundle)
    assert any("schema_version" in e for e in errors)


def test_missing_required_key(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    manifest_path = bundle / "manifest.v2.json"
    manifest_path.write_text(
        json.dumps({"schema_version": 2, "files": []}),
        encoding="utf-8",
    )

    errors = mod.assert_manifest(manifest_path, bundle)
    assert any("missing required key" in e for e in errors)


# ---------------------------------------------------------------------------
# negative path: drift (manifest <-> disk)
# ---------------------------------------------------------------------------


def test_manifest_lists_file_not_on_disk(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    manifest_path = _make_manifest(
        bundle,
        [("debug/deps/serde-abc.d", 1)],
    )
    # Note: no actual file written.

    errors = mod.assert_manifest(manifest_path, bundle)
    assert any("do not exist on disk" in e for e in errors)


def test_strict_mode_catches_orphan(mod, tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    listed = ["debug/deps/serde-abc.d"]
    _populate_bundle(bundle, listed + ["debug/deps/orphan.d"])
    manifest_path = _make_manifest(bundle, [(p, 1) for p in listed])

    errors = mod.assert_manifest(manifest_path, bundle, strict=True)
    assert any("orphan.d" in e for e in errors)
    # And the same fixture should pass without --strict.
    errors_loose = mod.assert_manifest(manifest_path, bundle, strict=False)
    assert errors_loose == [], errors_loose


# ---------------------------------------------------------------------------
# negative path: dropped-category patterns
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "rel_path,label_substr",
    [
        ("debug/incremental/foo.bin", "incremental"),
        ("debug/deps/libserde-abc.rlib", "rlib"),
        ("debug/deps/libserde-abc.rmeta", "rmeta"),
        ("debug/deps/serde-abc.dwo", "dwo"),
        ("debug/deps/soldr.pdb", "pdb"),
        ("debug/deps/soldr.dSYM/Contents/Info.plist", "dSYM"),
        ("debug/build/serde-abc/build-script-build", "Unix"),
        ("debug/build/serde-abc/build-script-build.exe", "Windows"),
        (
            "debug/.fingerprint/serde-abc/serde-abc.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/nested/serde-abc.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/dependency-serde.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/outputting-serde.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/library-serde.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/binary-serde.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/build-script.json",
            "diagnostic JSON",
        ),
        (
            "debug/.fingerprint/serde-abc/run-build-script.json",
            "diagnostic JSON",
        ),
        ("debug/.fingerprint/serde-abc/dep.json", "diagnostic JSON"),
        ("debug/.fingerprint/serde-abc/output.json", "diagnostic JSON"),
        ("debug/.fingerprint/serde-abc/lib.json", "diagnostic JSON"),
        ("debug/.fingerprint/serde-abc/bin.json", "diagnostic JSON"),
    ],
)
def test_dropped_category_triggers_failure(
    mod, tmp_path: Path, rel_path: str, label_substr: str
) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    _populate_bundle(bundle, [rel_path])
    manifest_path = _make_manifest(bundle, [(rel_path, 1)])

    errors = mod.assert_manifest(manifest_path, bundle)
    assert errors, f"expected failure for {rel_path}"
    joined = "\n".join(errors)
    assert "dropped artifact classes" in joined
    assert label_substr in joined


@pytest.mark.parametrize(
    "filename",
    [
        "dep-serde.json",
        "output-serde.json",
        "lib-serde.json",
        "bin-serde.json",
        "build-script-build-script-build.json",
        "run-build-script-build-script-build.json",
        "nested/dep-serde.json",
    ],
)
def test_load_bearing_fingerprint_json_is_allowed(
    mod, tmp_path: Path, filename: str
) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    rel_path = f"debug/.fingerprint/serde-abc/{filename}"
    _populate_bundle(bundle, [rel_path])
    manifest_path = _make_manifest(bundle, [(rel_path, 1)])

    assert mod.assert_manifest(manifest_path, bundle) == []


# ---------------------------------------------------------------------------
# CLI subprocess surface
# ---------------------------------------------------------------------------


def test_cli_exits_zero_on_clean_bundle(tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    files = ["debug/deps/serde-abc.d", "debug/.fingerprint/serde-abc/invoked.timestamp"]
    _populate_bundle(bundle, files)
    manifest_path = bundle / "manifest.v2.json"
    manifest_path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "cache_profile": "thin-v2",
                "bundle_root": str(bundle),
                "generated_at_unix_seconds": 1700000000,
                "files": [{"path": p, "size_bytes": 1} for p in files],
            }
        ),
        encoding="utf-8",
    )

    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), str(manifest_path), str(bundle)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "assert_thin_manifest: OK" in result.stdout


def test_cli_exits_one_on_dropped_category(tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    rlib = "debug/deps/libserde-abc.rlib"
    _populate_bundle(bundle, [rlib])
    manifest_path = bundle / "manifest.v2.json"
    manifest_path.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "cache_profile": "thin-v2",
                "bundle_root": str(bundle),
                "generated_at_unix_seconds": 1700000000,
                "files": [{"path": rlib, "size_bytes": 1}],
            }
        ),
        encoding="utf-8",
    )

    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), str(manifest_path), str(bundle)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 1
    assert "FAIL" in result.stderr
    assert "rlib" in result.stderr
