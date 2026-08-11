import re
from pathlib import Path

from conftest import WORKSPACE_CRATES

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"


def _job_block(workflow: str, job: str, next_job: str | None = None) -> str:
    start = workflow.index(f"  {job}:\n")
    if next_job:
        end = workflow.index(f"  {next_job}:\n", start)
    else:
        match = re.search(r"(?m)^  [a-zA-Z0-9_-]+:\n", workflow[start + 1 :])
        end = start + 1 + match.start() if match else len(workflow)
    return workflow[start:end]


def _step_block(workflow: str, step_name: str) -> str:
    """Return one workflow step and fail if its name is not unique."""
    marker = f"      - name: {step_name}\n"
    assert workflow.count(marker) == 1, f"expected exactly one {step_name!r} step"
    start = workflow.index(marker)
    end_match = re.search(
        r"(?m)^      - (?:name: |uses: |run: )", workflow[start + len(marker) :]
    )
    end = start + len(marker) + end_match.start() if end_match else len(workflow)
    return workflow[start:end]


def _job_input(job: str, name: str) -> str:
    matches = re.findall(rf"^      {re.escape(name)}: ([^\n]+)$", job, re.MULTILINE)
    assert len(matches) == 1, f"expected one {name!r} input"
    return matches[0]


def _assert_no_narrowing(command: str) -> None:
    assert not re.search(r"(?:^|\s)(?:-p|--package|--exclude)(?:[=\s]|$)", command)
    assert not re.search(r"(?:^|\s)(?:-E|--filter|--filter-expr)(?:[=\s]|$)", command)


def test_windows_behavior_contract_reaches_native_target_runners() -> None:
    behavioral_test = (
        REPO_ROOT / "crates" / "soldr-cli" / "tests" / "windows_delete_semantics.rs"
    ).read_text(encoding="utf-8")
    expected_tests = [
        "read_only_files_do_not_block_a_recursive_delete",
        "read_only_files_do_not_block_a_single_file_delete",
        "a_read_only_directory_does_not_block_its_parents_delete",
    ]
    assert behavioral_test.startswith("#![cfg(windows)]\n")
    assert "#[test]" not in behavioral_test
    for test_name in expected_tests:
        declaration = rf"(?m)^timed_test!\(\s*{re.escape(test_name)}\s*,"
        assert re.search(declaration, behavioral_test)

    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    archive = _step_block(cross, "Build nextest archive")
    for required in [
        "soldr cargo nextest archive",
        '--target "$target"',
        "--workspace",
        '--archive-file "$archive"',
        "--archive-format tar-zst",
        'archive="dist/${{ inputs.artifact_name }}-tests.tar.zst"',
        'ls -la "$archive"',
    ]:
        assert required in archive
    command_marker = "\n          soldr cargo nextest archive \\\n"
    archive_command = archive[archive.index(command_marker) + 1 :]
    archive_command = archive_command[
        : archive_command.index('\n          ls -la "$archive"')
    ]
    _assert_no_narrowing(archive_command)

    upload = _step_block(cross, "Upload artifact")
    assert "name: ${{ inputs.artifact_name }}" in upload
    assert "dist/${{ inputs.artifact_name }}-tests.tar.zst" in upload
    assert "if-no-files-found: error" in upload
    assert "if-no-files-found: warn" not in upload
    assert "if-no-files-found: ignore" not in upload

    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    replay = _step_block(target_run, "Run complete pre-built test archive")
    archive_assignment = 'archive="artifact/${{ inputs.artifact_name }}-tests.tar.zst"'
    archive_check = 'test -f "$archive"'
    list_command = '"$NEXTEST_BIN" nextest list'
    run_command = '"$NEXTEST_BIN" nextest run'
    for required in [
        archive_assignment,
        archive_check,
        list_command,
        run_command,
        '--archive-file "$archive"',
        "--no-fail-fast",
    ]:
        assert required in replay
    assert replay.index(archive_assignment) < replay.index(archive_check)
    assert replay.index(archive_check) < replay.index(list_command)
    assert replay.index(list_command) < replay.index(run_command)
    assert replay.count('--archive-file "$archive"') == 2
    list_invocation = replay[replay.index(list_command) : replay.index(run_command)]
    run_invocation = replay[replay.index(run_command) :]
    _assert_no_narrowing(list_invocation)
    _assert_no_narrowing(run_invocation)


def test_windows_target_runner_pairs_share_their_producer_artifacts() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    pairs = [
        (
            "e2e-windows-x64-build",
            "e2e-windows-x64",
            "x86_64-pc-windows-msvc",
            "windows-2025",
            "soldr-ci-e2e-windows-x64",
        ),
        (
            "e2e-windows-arm64-build",
            "e2e-windows-arm64",
            "aarch64-pc-windows-msvc",
            "windows-11-arm",
            "soldr-ci-e2e-windows-arm64",
        ),
    ]
    for build_name, run_name, target, runner, artifact in pairs:
        build = _job_block(ci, build_name, run_name)
        run = _job_block(ci, run_name)
        assert "uses: ./.github/workflows/_ci-cross-build-linux.yml" in build
        assert "uses: ./.github/workflows/_ci-target-run.yml" in run
        assert re.search(rf"(?m)^    needs: {re.escape(build_name)}$", run)
        assert _job_input(build, "artifact_name") == artifact
        assert _job_input(run, "artifact_name") == artifact
        assert _job_input(build, "source_ref") == "${{ github.sha }}"
        assert _job_input(build, "target") == target
        assert _job_input(run, "target") == target
        assert _job_input(run, "runs_on") == runner


def test_windows_msvc_ci_builds_and_archives_real_tests() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    target_run = (WORKFLOWS / "_ci-target-run.yml").read_text(encoding="utf-8")
    cache_roundtrip = (
        REPO_ROOT / ".github" / "scripts" / "windows_msvc_cache_roundtrip.py"
    ).read_text(encoding="utf-8")
    nextest_config = (REPO_ROOT / ".config" / "nextest.toml").read_text(
        encoding="utf-8"
    )
    baseline = (WORKFLOWS / "baseline-zero-deps.yml").read_text(encoding="utf-8")
    arm_build = _job_block(ci, "e2e-windows-arm64-build", "e2e-windows-arm64")
    arm_run = _job_block(ci, "e2e-windows-arm64")
    assert "if: false" not in arm_build
    assert "if: false" not in arm_run

    assert "if: (!contains(inputs.target, 'pc-windows-msvc'))" not in cross
    assert "soldr --no-cache build --profile" not in cross
    assert 'soldr build --profile "$ci_profile" --target "$target"' in cross
    assert "soldr cargo nextest archive" in cross
    assert "soldr_args+=(--no-cache)" not in cross
    assert "Validate warm Windows cache restoration" in cross
    assert "windows_msvc_cache_roundtrip.py" in cross
    assert "--phase build" in cross
    assert "--phase archive" in cross
    assert (
        "--no-cache"
        not in cross[cross.index("- name: Cross-build soldr (ci-nextest profile)") :]
    )
    assert "cache: ${{ (contains(inputs.target, 'pc-windows-msvc')" in cross
    assert "expected binary missing: $binary; searching target tree" in cross
    assert 'find target -type f \\( -name "soldr" -o -name "soldr.exe" \\)' in cross
    assert "normalized binary layout:" in cross
    assert '-path "*/$ci_profile/deps/soldr"' in cross
    assert 'dd if="$binary" bs=1 count=2' in cross
    assert (
        "              soldr --no-cache cargo xwin build "
        "--target x86_64-pc-windows-msvc\n"
        "              ls -l target/x86_64-pc-windows-msvc/debug/hellowin.exe"
        in baseline
    )
    for first_party_package in WORKSPACE_CRATES:
        assert f"-p {first_party_package}" in cross

    assert "test archive missing" not in target_run
    assert '"$SOLDR_BIN" --version' not in target_run
    assert '"$NEXTEST_BIN" nextest run' in target_run
    assert 'echo "SOLDR_BIN=$soldr_bin"' in target_run
    assert (
        "case '${{ inputs.target }}' in *-pc-windows-msvc) suffix=\".exe\""
        in target_run
    )
    assert "artifact/package/soldr$suffix" in target_run
    assert "artifact/package/tools/cargo-nextest$suffix" in target_run
    assert 'echo "SOLDR_TEST_WORKSPACE_ROOT=$GITHUB_WORKSPACE"' in target_run
    assert "SOLDR_GITHUB_TOKEN: ${{ github.token }}" in target_run
    assert "actions/setup-python@" in target_run
    assert 'python-version: "3.13"' in target_run
    assert "python3 .github/scripts/target_run_summary.py" not in target_run
    assert '"$SOLDR_BIN" toolchain ensure --json' in target_run
    assert '"$SOLDR_BIN" toolchain link' in target_run
    assert '--shim-dir "$TOOLCHAIN_SHIM_DIR"' in target_run
    assert '"$SOLDR_BIN" rustc -vV' in target_run
    assert '"$SOLDR_BIN" cargo -V' in target_run
    assert "skip_filter:" not in target_run
    assert "inputs.skip_filter" not in target_run
    assert "SOLDR_TEST_SKIP_SOURCE_TREE" not in target_run
    assert "submodules: recursive" in target_run
    for workflow in [cross, target_run]:
        assert "--filter-expr" not in workflow
        assert "\n            -E " not in workflow
        assert "\n            --filter " not in workflow
    assert "ARCHIVE_FILTER" not in cache_roundtrip
    assert '"-E"' not in cache_roundtrip
    assert "archive every test binary" in cross
    assert "--profile target-run" in target_run
    assert "target/nextest/target-run/junit.xml" in target_run
    assert "target_run_summary.py" in target_run
    assert "--require-junit" in target_run
    assert target_run.index('if [ "$run_status" -ne 0 ]') < target_run.index(
        'exit "$summary_status"'
    )
    assert "if: always()" in target_run
    assert "[profile.target-run.junit]" in nextest_config
    assert 'path = "junit.xml"' in nextest_config
    assert "default-filter" not in nextest_config
    assert "fetch_catalogued_nextest.py" in cross
    assert "cargo-nextest.json" in target_run
    assert "taiki-e/install-action" not in target_run


def test_native_linux_runs_the_complete_workspace_suite() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    build_and_test = (WORKFLOWS / "_build-and-test.yml").read_text(encoding="utf-8")
    assert "x86_64 GNU is the canonical native exception" in ci
    assert "other seven" in ci
    assert (
        "soldr cargo test --workspace --lib --tests --locked --target ${{ inputs.target }}"
    ) in build_and_test
    assert "soldr cargo test -p soldr-cli" not in build_and_test


def test_archived_source_tests_use_only_runtime_workspace_resolution() -> None:
    crates_root = REPO_ROOT / "crates"
    allowed = {
        crates_root / "soldr-cli" / "tests" / "common" / "mod.rs",
        crates_root / "soldr-cli" / "src" / "prepare_cmd.rs",
        crates_root / "soldr-fetch" / "build.rs",
    }
    forbidden = (
        'env!("CARGO_MANIFEST_DIR")',
        'option_env!("CARGO_MANIFEST_DIR")',
        'var("CARGO_MANIFEST_DIR")',
        'var_os("CARGO_MANIFEST_DIR")',
    )
    offenders = []
    for path in crates_root.rglob("*.rs"):
        if path in allowed:
            continue
        body = path.read_text(encoding="utf-8")
        if any(pattern in body for pattern in forbidden):
            offenders.append(path.relative_to(REPO_ROOT).as_posix())
    assert not offenders, "workflows must not hardcode these patterns: " + ", ".join(
        offenders
    )


def test_fast_build_only_skips_windows_e2e_for_low_risk_changes() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    policy = _job_block(ci, "windows-e2e-policy", "e2e-cross-bootstrap-soldr")
    x64_build = _job_block(ci, "e2e-windows-x64-build", "e2e-windows-x64")
    arm64_build = _job_block(ci, "e2e-windows-arm64-build", "e2e-windows-arm64")

    assert "windows_e2e_policy.py" in policy
    assert "fetch-depth: 0" in policy
    assert "run_windows_e2e" in policy
    for block in [x64_build, arm64_build]:
        assert "windows-e2e-policy" in block
        assert "needs.windows-e2e-policy.outputs.run_windows_e2e == 'true'" in block
        assert "fast-build" not in block

    windows_section = ci[ci.index("# ---------- Windows x64") :]
    assert "github.event.pull_request.labels" not in windows_section
    assert "fast-build may skip only the Windows MSVC E2E pairs" in ci
    assert "macOS E2E pairs always run" in ci


def test_cross_workflow_bootstraps_toolchain_dependencies_through_soldr() -> None:
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    assert "cross-targets:" not in cross
    assert (
        'soldr prepare --target "${{ inputs.target }}" --github-env "$GITHUB_ENV"'
        in cross
    )
    assert "soldr --no-cache cargo build --profile ci-bootstrap" in cross
    for unmanaged_installer in [
        "sudo apt-get",
        "mlugg/setup-zig",
        "taiki-e/install-action",
        "run: rustup ",
        "\n          cargo build --profile ci-bootstrap",
        "\n            cargo zigbuild ",
        "\n            cargo build ",
        "nextest_cmd=(cargo nextest archive)",
    ]:
        assert unmanaged_installer not in cross


def test_catalogue_download_consumers_require_sha256_metadata() -> None:
    cross = (WORKFLOWS / "cross-compile-all-targets.yml").read_text(encoding="utf-8")
    baseline = (WORKFLOWS / "baseline-zero-deps.yml").read_text(encoding="utf-8")
    fetch = (REPO_ROOT / ".github" / "scripts" / "fetch_or_build_tool.sh").read_text(
        encoding="utf-8"
    )
    downloader = (
        REPO_ROOT / ".github" / "scripts" / "download_catalogued_asset.py"
    ).read_text(encoding="utf-8")

    assert cross.count("--json") >= 2
    assert cross.count("download_large_asset.sh") >= 2
    assert "--json cargo-zigbuild" in baseline
    assert "download_large_asset.sh" in baseline
    assert "download_catalogued_asset.py" in fetch
    assert "sha256 mismatch" in downloader


def test_linux_zig_cross_lanes_use_current_checkout_soldr_bootstrap() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    lane_names = [
        ("e2e-linux-arm64-build", "e2e-linux-arm64"),
        # x86_64-musl has no paired target-run (soldr#1978 item 3). Delimit
        # with None rather than the *next lane's* header: every Linux cross
        # lane carries identical `needs:` / `bootstrap_artifact_name:` lines,
        # so a job inserted between the two would be swallowed into this
        # block and satisfy the assertions below exactly when this lane lost
        # them. None yields the same slice without that coupling.
        ("e2e-linux-x64-musl-build", None),
        ("e2e-linux-arm64-musl-build", "e2e-linux-arm64-musl"),
    ]
    for job, next_job in lane_names:
        block = _job_block(ci, job, next_job)
        assert "needs: e2e-cross-bootstrap-soldr" in block
        assert "bootstrap_artifact_name: soldr-ci-bootstrap-linux-gnu" in block

    download = cross[
        cross.index("      - name: Download shared bootstrap soldr artifact") :
    ]
    download = download[: download.index("      - name:", 10)]
    assert "inputs.bootstrap_artifact_name != ''" in download
    assert "contains(inputs.target" not in download

    expose = cross[cross.index("      - name: Expose bootstrap soldr on PATH") :]
    expose = expose[: expose.index("      #", 10)]
    assert "inputs.bootstrap_artifact_name != ''" in expose


def test_native_linux_integration_backstop_runs_on_pull_requests() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    block = _job_block(ci, "build-linux-x64", "pep517-daemon-smoke")
    assert "github.ref_name == 'main' || github.event_name == 'pull_request'" in block
    assert "soldr#1676" in block
    assert "canonical native exception" in block


def test_manual_cross_compile_workflows_use_blessed_supported_targets() -> None:
    build_all = (WORKFLOWS / "build-all-from-linux.yml").read_text(encoding="utf-8")
    cross_all = (WORKFLOWS / "cross-compile-all-targets.yml").read_text(
        encoding="utf-8"
    )

    for target in [
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ]:
        assert target in build_all
        assert target in cross_all

    assert "soldr build --release" in build_all
    for friendly in [
        "linux-arm",
        "linux-x86-musl",
        "linux-arm-musl",
        "windows-x86",
        "windows-arm",
    ]:
        start = cross_all.index(f"          - friendly: {friendly}\n")
        next_entry = cross_all.find("          - friendly:", start + 1)
        end = next_entry if next_entry != -1 else len(cross_all)
        assert "tool:" not in cross_all[start:end]
    assert "soldr build \\\n" in cross_all
    assert "matrix.tool" not in cross_all


def test_production_cross_workflows_do_not_select_legacy_backends() -> None:
    workflows = [
        WORKFLOWS / "cross-compile-all-targets.yml",
        WORKFLOWS / "_ci-cross-build-linux.yml",
    ]
    helper = REPO_ROOT / ".github" / "scripts" / "fetch_or_build_tool.sh"
    forbidden = [
        "matrix.tool",
        "make_zig_cc_wrappers.sh",
        "cross-targets:",
        "cross-" + "tool:",
        "soldr " + "cargo zigbuild",
        "soldr " + "cargo xwin",
    ]
    for path in [*workflows, helper]:
        body = path.read_text(encoding="utf-8")
        executable = "\n".join(
            line for line in body.splitlines() if not line.lstrip().startswith("#")
        )
        for token in forbidden:
            assert (
                token not in executable
            ), f"{path.relative_to(REPO_ROOT)} selects {token!r}"

    cross_all_executable = "\n".join(
        line
        for line in workflows[0].read_text(encoding="utf-8").splitlines()
        if not line.lstrip().startswith("#")
    )
    helper_selectors = re.findall(
        r"fetch_or_build_tool\.sh\s*\\\s*\n"
        r"\s*\S+\s+(?:\"[^\"]*\"|'[^']*'|\S+)\s+(\S+)",
        cross_all_executable,
    )
    assert len(helper_selectors) == 4
    assert set(helper_selectors) == {"soldr-build"}

    legacy_command = re.compile(r"(?m)^\s*(?:soldr\s+)?cargo\s+(?:zigbuild|xwin)\b")
    for path in workflows:
        body = path.read_text(encoding="utf-8").replace("\\\n", " ")
        assert legacy_command.search(body) is None

    ci_cross = workflows[1].read_text(encoding="utf-8")
    assert "soldr cargo clippy \\\n            --target" in ci_cross
    assert "soldr cargo nextest archive" in ci_cross
    assert "Validate native target lifecycle" in cross_all_executable
    assert "Validate target lint and test lifecycle" in cross_all_executable
    assert cross_all_executable.count("soldr cargo test --no-run") >= 2


def test_normal_gnu_lifecycle_has_no_zig_fallback() -> None:
    """#2237: GNU must stay catalogue-backed even while musl retains #2244's fallback."""
    lifecycle = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "target_lifecycle.rs"
    ).read_text(encoding="utf-8")
    legacy_musl = (
        REPO_ROOT / "crates" / "soldr-cli" / "src" / "linux_cross.rs"
    ).read_text(encoding="utf-8")
    prepare = (REPO_ROOT / "crates" / "soldr-cli" / "src" / "prepare_cmd.rs").read_text(
        encoding="utf-8"
    )
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    assert "no catalogue-backed GNU/Linux toolchain is available" in lifecycle
    assert "abi == Some(TargetAbi::Musl)" in lifecycle
    assert not re.search(r'"(?:x86_64|aarch64)-unknown-linux-gnu"\s*=>', legacy_musl)
    assert (
        "GNU Linux uses the catalogue-backed compiler/sysroot lifecycle" in legacy_musl
    )
    assert "GNU/Linux toolchain" in prepare
    assert "GNU uses the catalogue-backed toolchain" in prepare

    executable = "\n".join(
        line for line in cross.splitlines() if not line.lstrip().startswith("#")
    )
    assert not re.search(
        r"(?:soldr\s+)?cargo\s+zigbuild\b.*unknown-linux-gnu", executable
    )
    assert "soldr build + catalogue-backed GCC" in cross


def test_all_miss_cross_builds_bound_compile_concurrency() -> None:
    """#2453: cache-disabled hosted lanes must retain memory headroom."""
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")

    cross_job = _job_block(cross, "cross-build", "")
    wheel_job = _job_block(ci, "wheel-cross-verify")
    for job in (cross_job, wheel_job):
        assert 'CARGO_BUILD_JOBS: "2"' in job
        assert 'SOLDR_JOBS: "2"' in job
    assert 'CARGO_PROFILE_CI_NEXTEST_CODEGEN_UNITS: "4"' in cross_job
    assert "shared-key: cross-build-${{ inputs.target }}-v7" in cross_job


def test_gnu_catalogue_fixture_is_part_of_both_gnu_ci_lanes() -> None:
    """#2236: CI must execute the mixed-language catalogue proof, not just compile Soldr."""
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")
    proof = (REPO_ROOT / ".github/scripts/gnu_linux_toolchain_e2e.py").read_text(
        encoding="utf-8"
    )

    assert "Prove catalogue GNU lifecycle without Zig" in cross
    assert "contains(inputs.target, 'unknown-linux-gnu')" in cross
    assert "gnu_linux_toolchain_e2e.py" in cross
    assert "Build current Soldr GNU proof driver" in cross
    assert "target/x86_64-unknown-linux-gnu/ci-bootstrap/soldr" in cross
    for required in (
        "cc::Build",
        "pkg_config::Config",
        "cmake-probe",
        "verify_glibc_baseline.py",
        "SOLDR_GNU_LINUX_TOOLCHAIN_ROOT",
        "CMAKE_C_COMPILER",
        'cmake_definition("CMAKE_C_COMPILER")',
        'cmake_definition("CMAKE_SYSROOT")',
        "SOLDR_CACHE_DIR",
        "SOLDR_TEST_NO_NETWORK",
        "prepared.tar.zst",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
    ):
        assert required in proof


def test_mac_x64_distribution_uses_pinned_setup_soldr_on_intel() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    install = (REPO_ROOT / "scripts" / "install.js").read_text(encoding="utf-8")
    npm_docs = (REPO_ROOT / "docs" / "NPM_PUBLISHING.md").read_text(encoding="utf-8")
    verification_docs = (REPO_ROOT / "docs" / "RELEASE_VERIFICATION.md").read_text(
        encoding="utf-8"
    )

    mac_build = _job_block(ci, "e2e-macos-x64-build", "e2e-macos-x64")
    assert "if: false" not in mac_build
    assert "target: x86_64-apple-darwin" in mac_build

    # Release lanes use the same pinned setup-soldr target environment;
    # macOS x64 stays native on the Intel runner.
    assert (
        "- name: macOS x64\n"
        "            runner: macos-15-intel\n"
        "            target: x86_64-apple-darwin" in release
    )
    assert '"x86_64-apple-darwin": {"os": "darwin", "arch": "x86_64"}' in release
    assert 'prepare --target "$target" --github-env "$GITHUB_ENV"' in release
    assert (
        "uses: zackees/setup-soldr@62d1596b70168e422156f12273a2ed476d3a16dc" in release
    )
    assert "version: 0.8.44" in release
    assert "cross-targets: ${{ matrix.setup_target }}" in release
    assert "target-wheel-hook" in release
    assert "soldr-${version}-x86_64-apple-darwin.tar.zst" in release
    intel_wheel = "soldr-${cargo_version}-py3-none-macosx_10_12_x86_64.whl"
    assert release.count(intel_wheel) == 2
    assert "soldr-${cargo_version}-py3-none-macosx_11_0_x86_64.whl" not in release

    sdk_step = _step_block(release, "Restore the native macOS SDK root")
    assert "SDKROOT=$(xcrun --sdk macosx --show-sdk-path)" in sdk_step
    native_build = _step_block(
        release, "Build native macOS release binary through pinned Soldr"
    )
    assert "if: contains(matrix.target, 'apple-darwin')" in native_build
    assert ".github/scripts/native_release_build.py binary" in native_build
    blessed_build = _step_block(release, "Build release binary (soldr-driven)")
    assert "!contains(matrix.target, 'apple-darwin')" in blessed_build

    assert '"darwin-x64": { triple: "x86_64-apple-darwin"' in install
    assert "intentionally not published" not in install
    assert "x86_64-apple-darwin" in npm_docs
    assert "macos-15-intel" in npm_docs
    assert "x86_64-apple-darwin" in verification_docs
    assert "Mach-O x86_64" in verification_docs


def test_linux_arm64_release_uses_matching_supported_hosts() -> None:
    """GNU ARM cross-builds on x64 while musl ARM builds and smokes natively."""
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    assert "- name: Linux ARM64 (glibc)\n" in release
    assert "- name: Linux ARM64 (musl)\n" in release
    arm_blocks = re.findall(
        r"- name: Linux ARM64 \((?:glibc|musl)\)\n(?:\s+#.*\n)*\s+runner: ([^\n]+)",
        release,
    )
    assert arm_blocks == ["ubuntu-24.04", "ubuntu-24.04-arm"]


def test_release_wheels_use_setup_soldr_target_hooks_without_zig_or_xwin() -> None:
    """PEP 517 runs inside setup-soldr's prepared target environment."""
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")

    assert '"$driver" prepare --target "$target" --github-env "$GITHUB_ENV"' in release
    assert (
        "uses: zackees/setup-soldr@62d1596b70168e422156f12273a2ed476d3a16dc" in release
    )
    assert "version: 0.8.44" in release
    assert "cross-targets: ${{ matrix.setup_target }}" in release
    assert "wheel_hook='${{ steps.setup_soldr.outputs.target-wheel-hook }}'" in release
    assert ".github/scripts/build_release_wheel.py" in release
    assert "uv python install 3.13" in release
    assert "uv run --no-project --python 3.13 --with build python" in release
    assert '--target "${{ matrix.target }}"' in release
    assert "maturin --zig" not in release
    assert "Setup zig for Linux wheel lanes" not in release
    assert "lzma_pkgconfig" not in release
    assert "runner: ubuntu-24.04-arm" in release
    assert "startsWith(matrix.target, 'aarch64-')" not in release
    assert (
        "target: aarch64-unknown-linux-musl\n"
        '            setup_target: ""\n'
        "            binary: soldr" in release
    )
    native_arm_musl = _step_block(release, "Build ARM64 musl release binary natively")
    assert "CC_aarch64_unknown_linux_musl: musl-gcc" in native_arm_musl
    assert ".github/scripts/native_release_build.py binary" in native_arm_musl

    target_hook_wheel = _step_block(
        release, "Build wheel through setup-soldr target environment"
    )
    assert (
        "if: ${{ !contains(matrix.target, 'unknown-linux-musl') }}" in target_hook_wheel
    )

    native_arm_wheel = _step_block(release, "Build musl wheel in an explicit uv venv")
    assert "CC_aarch64_unknown_linux_musl: musl-gcc" in native_arm_wheel
    assert ".github/scripts/native_release_build.py musl-wheel" in native_arm_wheel
    assert '--target "${{ matrix.target }}"' in native_arm_wheel

    native_helper = (
        REPO_ROOT / ".github" / "scripts" / "native_release_build.py"
    ).read_text(encoding="utf-8")
    assert '["uv", "venv", "--python", "3.13", str(venv)]' in native_helper
    assert "cargo_via_soldr_rustup.sh" in native_helper
    assert '"musllinux_1_2"' in native_helper

    cargo_bridge = (
        REPO_ROOT / ".github" / "scripts" / "cargo_via_soldr_rustup.sh"
    ).read_text(encoding="utf-8")
    assert (
        'exec "$SOLDR_RELEASE_DRIVER" rustup run "$SOLDR_RELEASE_TOOLCHAIN" cargo "$@"'
        in cargo_bridge
    )


def test_release_target_prepare_retries_transient_setup_failure() -> None:
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    setup = _step_block(release, "Install pinned Soldr and prepare target environment")
    retry = _step_block(
        release, "Retry target preparation after setup transport failure"
    )
    materialize = _step_block(
        release, "Materialize installed Soldr as release build driver"
    )
    wheel = _step_block(release, "Build wheel through setup-soldr target environment")

    assert "continue-on-error: ${{ matrix.setup_target != '' }}" in setup
    assert (
        "if: steps.setup_soldr.outcome == 'failure' && matrix.setup_target != ''"
        in retry
    )
    assert ".github/scripts/prepare_release_target.py" in retry
    assert '--github-env "$GITHUB_ENV"' in retry
    assert "Get-Command soldr -ErrorAction Stop" in materialize
    assert "wheel_hook='python -m build --wheel'" in wheel


def test_windows_wheel_does_not_reuse_archive_executable_output() -> None:
    """PEP 517 must not rebuild the archive lane's still-open soldr.exe."""

    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    matrix = release.split("      matrix:\n", 1)[1].split("\n    steps:\n", 1)[0]
    wheel_step = _step_block(
        release, "Build wheel through setup-soldr target environment"
    )
    wheel_smoke = _step_block(release, "Smoke test wheel")

    assert "build_driver:" not in matrix
    assert (
        "- name: Windows x64 (Linux cross)\n"
        "            runner: ubuntu-24.04\n"
        "            target: x86_64-pc-windows-msvc" in matrix
    )
    assert ".github/scripts/build_release_wheel.py" in wheel_step
    assert "--target-dir target" not in wheel_step
    assert "!contains(matrix.target, 'pc-windows-msvc')" in wheel_smoke

    for step_name in [
        "Restore executable bit on bootstrap driver",
        "Build release binary (soldr-driven)",
        "Package combined archive (tar.zst level 19)",
        "Build wheel through setup-soldr target environment",
        "Smoke test combined tar.zst archive",
    ]:
        step = _step_block(release, step_name)
        assert 'case "$RUNNER_OS" in' in step

    smoke_windows = _job_block(release, "smoke_windows", "publish")
    assert "runner: windows-2025" in smoke_windows
    assert "target: x86_64-pc-windows-msvc" in smoke_windows
    assert "runner: windows-11-arm" in smoke_windows
    assert "target: aarch64-pc-windows-msvc" in smoke_windows
    assert (
        "- name: Windows ARM64 (Linux cross)\n"
        "            runner: ubuntu-24.04\n"
        "            target: aarch64-pc-windows-msvc" in matrix
    )


def test_partial_immutable_release_can_recover_missing_pypi_wheels() -> None:
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    pypi_job = _job_block(release, "publish-pypi", "publish-npm")

    assert (
        "github_release_immutable: ${{ steps.validate.outputs.github_release_immutable }}"
        in release
    )
    assert 'gh api "repos/${GITHUB_REPOSITORY}/releases/tags/${version}"' in release
    assert "github_release_immutable != 'true'" in release
    assert "needs.verify_github_release.result == 'success'" in pypi_job
    assert "github.event_name == 'workflow_dispatch'" in pypi_job
    assert "inputs.force_pypi_publish" in pypi_job
    assert "skip-existing: true" in pypi_job


def test_cross_compile_docs_match_current_blessed_surfaces() -> None:
    docs = (REPO_ROOT / "docs" / "CROSS_COMPILE.md").read_text(encoding="utf-8")
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")

    assert "soldr build --release --target x86_64-pc-windows-msvc" in docs
    assert "soldr cargo xwin build --release" in docs
    assert "_cross-build-windows-host.yml" not in docs
    assert "cross-build-from-windows-x64-linux" not in docs
    assert "build-macos-x64.yml" not in ci
    assert "macos-15-intel" in ci
    assert "soldr#1237" not in release
    assert "x86_64-apple-darwin: **intentionally omitted**" not in release
