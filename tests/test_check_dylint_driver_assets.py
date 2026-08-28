"""Regression coverage for the Dylint-driver catalogue guard."""

from __future__ import annotations

import sys
from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / ".github" / "scripts"
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))
GUARD = load_script_module(
    ROOT / ".github" / "scripts" / "check_dylint_driver_assets.py",
    "check_dylint_driver_assets",
)


def entry(asset: str) -> dict:
    return {
        "owner": "zackees",
        "repo": "soldr-toolchain",
        "tag": "assets",
        "asset": asset,
        "sha256": "0" * 64,
        "size_bytes": 1,
        "urls": ["https://example.invalid/asset"],
        "parts": None,
    }


def test_every_released_target_is_covered_by_the_live_contract() -> None:
    required = GUARD.required_assets(ROOT)
    assert len(required) == 8
    assert all("x86_64-pc-windows-gnu" not in asset for asset in required)


def test_missing_target_is_reported_deterministically() -> None:
    required = GUARD.required_assets(ROOT)
    omitted = next(asset for asset in required if "aarch64-pc-windows-msvc" in asset)
    payload = {
        "schema_version": 2,
        "entries": [entry(asset) for asset in required if asset != omitted],
    }
    assert GUARD.missing_assets(payload, required) == [omitted]


def test_complete_fixture_passes() -> None:
    required = GUARD.required_assets(ROOT)
    payload = {"schema_version": 2, "entries": [entry(asset) for asset in required]}
    assert GUARD.missing_assets(payload, required) == []


def test_direct_transport_with_an_empty_inactive_parts_list_is_missing() -> None:
    required = GUARD.required_assets(ROOT)
    direct = next(iter(required))
    direct_entry = entry(direct)
    direct_entry["parts"] = []
    assert GUARD.missing_assets({"schema_version": 2, "entries": [direct_entry]}, {direct}) == [
        direct
    ]


def test_multipart_transport_with_an_empty_inactive_urls_list_is_missing() -> None:
    required = GUARD.required_assets(ROOT)
    multipart = next(iter(required))
    multipart_entry = entry(multipart)
    multipart_entry.update(
        {
            "urls": [],
            "parts": [
                {
                    "number": 1,
                    "size_bytes": 1,
                    "sha256": "0" * 64,
                    "urls": ["https://example.invalid/part"],
                }
            ],
            "min_client_version": 2,
            "source_path": "dylint/driver.tar.gz",
        }
    )
    assert GUARD.missing_assets(
        {"schema_version": 2, "entries": [multipart_entry]}, {multipart}
    ) == [multipart]


def test_multipart_transport_with_a_boolean_part_number_is_missing() -> None:
    required = GUARD.required_assets(ROOT)
    multipart = next(iter(required))
    multipart_entry = entry(multipart)
    multipart_entry.update(
        {
            "urls": None,
            "parts": [
                {
                    "number": True,
                    "size_bytes": 1,
                    "sha256": "0" * 64,
                    "urls": ["https://example.invalid/part"],
                }
            ],
            "min_client_version": 2,
            "source_path": "dylint/driver.tar.gz",
        }
    )
    assert GUARD.missing_assets(
        {"schema_version": 2, "entries": [multipart_entry]}, {multipart}
    ) == [multipart]


def test_malformed_transport_does_not_count_as_a_published_asset() -> None:
    required = GUARD.required_assets(ROOT)
    malformed = next(iter(required))
    bad_entry = entry(malformed)
    bad_entry["urls"] = "https://example.invalid/not-a-list"
    payload = {"schema_version": 2, "entries": [bad_entry]}
    assert GUARD.missing_assets(payload, {malformed}) == [malformed]


def test_malformed_transport_url_does_not_count_as_a_published_asset() -> None:
    required = GUARD.required_assets(ROOT)
    malformed = next(iter(required))
    bad_entry = entry(malformed)
    bad_entry["urls"] = ["https://"]
    payload = {"schema_version": 2, "entries": [bad_entry]}
    assert GUARD.missing_assets(payload, {malformed}) == [malformed]
