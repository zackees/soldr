"""Keep CI/Docker artifact transfers on shared verified download policies."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / ".github/scripts/download_large_asset.sh"
TARGETS = {
    ROOT / ".github/workflows/baseline-zero-deps.yml": 2,
    ROOT / "ci/docker-aarch64-musl-cross/Dockerfile": 3,
    ROOT / "ci/docker-aarch64-windows-msvc-cross/Dockerfile": 3,
}


def test_large_download_helper_has_progress_and_integrity_guards() -> None:
    source = HELPER.read_text(encoding="utf-8")
    for required in [
        "--speed-limit",
        "--speed-time",
        "--max-time",
        "--continue-at -",
        "sha256sum",
        "download failure=integrity",
        "2 ** (attempt - 1)",
    ]:
        assert required in source
    assert "7200" in source


def test_audited_large_downloads_use_shared_helper_and_not_short_deadlines() -> None:
    for path, minimum_invocations in TARGETS.items():
        source = path.read_text(encoding="utf-8")
        assert (
            source.count("download-large-asset")
            + source.count("download_large_asset.sh")
            + source.count("download_catalogued_asset.py")
            >= minimum_invocations
        ), path
        assert "--max-time 120" not in source, path
        assert "--max-time 600" not in source, path
