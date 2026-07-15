from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"


def _job_block(workflow: str, job: str, next_job: str | None = None) -> str:
    start = workflow.index(f"  {job}:\n")
    end = workflow.index(f"  {next_job}:\n", start) if next_job else len(workflow)
    return workflow[start:end]


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
    assert (
        'if [[ "$target" == *-pc-windows-msvc ]] \\\n'
        '             || [[ "$target" == *-apple-darwin ]]; then\n'
        '            soldr build --profile "$ci_profile"' in cross
    )
    assert "soldr cargo nextest archive" in cross
    assert "soldr_args+=(--no-cache)" not in cross
    assert "Validate warm Windows cache restoration" in cross
    assert "windows_msvc_cache_roundtrip.py" in cross
    assert "--phase build" in cross
    assert "--phase archive" in cross
    assert (
        "--no-cache" not in cross[cross.index("- name: Cross-build release binary") :]
    )
    assert "cache: ${{ (contains(inputs.target, 'pc-windows-msvc')" in cross
    assert 'expected binary missing: $binary; searching target tree' in cross
    assert 'find target -type f \\( -name "soldr" -o -name "soldr.exe" \\)' in cross
    assert 'normalized binary layout:' in cross
    assert '-path "*/$ci_profile/deps/soldr"' in cross
    assert 'dd if="$binary" bs=1 count=2' in cross
    assert (
        "              soldr --no-cache cargo xwin build "
        "--target x86_64-pc-windows-msvc\n"
        "              ls -l target/x86_64-pc-windows-msvc/debug/hellowin.exe"
        in baseline
    )
    for first_party_package in [
        "soldr-cli",
        "soldr-core",
        "soldr-fetch",
        "soldr-cache",
        "soldr-daemon",
    ]:
        assert f"-p {first_party_package}" in cross

    assert "test archive missing" not in target_run
    assert '"$SOLDR_BIN" --version' not in target_run
    assert '"$NEXTEST_BIN" nextest run' in target_run
    assert 'echo "SOLDR_BIN=$soldr_bin"' in target_run
    assert 'case \'${{ inputs.target }}\' in *-pc-windows-msvc) suffix=".exe"' in target_run
    assert 'artifact/package/soldr$suffix' in target_run
    assert 'artifact/package/tools/cargo-nextest$suffix' in target_run
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
        "soldr cargo test --workspace --lib --tests --locked "
        "--target ${{ inputs.target }}"
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
    assert offenders == []


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

    assert "cross-targets: ${{ inputs.target }}" in cross
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

    assert cross.count("--json") >= 3
    assert cross.count("catalogue sha256 mismatch") >= 3
    assert "--json cargo-zigbuild" in baseline
    assert "cargo-zigbuild catalogue sha256 mismatch" in baseline
    assert "download_catalogued_asset.py" in fetch
    assert "sha256 mismatch" in downloader


def test_linux_zig_cross_lanes_use_current_checkout_soldr_bootstrap() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    lane_names = [
        ("e2e-linux-arm64-build", "e2e-linux-arm64"),
        ("e2e-linux-x64-musl-build", "e2e-linux-x64-musl"),
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


def test_windows_gnu_validation_runs_bounded_pr_runtime_smoke() -> None:
    workflow = (WORKFLOWS / "windows-gnu-mingw-validation.yml").read_text(
        encoding="utf-8"
    )
    assert "pull_request:" in workflow
    assert '"crates/**"' in workflow
    assert "soldr build --release --target $env:TARGET --package soldr-cli" in workflow
    assert "objdump -f $binary" in workflow
    assert "& $binary --version" in workflow
    assert "pei-x86-64" in workflow
    assert "if: github.event_name != 'pull_request'" in workflow


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
    for friendly in ["windows-x86", "windows-arm"]:
        start = cross_all.index(f"          - friendly: {friendly}\n")
        next_entry = cross_all.find("          - friendly:", start + 1)
        end = next_entry if next_entry != -1 else len(cross_all)
        assert "tool: soldr-build" in cross_all[start:end]


def test_mac_x64_distribution_is_cross_built_and_intel_smoke_tested() -> None:
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

    assert (
        "- name: macOS x64 (cross-compiled)\n"
        "            runner: ubuntu-24.04\n"
        "            target: x86_64-apple-darwin" in release
    )
    assert '"x86_64-apple-darwin": {"os": "darwin", "arch": "x86_64"}' in release
    assert 'prepare --target "$target" --github-env "$GITHUB_ENV"' in release
    assert "smoke_macos_x64:" in release
    assert "runs-on: macos-15-intel" in release
    assert "Mach-O 64-bit executable x86_64" in release
    assert "lipo -archs extracted/soldr" in release
    assert "needs.smoke_macos_x64.result == 'success'" in release
    assert "soldr-${version}-x86_64-apple-darwin.tar.zst" in release
    assert "soldr-${cargo_version}-py3-none-macosx_11_0_x86_64.whl" in release

    assert '"darwin-x64": { triple: "x86_64-apple-darwin"' in install
    assert "intentionally not published" not in install
    assert "x86_64-apple-darwin" in npm_docs
    assert "macos-15-intel" in npm_docs
    assert "x86_64-apple-darwin" in verification_docs
    assert "Mach-O x86_64" in verification_docs


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
