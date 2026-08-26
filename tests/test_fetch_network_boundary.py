"""Static guardrails for the blessed soldr-fetch network boundary."""

import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FETCH_BOUNDARY = (
    ROOT / "crates" / "soldr-fetch" / "src" / "fetch" / "stream_download.rs"
)


def test_reqwest_is_owned_only_by_soldr_fetch() -> None:
    """No facade or sibling crate may grow its own HTTP client dependency."""
    manifests = list((ROOT / "crates").glob("*/Cargo.toml"))
    owners = set()
    for manifest in manifests:
        parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
        if "reqwest" in parsed.get("dependencies", {}):
            owners.add(manifest.parent.name)
    assert owners == {"soldr-fetch"}


def test_network_dylint_and_ci_gate_exist() -> None:
    lint = ROOT / "dylints" / "ban_raw_network_access" / "src" / "lib.rs"
    plan = (ROOT / "crates" / "soldr-cli" / "src" / "ci_test" / "plan.rs").read_text(
        encoding="utf-8"
    )
    workflow = (ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )
    assert lint.is_file()
    assert "fetch::stream_download" in lint.read_text(encoding="utf-8")
    assert '"ban_raw_network_access"' in plan
    assert 'format!("dylint-test-{lint}")' in plan
    assert workflow.count("name: Run prescribed host validation") == 1
    assert "ci-test --target" in workflow


def test_blessed_boundary_documents_all_timeout_layers() -> None:
    source = FETCH_BOUNDARY.read_text(encoding="utf-8")
    for symbol in (
        "CONTROL_HEADER_TIMEOUT",
        "ASSET_HEADER_TIMEOUT",
        "ASSET_IDLE_TIMEOUT",
        "ASSET_SAFETY_TIMEOUT",
        "stream_response_to_temp_file_with_safety_timeout",
    ):
        assert symbol in source
