"""Parity guard for soldr's canonical eight-target contract (issue #1695)."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

from conftest import load_script_module

ROOT = Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "ci" / "canonical-targets.json"
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release-auto.yml"


def contract_targets() -> list[dict]:
    payload = json.loads(CONTRACT.read_text(encoding="utf-8"))
    assert payload["schema_version"] == 1
    targets = payload["targets"]
    assert len(targets) == 8, "canonical contract must contain exactly eight targets"
    assert len({row["triple"] for row in targets}) == 8, "duplicate canonical triple"
    assert len({row["alias"] for row in targets}) == 8, "duplicate canonical alias"
    return targets


def rust_string_list(path: Path, constant: str) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(rf"pub const {constant}[^=]*= &\[(.*?)\];", text, re.DOTALL)
    assert match, f"missing Rust constant {constant} in {path.relative_to(ROOT)}"
    return re.findall(r'"([^"]+)"', match.group(1))


def rust_tuple_table(path: Path, constant: str) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(rf"pub const {constant}[^=]*= &\[(.*?)\];", text, re.DOTALL)
    assert match, f"missing Rust table {constant} in {path.relative_to(ROOT)}"
    return dict(re.findall(r'\("([^"]+)",\s*"([^"]+)"\)', match.group(1)))


def workflow_job(text: str, job: str) -> str:
    match = re.search(
        rf"^  {re.escape(job)}:\s*$(.*?)(?=^  [\w-]+:\s*$|\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    assert match, f"canonical CI job `{job}` is missing"
    return match.group(1)


def test_runtime_and_manifest_mirrors_match_contract() -> None:
    rows = contract_targets()
    expected_triples = [row["triple"] for row in rows]
    expected_aliases = {row["alias"]: row["triple"] for row in rows}

    core = rust_string_list(
        ROOT / "crates" / "soldr-core" / "src" / "core" / "canonical_targets.rs",
        "CANONICAL_TARGETS",
    )
    aliases = rust_tuple_table(
        ROOT / "crates" / "soldr-cli" / "src" / "target_alias.rs",
        "CANONICAL_ALIASES",
    )
    manifest_section = (
        (ROOT / "Cargo.toml")
        .read_text(encoding="utf-8")
        .split("[workspace.metadata.soldr]", 1)[1]
        .split("\n[", 1)[0]
    )
    manifest_targets = re.findall(r'"([^-"\s]+(?:-[^"\s]+){2,})"', manifest_section)

    assert (
        core == expected_triples
    ), "CANONICAL_TARGETS drifted from canonical-targets.json"
    assert (
        aliases == expected_aliases
    ), "CANONICAL_ALIASES drifted from canonical-targets.json"
    assert (
        manifest_targets == expected_triples
    ), "Cargo target metadata drifted from canonical-targets.json"


def test_ci_and_blessed_alias_workflow_cover_every_target() -> None:
    rows = contract_targets()
    workflow = CI_WORKFLOW.read_text(encoding="utf-8")
    for row in rows:
        ci = row["ci"]
        # Dispatch below falls through to the `cross` arm, where a null
        # `run_job` would blow up inside `workflow_job` with a TypeError
        # rather than a readable assertion. Pin the enum so a typo'd kind
        # fails here, naming itself.
        assert ci["kind"] in {
            "native",
            "cross",
            "cross-build",
        }, f"{ci['build_job']} has unknown ci.kind {ci['kind']!r}"
        build = workflow_job(workflow, ci["build_job"])
        assert (
            row["triple"] in build
        ), f"{ci['build_job']} no longer builds {row['triple']}"
        if ci["kind"] == "native":
            assert (
                "_bootstrap-e2e.yml" in build
            ), f"{ci['build_job']} lost native build/run coverage"
            assert ci["runner"] in build, f"{ci['build_job']} runner drifted"
            continue
        if ci["kind"] == "cross-build":
            # soldr#1978 item 3. A target whose only available runner is the
            # cross-build host itself gets no replay job: the split exists to
            # reach a target-native runner, and there is none to reach. Pin
            # the absence, so the degenerate pair cannot come back unnoticed.
            assert (
                "_ci-cross-build-linux.yml" in build
            ), f"{ci['build_job']} lost Linux cross-build coverage"
            assert (
                ci["run_job"] is None
            ), f"{ci['build_job']} is cross-build but names a run job"
            # The whole justification for dropping the replay is that the
            # contract runner and the cross-build host are the same image. If
            # either side moves, the split becomes non-degenerate again and
            # this target needs its `target-run` back.
            cross_build = (
                ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"
            ).read_text(encoding="utf-8")
            # Whole-line match: `runs-on: ubuntu-24.04` is a *prefix* of
            # `runs-on: ubuntu-24.04-arm`, so a substring check would keep
            # passing after the host moved to ARM -- precisely the drift this
            # assertion exists to catch.
            assert re.search(
                rf"^\s*runs-on: {re.escape(ci['runner'])}\s*$",
                cross_build,
                re.MULTILINE,
            ), f"{ci['build_job']} host no longer matches its contract runner"
            # With no replay, compiling the workspace's test binaries for this
            # target is the only remaining target-specific test coverage. It
            # is cheap to delete by accident while "cleaning up the lane with
            # no consumer", so pin that the archive is still built and that
            # only the unread *upload* was dropped.
            assert (
                "upload_test_archive: false" in build
            ), f"{ci['build_job']} still uploads an artifact nothing replays"
            cross_build_archive = re.search(
                r"^\s*- name: Upload artifact\n\s*if: inputs\.upload_test_archive$",
                cross_build,
                re.MULTILINE,
            )
            assert cross_build_archive, (
                "the upload gate vanished from _ci-cross-build-linux.yml; "
                "build-only lanes would resume uploading unread archives"
            )
            assert (
                "nextest archive" in cross_build or "nextest_cmd" in cross_build
            ), f"{ci['build_job']} no longer compiles test binaries for its target"
            replay = ci["build_job"].removesuffix("-build")
            assert not re.search(
                rf"^  {re.escape(replay)}:\s*$", workflow, re.MULTILINE
            ), (
                f"{replay} reappeared: a target-run on {ci['runner']} replays "
                f"{row['triple']} on the image it was built on (soldr#1978 item 3)"
            )
            continue
        run = workflow_job(workflow, ci["run_job"])
        assert (
            "_ci-cross-build-linux.yml" in build
        ), f"{ci['build_job']} lost Linux cross-build coverage"
        assert (
            "_ci-target-run.yml" in run
        ), f"{ci['run_job']} lost native execution coverage"
        assert (
            f"needs: {ci['build_job']}" in run
        ), f"{ci['run_job']} is detached from its build job"
        assert (
            row["triple"] in run and ci["runner"] in run
        ), f"{ci['run_job']} target/runner drifted"

    blessed = (ROOT / ".github" / "workflows" / "build-all-from-linux.yml").read_text(
        encoding="utf-8"
    )
    matrix = dict(re.findall(r"\{ alias: ([^,]+),\s+target: ([^,]+),", blessed))
    expected = {row["alias"]: row["triple"] for row in rows}
    assert (
        matrix == expected
    ), "blessed build alias matrix drifted from canonical-targets.json"
    assert (
        '--target "${{ matrix.alias }}"' in blessed
    ), "workflow no longer exercises aliases through soldr build"


def test_release_inclusions_and_exclusions_match_contract() -> None:
    rows = contract_targets()
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")
    included = {row["triple"] for row in rows if row["release"]["status"] == "included"}
    archive_gate = workflow.split("expected_assets=(", 1)[1].split("\n          )", 1)[
        0
    ]
    archives = set(re.findall(r'soldr-\$\{version\}-([^"/]+)\.tar\.zst', archive_gate))
    assert (
        archives == included
    ), "release archive gate drifted from canonical-targets.json"
    for row in rows:
        release = row["release"]
        if release["status"] == "documented-exclusion":
            assert (
                release["workflow_marker"] in workflow
            ), f"release exclusion for {row['triple']} is undocumented"


def test_catalogue_mappings_cover_every_target() -> None:
    rows = contract_targets()
    script = ROOT / ".github" / "scripts" / "fetch_catalogued_nextest.py"
    sys.path.insert(0, str(script.parent))
    module = load_script_module(script, "fetch_catalogued_nextest")

    for row in rows:
        expected = tuple(row["catalogue"]["nextest"])
        assert (
            module.query_for_target(row["triple"]) == expected
        ), f"cargo-nextest mapping drifted for {row['triple']}"

    syslib_tables = {
        "bzip2_sysroot.rs": "BZIP2_TARGETS",
        "lzma_sysroot.rs": "LZMA_TARGETS",
        "mimalloc_sysroot.rs": "MIMALLOC_TARGETS",
        "python_sysroot.rs": "PYTHON_SYSROOT_TARGETS",
        "sqlite_sysroot.rs": "SQLITE_TARGETS",
        "uv_tool.rs": "UV_TOOL_HOSTS",
        "zlib_ng_sysroot.rs": "ZLIB_NG_TARGETS",
        "zstd_sysroot.rs": "ZSTD_TARGETS",
    }
    fetch = ROOT / "crates" / "soldr-fetch" / "src" / "fetch"
    for filename, constant in syslib_tables.items():
        mapping = rust_tuple_table(fetch / filename, constant)
        for row in rows:
            assert (
                mapping.get(row["triple"]) == row["catalogue"]["syslib_slug"]
            ), f"{constant} mapping drifted for {row['triple']}"


def test_documented_alias_table_matches_contract() -> None:
    docs = (ROOT / "docs" / "CROSS_COMPILE.md").read_text(encoding="utf-8")
    table = docs.split("<!-- canonical-target-contract:start -->", 1)[1].split(
        "<!-- canonical-target-contract:end -->", 1
    )[0]
    for row in contract_targets():
        assert (
            f"| `{row['alias']}` | `{row['triple']}` |" in table
        ), f"documentation row missing for {row['alias']} -> {row['triple']}"
