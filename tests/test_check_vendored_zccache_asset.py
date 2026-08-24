"""The vendored-zccache asset guard must catch a pin that leads the release.

soldr#2164 is the incident: the pin moved to a version with no published
release, every local signal stayed green because none of them exercise that
fetch, and `main` went red. This guard is the pre-flight CLAUDE.md documents,
run by CI rather than from memory.

The failure directions are asymmetric and both matter. Missing a bad pin means
the incident repeats on `main`. Firing on a good one — or on a GitHub Pages
outage — means a guard on every PR that people learn to ignore, which is how
the next bad pin gets waved through.

No test here touches the network: the manifest is a fixture, so these assert on
selection logic rather than on what happens to be published today.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPTS_DIR = Path(__file__).resolve().parents[1] / ".github" / "scripts"
# The guard imports `toolchain_asset_query` as a sibling, which works when it is
# run as a script (Python adds its own directory) but not when `conftest` loads
# it as a module. The unusual caller does the setup, so the script can keep the
# same plain import its siblings use.
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_vendored_zccache_asset.py"
)


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_vendored_zccache_asset")


def platform_entry(os_key: str, arch: str, **rest) -> dict:
    """One manifest platform row, in the shape `toolchain_asset_query` expects."""
    return {
        "platform": {"os": os_key, "arch": arch, **rest},
        "asset": {
            "filename": f"zccache-{os_key}-{arch}.tar.gz",
            "urls": [f"https://example.invalid/zccache-{os_key}-{arch}.tar.gz"],
            "sha256": "0" * 64,
        },
    }


def complete_platforms() -> list[dict]:
    """Every platform `cross-compile-all-targets.yml` asks for."""
    return [
        platform_entry("linux", "x86_64", libc="musl"),
        platform_entry("linux", "aarch64", libc="musl"),
        platform_entry("darwin", "x86_64"),
        platform_entry("darwin", "aarch64"),
        platform_entry("windows", "x86_64", abi="msvc"),
        platform_entry("windows", "aarch64", abi="msvc"),
    ]


def manifest(version: str, platforms: list[dict]) -> dict:
    return {"releases": [{"version": version, "platforms": platforms}]}


# ------------------------------- reading versions ------------------------------


def write_repo(tmp_path: Path, vendored: str | None, locked: str | None) -> Path:
    if vendored is not None:
        manifest_path = tmp_path / "_vender" / "zccache" / "Cargo.toml"
        manifest_path.parent.mkdir(parents=True, exist_ok=True)
        manifest_path.write_text(
            f'[package]\nname = "zccache"\nversion = "{vendored}"\n',
            encoding="utf-8",
        )
    if locked is not None:
        (tmp_path / "Cargo.lock").write_text(
            f'[[package]]\nname = "zccache"\nversion = "{locked}"\n',
            encoding="utf-8",
        )
    return tmp_path


def test_vendored_version_is_read_from_the_submodule_manifest(guard, tmp_path):
    write_repo(tmp_path, "1.13.5", None)
    assert guard.vendored_version(tmp_path) == "1.13.5"


def test_the_first_version_key_wins_like_the_workflows_sed(guard, tmp_path):
    """The workflow does `sed ... | head -n1`; a workspace manifest can carry
    more than one `version =`, and disagreeing with CI here would make the
    guard check a version release staging never requests."""
    path = tmp_path / "_vender" / "zccache" / "Cargo.toml"
    path.parent.mkdir(parents=True)
    path.write_text(
        '[package]\nname = "zccache"\nversion = "1.13.5"\n\n'
        '[dependencies.serde]\nversion = "1.0.200"\n',
        encoding="utf-8",
    )
    assert guard.vendored_version(tmp_path) == "1.13.5"


def test_an_absent_submodule_reads_as_none(guard, tmp_path):
    assert guard.vendored_version(tmp_path) is None


def test_locked_version_is_read_from_cargo_lock(guard, tmp_path):
    write_repo(tmp_path, "1.13.5", "1.13.5")
    assert guard.locked_version(tmp_path) == "1.13.5"


def test_locked_version_does_not_match_a_different_package(guard, tmp_path):
    (tmp_path / "Cargo.lock").write_text(
        '[[package]]\nname = "zccache-depgraph"\nversion = "9.9.9"\n',
        encoding="utf-8",
    )
    assert guard.locked_version(tmp_path) is None


# ------------------------------ platform coverage ------------------------------


def test_a_complete_release_reports_nothing_missing(guard):
    payload = manifest("1.13.5", complete_platforms())
    assert guard.missing_platforms(payload, "1.13.5") == []


def test_a_partial_release_names_exactly_what_is_absent(guard):
    """The linux/musl row alone is what CLAUDE.md's example query checks.

    A guard that checked only that row would pass this manifest while five of
    the six release targets have nothing to download.
    """
    payload = manifest("1.13.5", [platform_entry("linux", "x86_64", libc="musl")])
    missing = guard.missing_platforms(payload, "1.13.5")
    assert ("linux", "x86", "musl") not in missing
    assert len(missing) == 5
    assert ("windows", "arm", "msvc") in missing
    assert ("mac", "x86", None) in missing


def test_an_asset_without_a_digest_does_not_count(guard):
    """Release staging verifies sha256, so an entry it cannot verify is not a
    usable asset — accepting it here would pass a pin that fails on main."""
    platforms = complete_platforms()
    del platforms[0]["asset"]["sha256"]
    payload = manifest("1.13.5", platforms)
    missing = guard.missing_platforms(payload, "1.13.5")
    assert missing == [
        ("linux", "x86", "musl")
    ], f"only the digest-less row should be missing, got {missing}"


def test_an_unpublished_version_raises_rather_than_reporting_gaps(guard):
    """Absent version and partial platforms have different remedies — publish a
    release, versus publish more assets — so they must not collapse together."""
    payload = manifest("1.13.5", complete_platforms())
    with pytest.raises(SystemExit):
        guard.missing_platforms(payload, "1.14.0")


# --------------------------------- exit codes ----------------------------------


def test_a_version_lock_disagreement_fails(guard, tmp_path, monkeypatch, capsys):
    write_repo(tmp_path, "1.14.0", "1.13.5")
    monkeypatch.setattr("sys.argv", ["check", "--repo-root", str(tmp_path)])
    assert guard.main() == 1
    out = capsys.readouterr().out
    assert "1.14.0" in out and "1.13.5" in out
    assert "Cargo.lock" in out


def test_a_missing_submodule_fails_with_the_init_command(
    guard, tmp_path, monkeypatch, capsys
):
    monkeypatch.setattr("sys.argv", ["check", "--repo-root", str(tmp_path)])
    assert guard.main() == 1
    out = capsys.readouterr().out
    assert "git submodule update --init _vender/zccache" in out


def test_an_unreachable_manifest_is_skipped_not_failed(
    guard, tmp_path, monkeypatch, capsys
):
    """Every PR runs this. Failing them all on a Pages blip teaches people to
    ignore it, which costs more than the check is worth."""
    write_repo(tmp_path, "1.13.5", "1.13.5")
    monkeypatch.setattr(
        "sys.argv",
        [
            "check",
            "--repo-root",
            str(tmp_path),
            "--origin",
            "http://127.0.0.1:1/unreachable",
        ],
    )
    assert guard.main() == 0
    assert "skipped" in capsys.readouterr().out


def serve(guard, monkeypatch, payload: dict) -> None:
    """Answer the guard's manifest fetch from a fixture instead of the network."""

    class Response:
        def __enter__(self):
            return self

        def __exit__(self, *_):
            return False

        def read(self):
            import json as _json

            return _json.dumps(payload).encode("utf-8")

    monkeypatch.setattr(guard.urllib.request, "urlopen", lambda *a, **k: Response())


def test_the_2164_incident_fails_the_guard(guard, tmp_path, monkeypatch, capsys):
    """The pin leads the release: vendored 1.14.0, nothing published for it.

    This is the case that shipped green through builds, ~1460 tests, clippy,
    verify_vendor_state.py and loc_ratchet, and went red on main.
    """
    write_repo(tmp_path, "1.14.0", "1.14.0")
    serve(guard, monkeypatch, manifest("1.13.5", complete_platforms()))
    monkeypatch.setattr("sys.argv", ["check", "--repo-root", str(tmp_path)])

    assert guard.main() == 1
    out = capsys.readouterr().out
    assert "1.14.0" in out, "must name the version that has no release"
    assert "1.13.5" in out, "must list what is actually published"
    assert "soldr#2164" in out


def test_a_published_but_incomplete_version_fails_naming_the_gaps(
    guard, tmp_path, monkeypatch, capsys
):
    write_repo(tmp_path, "1.13.5", "1.13.5")
    serve(
        guard,
        monkeypatch,
        manifest("1.13.5", [platform_entry("linux", "x86_64", libc="musl")]),
    )
    monkeypatch.setattr("sys.argv", ["check", "--repo-root", str(tmp_path)])

    assert guard.main() == 1
    out = capsys.readouterr().out
    assert "windows/arm/msvc" in out
    assert "mac/x86" in out


def test_a_complete_version_passes(guard, tmp_path, monkeypatch):
    write_repo(tmp_path, "1.13.5", "1.13.5")
    serve(guard, monkeypatch, manifest("1.13.5", complete_platforms()))
    monkeypatch.setattr("sys.argv", ["check", "--repo-root", str(tmp_path)])
    assert guard.main() == 0


def test_the_repository_pin_has_every_required_asset(guard, monkeypatch):
    """Runs against the real repo and the live manifest.

    Skipped rather than failed when the manifest is unreachable, for the same
    reason `main` skips: this is the one test here that leaves the machine.
    """
    monkeypatch.setattr("sys.argv", ["check"])
    assert guard.main() == 0
