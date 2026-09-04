"""No macos-* GitHub Actions runner exists anywhere (soldr#3071, soldr#3076).

Owner mandate (2026-09-02): no job may run on a macos-* runner for building
or testing. macOS execution happens only inside a zackees/docker-mac-x64
Recovery guest (ci/macos_recovery_run.py, ci/smoke_release_artifacts.py)
hosted on an ordinary ubuntu-24.04 runner.

This replaces tests/test_macos_dockur_lane_contract.py, whose contract (a
hand-baked dockur/macos image pulled from GHCR over ssh, soldr#3071) never
worked: the image was never published and the ssh secret was never set, so
`e2e-macos-x64` and `smoke_macos_x64` failed at preflight on every run.
soldr#3076 replaces that whole plan with zackees/docker-mac-x64, which needs
neither.
"""

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"

# A `runs-on:`/`runs_on:`/`runner:` YAML value (bare or quoted, matrix-row
# style or JSON-style `"runner": "..."`) naming a macos-* runner label.
# Comment-only lines are excluded below.
RUNNER_LABEL_PATTERN = re.compile(
    r'(?:runs[-_]on|"?runner"?)\s*:\s*"?macos-[a-z0-9.]+', re.IGNORECASE
)

DOCKER_MAC_X64_PIN = "605ae73bca3bf03feb5f70a45dec7da048de137b"


def _non_comment_lines(text: str) -> list[str]:
    return [line for line in text.splitlines() if not line.strip().startswith("#")]


def test_no_workflow_names_a_macos_runner_label() -> None:
    offenders = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for line in _non_comment_lines(workflow.read_text(encoding="utf-8")):
            if RUNNER_LABEL_PATTERN.search(line):
                offenders.append(f"{workflow.name}: {line.strip()}")
    assert not offenders, (
        "no GitHub Actions job may run on a macos-* runner (owner mandate "
        f"2026-09-02, soldr#3071): {offenders}"
    )


SCAN_ROOTS = (".github", "ci", "docs", "tests")
SCAN_TOP_LEVEL_FILES = ("README.md",)


def test_no_ghcr_baked_guest_image_or_ssh_secret_anywhere() -> None:
    """soldr#3071's hand-baked guest was never published; soldr#3076 dropped it.

    Scoped to the directories that could plausibly reference it (workflow
    config, ci scripts, docs, and this test suite) rather than the whole
    checkout -- `target/` alone is tens of gigabytes of unrelated build
    output.
    """
    this_file = Path(__file__).resolve()
    offenders = []
    paths = [
        p
        for root in SCAN_ROOTS
        for p in sorted((REPO_ROOT / root).rglob("*"))
        if p.is_file() and p.resolve() != this_file
    ]
    paths += [REPO_ROOT / name for name in SCAN_TOP_LEVEL_FILES]
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, PermissionError):
            continue
        if "ghcr.io/zackees/soldr/macos-x64-guest" in text:
            offenders.append(f"{path.relative_to(REPO_ROOT)}: baked guest image string")
        if "SOLDR_MACOS_GUEST_SSH_KEY" in text:
            offenders.append(f"{path.relative_to(REPO_ROOT)}: guest ssh secret name")
    assert not offenders, offenders


def test_x64_lane_uses_the_recovery_guest_on_an_ubuntu_runner() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    start = ci.index("  e2e-macos-x64:\n")
    end = ci.index("\n  # ---------- macOS ARM64", start)
    run_job = ci[start:end]
    assert "runs_on: ubuntu-24.04" in run_job
    assert "target_execution: x86_64-recovery" in run_job
    assert "uses: ./.github/workflows/_ci-target-run.yml" in run_job


def test_no_macos_arm64_run_job_exists() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    assert re.search(r"(?m)^  e2e-macos-arm64:\s*$", ci) is None
    # The build-only lane must still exist -- only its paired run job is gone.
    assert re.search(r"(?m)^  e2e-macos-arm64-build:\s*$", ci) is not None


def test_docker_mac_x64_action_is_pinned_to_a_full_sha_with_a_main_comment() -> None:
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    for workflow_name, text in (
        ("_ci-target-run.yml", target_run),
        ("release-auto.yml", release),
    ):
        pattern = re.compile(
            rf"uses:\s*zackees/docker-mac-x64@{DOCKER_MAC_X64_PIN}\s*#\s*main"
        )
        assert pattern.search(text), (
            f"{workflow_name} must pin zackees/docker-mac-x64 to a full commit "
            f"SHA ({DOCKER_MAC_X64_PIN}) with a '# main' comment"
        )


def test_no_dockur_ssh_machinery_referenced_in_workflows() -> None:
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        text = workflow.read_text(encoding="utf-8")
        assert "macos_x64_guest.py" not in text, workflow.name
        assert "x86_64-dockur" not in text, workflow.name
        # soldr#3076: GHCR auth existed only to pull the retired baked guest
        # image; docker-mac-x64 needs no registry auth at all.
        assert "docker/login-action" not in text, workflow.name


def test_recovery_lane_ships_the_tests_archive_to_the_guest() -> None:
    """soldr#3078: the Recovery guest replays the real nextest archive now,
    not just `soldr --version`/`--help`, so the share-dir prep must actually
    ship it in."""
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    assert "share/tests.tar.zst" in target_run
    assert "-tests.tar.zst" in target_run
    assert "nextest_list_all" in target_run or "nextest list" in target_run


def test_release_workflow_has_the_macos_x64_replay_jobs() -> None:
    """soldr#3078: publish is gated on the same archive replay `e2e-macos-x64`
    runs on every PR, pinned to the release commit."""
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    assert re.search(r"(?m)^  e2e_macos_x64_build:\s*$", release) is not None
    assert re.search(r"(?m)^  e2e_macos_x64_replay:\s*$", release) is not None
    assert "uses: ./.github/workflows/_ci-cross-build-linux.yml" in release
    assert "target_execution: x86_64-recovery" in release


def test_release_publish_depends_on_the_macos_x64_replay() -> None:
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    start = release.index("\n  publish:\n")
    end = release.index("\n  verify_github_release:\n", start)
    publish_job = release[start:end]
    assert "e2e_macos_x64_replay" in publish_job
    assert "needs.e2e_macos_x64_replay.result == 'success'" in publish_job
