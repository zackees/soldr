"""The repository Actions-cache budget guard (soldr#3047 Phase B).

`check_cache_budget.py` groups every live GitHub Actions cache entry into the
family whose `key_prefixes` is the longest match, then fails when an entry
matches no family, when a family exceeds its own allocation, or when the
total exceeds the hard ceiling.

`test_captured_fixture_is_red` is the acceptance item for soldr#3047: the real
`gh cache list` snapshot that motivated this guard (44.23 GiB across 143
entries) must fail it. Everything else is built from small synthetic
manifests/listings so each failure mode is exercised in isolation, in the same
style as `tests/test_cache_ownership.py`.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
_SCRIPT = REPO_ROOT / ".github" / "scripts" / "check_cache_budget.py"
guard = load_script_module(_SCRIPT, "check_cache_budget")

MANIFEST = REPO_ROOT / "ci" / "cache-ownership.json"
FIXTURE = REPO_ROOT / "tests" / "fixtures" / "actions-cache" / "listing-2026-09-01.json"


def write_manifest(tmp_path: Path, budget: dict) -> Path:
    path = tmp_path / "cache-ownership.json"
    path.write_text(json.dumps({"budget": budget}), encoding="utf-8")
    return path


def write_listing(
    tmp_path: Path, entries: list[dict], name: str = "listing.json"
) -> Path:
    path = tmp_path / name
    path.write_text(
        json.dumps({"captured": "test", "usage_bytes": None, "entries": entries}),
        encoding="utf-8",
    )
    return path


def family(prefix: str, max_bytes: int) -> dict:
    return {
        "key_prefixes": [prefix],
        "max_bytes": max_bytes,
        "entries": [],
        "rationale": "fixture",
    }


def entry(key: str, size_bytes: int, ref: str = "refs/heads/main") -> dict:
    return {"key": key, "ref": ref, "sizeInBytes": size_bytes}


# --------------------------------------------------------------------------
# Acceptance: the real 2026-09-01 listing must be RED
# --------------------------------------------------------------------------


def test_captured_fixture_is_red(capsys: pytest.CaptureFixture[str]) -> None:
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(FIXTURE)])
    assert code == 1
    out = capsys.readouterr().out
    assert "v0-rust-cross-build" in out


# --------------------------------------------------------------------------
# A synthetic listing built from the real manifest
# --------------------------------------------------------------------------


def test_half_budget_synthetic_listing_from_real_manifest_passes(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    families = manifest["budget"]["families"]

    entries = []
    for spec in families.values():
        prefix = spec["key_prefixes"][0]
        entries.append(entry(f"{prefix}half-budget-fixture", spec["max_bytes"] // 2))

    listing_path = write_listing(tmp_path, entries)
    code = guard.main(["--manifest", str(MANIFEST), "--from-json", str(listing_path)])
    assert code == 0, capsys.readouterr().out


# --------------------------------------------------------------------------
# Synthetic failures, one rule at a time
# --------------------------------------------------------------------------


def test_unregistered_key_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 1000,
            "fail_total_bytes": 1000,
            "families": {"fam-a": family("a-", 500)},
        },
    )
    listing_path = write_listing(tmp_path, [entry("totally-unregistered-key-zzz", 10)])

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "totally-unregistered-key-zzz" in out


def test_family_over_its_own_budget_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 1000,
            "fail_total_bytes": 1000,
            "families": {"fam-a": family("a-", 100)},
        },
    )
    listing_path = write_listing(tmp_path, [entry("a-1", 150)])

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "fam-a" in out


def test_families_under_but_total_over_fail_total_bytes_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    manifest_path = write_manifest(
        tmp_path,
        {
            "total_max_bytes": 2000,
            "fail_total_bytes": 1000,
            "families": {
                "fam-a": family("a-", 1000),
                "fam-b": family("b-", 1000),
            },
        },
    )
    listing_path = write_listing(
        tmp_path,
        [entry("a-1", 600), entry("b-1", 600)],
    )

    code = guard.main(
        ["--manifest", str(manifest_path), "--from-json", str(listing_path)]
    )
    assert code == 1
    out = capsys.readouterr().out
    assert "fail_total_bytes" in out


# --------------------------------------------------------------------------
# The real manifest's budget object must agree with itself
# --------------------------------------------------------------------------


def test_manifest_budget_is_self_consistent() -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    budget = manifest["budget"]
    families = budget["families"]

    assert (
        sum(spec["max_bytes"] for spec in families.values())
        == budget["total_max_bytes"]
    )
    assert budget["total_max_bytes"] == 9663676416

    owned_prefixes = [
        (prefix, family_id)
        for family_id, spec in families.items()
        for prefix in spec["key_prefixes"]
    ]
    for prefix_a, family_a in owned_prefixes:
        for prefix_b, family_b in owned_prefixes:
            if family_a == family_b:
                continue
            assert not prefix_b.startswith(
                prefix_a
            ), f"{prefix_a!r} ({family_a}) is a prefix of {prefix_b!r} ({family_b})"


# --------------------------------------------------------------------------
# Network policy: skip, never fail, when gh is unavailable
# --------------------------------------------------------------------------


def test_live_mode_skips_when_gh_is_unavailable(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def boom(_args: list[str]) -> str:
        raise FileNotFoundError("gh")

    monkeypatch.setattr(guard, "run_gh", boom)

    code = guard.main(["--manifest", str(MANIFEST)])
    assert code == 0
    assert "skipped" in capsys.readouterr().out
