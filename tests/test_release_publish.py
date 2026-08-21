"""soldr#2469 step 2.2: the tag + draft-release mutations, out of YAML.

These two steps create the release tag and the draft GitHub Release. They were
inline bash, so their idempotency and their soldr#1252 recovery guidance could
only be exercised by attempting a real release. A fake runner makes both
testable.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from collections.abc import Callable, Sequence
from pathlib import Path

REPO_ROOT = Path(__file__).parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "release_publish.py"

REPO = "zackees/soldr"
TAG = "v1.2.3"
SHA = "0123456789abcdef"
RUN_ID = "42"


def load_module():
    spec = importlib.util.spec_from_file_location("release_publish", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["release_publish"] = module
    spec.loader.exec_module(module)
    return module


MODULE = load_module()


def fake_runner(
    codes: list[int],
) -> tuple[Callable[[Sequence[str]], subprocess.CompletedProcess], list[list[str]]]:
    """A runner replaying scripted exit codes, plus the list it records into.

    Returns the pair rather than a callable with a `.calls` attribute: a
    one-method class trips pylint's `too-few-public-methods`, and hanging an
    attribute off the closure trips mypy's `attr-defined`. An explicit tuple
    is what both accept, and it reads no worse at the call site.
    """
    remaining = list(codes)
    calls: list[list[str]] = []

    def run(args: Sequence[str]) -> subprocess.CompletedProcess:
        calls.append(list(args))
        code = remaining.pop(0) if remaining else 0
        return subprocess.CompletedProcess(list(args), code, "", "")

    return run, calls


def make_dist(tmp_path: Path, names=("a.tar.zst", "b.whl")) -> Path:
    dist = tmp_path / "dist"
    dist.mkdir()
    for name in names:
        (dist / name).write_text("x", encoding="utf-8")
    return dist


# --- asset globbing -------------------------------------------------------


def test_assets_are_sorted_for_a_deterministic_command_line(tmp_path: Path) -> None:
    dist = make_dist(tmp_path, ("z.whl", "a.tar.zst", "m.txt"))
    assets = MODULE.release_assets(dist)
    assert [Path(p).name for p in assets] == ["a.tar.zst", "m.txt", "z.whl"]


def test_an_empty_dist_is_a_named_error_not_a_literal_glob(tmp_path: Path) -> None:
    """The inline `dist/*` degraded to a literal argument and confused gh."""
    dist = tmp_path / "dist"
    dist.mkdir()
    try:
        MODULE.release_assets(dist)
    except FileNotFoundError as error:
        assert "no release assets found" in str(error)
    else:  # pragma: no cover - the assertion above is the contract
        raise AssertionError("an empty dist must raise")


def test_a_missing_dist_is_a_named_error(tmp_path: Path) -> None:
    try:
        MODULE.release_assets(tmp_path / "nope")
    except FileNotFoundError as error:
        assert "does not exist" in str(error)
    else:  # pragma: no cover
        raise AssertionError("a missing dist must raise")


def test_directories_inside_dist_are_not_uploaded(tmp_path: Path) -> None:
    dist = make_dist(tmp_path)
    (dist / "subdir").mkdir()
    assert [Path(p).name for p in MODULE.release_assets(dist)] == ["a.tar.zst", "b.whl"]


# --- ensure-tag -----------------------------------------------------------


def test_an_existing_tag_is_a_skip_not_a_failure() -> None:
    run, calls = fake_runner([0])  # `gh api refs/tags/<tag>` succeeds
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 0
    assert len(calls) == 1, "an existing tag must not attempt a create"


def test_a_missing_tag_is_created_at_the_requested_sha() -> None:
    run, calls = fake_runner([1, 0])  # lookup fails, create succeeds
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 0
    create = calls[1]
    assert f"ref=refs/tags/{TAG}" in create
    assert f"sha={SHA}" in create
    assert "POST" in create


def test_a_tag_create_failure_exits_one(monkeypatch, tmp_path: Path) -> None:
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    run, _calls = fake_runner([1, 1])
    assert MODULE.ensure_tag(REPO, TAG, SHA, run) == 1
    text = summary.read_text(encoding="utf-8")
    assert "Manual release recovery needed (tag)" in text
    # The recovery block must be runnable as-is (soldr#1252).
    assert f"gh api repos/{REPO}/git/refs -X POST -f ref=refs/tags/{TAG}" in text
    assert f"sha={SHA}" in text


# --- create-draft ---------------------------------------------------------


def test_a_new_release_is_created_as_a_draft(monkeypatch, tmp_path: Path) -> None:
    output = tmp_path / "out.txt"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    dist = make_dist(tmp_path)
    run, calls = fake_runner([1, 0])  # view fails (absent), create succeeds

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 0
    )
    create = calls[1]
    # Draft is the whole point: soldr#2469 step 4.2 verifies the asset set
    # before publication, which is impossible once a release is immutable.
    assert "--draft" in create
    assert "--generate-notes" in create
    assert create[create.index("--target") + 1] == SHA
    assert output.read_text(encoding="utf-8").strip() == "created=true"


def test_an_existing_release_re_uploads_with_clobber(
    monkeypatch, tmp_path: Path
) -> None:
    """Idempotency: a rerun after a partial success must not fail."""
    output = tmp_path / "out.txt"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    dist = make_dist(tmp_path)
    run, calls = fake_runner([0, 0])  # view succeeds (present), upload succeeds

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 0
    )
    upload = calls[1]
    assert upload[:3] == ["gh", "release", "upload"]
    assert "--clobber" in upload
    assert output.read_text(encoding="utf-8").strip() == "created=true"


def test_a_failed_re_upload_does_not_report_success(
    monkeypatch, tmp_path: Path
) -> None:
    """A partial asset set that reports success is the 0.9.0 wound."""
    output = tmp_path / "out.txt"
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    dist = make_dist(tmp_path)
    run, _calls = fake_runner([0, 1])  # view succeeds, upload fails

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 1
    )
    assert output.read_text(encoding="utf-8").strip() == "created=false"
    assert "Manual release recovery needed" in summary.read_text(encoding="utf-8")


def test_a_failed_create_sets_created_false_and_emits_recovery(
    monkeypatch, tmp_path: Path
) -> None:
    output = tmp_path / "out.txt"
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    dist = make_dist(tmp_path)
    run, _calls = fake_runner([1, 1])  # view fails, create fails

    assert (
        MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
        == 1
    )
    assert output.read_text(encoding="utf-8").strip() == "created=false"
    text = summary.read_text(encoding="utf-8")
    assert f"gh run download {RUN_ID} --repo {REPO}" in text
    assert "--pattern 'release-soldr-*'" in text


def test_every_asset_is_passed_to_gh(monkeypatch, tmp_path: Path) -> None:
    """The shell used to expand `dist/*`; the script must pass them all."""
    monkeypatch.setenv("GITHUB_OUTPUT", str(tmp_path / "out.txt"))
    dist = make_dist(tmp_path, ("one.whl", "two.whl", "three.tar.zst"))
    run, calls = fake_runner([1, 0])
    MODULE.create_draft_release(REPO, TAG, SHA, run_id=RUN_ID, dist=dist, run=run)
    create = calls[1]
    for name in ("one.whl", "two.whl", "three.tar.zst"):
        assert any(arg.endswith(name) for arg in create), name


# --- workflow wiring ------------------------------------------------------


def test_the_workflow_invokes_the_script_for_both_mutations() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "release-auto.yml").read_text(
        encoding="utf-8"
    )
    assert "release_publish.py ensure-tag" in workflow
    assert "release_publish.py create-draft" in workflow
    # The inline implementations must be gone, not merely bypassed. Match on
    # strings unique to those blocks rather than on the command names: a
    # comment explaining why `gh release create --target <sha>` creates the
    # tag implicitly is worth keeping, and should not fail this test.
    assert "already exists; uploading/replacing assets" not in workflow
    assert 'echo "created=true" >> "$GITHUB_OUTPUT"' not in workflow
    assert "Manual release recovery needed" not in workflow
