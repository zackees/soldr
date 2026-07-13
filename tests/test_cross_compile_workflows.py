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
    arm_build = _job_block(ci, "e2e-windows-arm64-build", "e2e-windows-arm64")
    arm_run = _job_block(ci, "e2e-windows-arm64")
    assert "if: false" not in arm_build
    assert "if: false" not in arm_run

    assert "if: (!contains(inputs.target, 'pc-windows-msvc'))" not in cross
    assert "soldr cargo nextest archive" in cross
    assert (
        "- name: Isolate Windows cross-target zccache\n"
        "        if: contains(inputs.target, 'pc-windows-msvc')\n"
        "        shell: bash"
        in cross
    )
    assert (
        'echo "ZCCACHE_CACHE_DIR=$RUNNER_TEMP/soldr-windows-cross-zccache-'
        '${{ inputs.target }}" >> "$GITHUB_ENV"'
        in cross
    )
    isolation_step = cross.index("- name: Isolate Windows cross-target zccache")
    setup_step = cross.index("- name: Setup soldr build cache")
    cook_step = cross.index("- name: Restore cooked dependency cache")
    assert isolation_step < setup_step < cook_step
    assert (
        'if [[ "$target" == *-pc-windows-msvc ]] \\\n'
        '             || [[ "$target" == *-apple-darwin ]]; then\n'
        '            soldr build --profile "$ci_profile"'
        in cross
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
    assert (
        "SOLDR_BIN: ${{ github.workspace }}/artifact/package/soldr"
        "${{ contains(inputs.target, 'pc-windows-msvc') && '.exe' || '' }}"
        in target_run
    )
    assert 'SOLDR_BIN="${SOLDR_BIN}.exe"' not in target_run
    for artifact_only_incompatible in [
        "!test(=embedded_wrapper_path_has_no_standalone_compile_telemetry_calls)",
        "!test(=gc_list_json_reports_built_project_target_dir)",
        "!test(=wrapper_mode_stdin_source_propagates_nonzero_exit_code)",
        "!binary(=cli_exec)",
        "!binary(=cli_toolchain_doctor)",
    ]:
        assert artifact_only_incompatible in target_run
    assert "fetch_catalogued_nextest.py" in cross
    assert "cargo-nextest.json" in target_run
    assert "taiki-e/install-action" not in target_run


def test_cross_workflow_bootstraps_toolchain_dependencies_through_soldr() -> None:
    cross = (WORKFLOWS / "_ci-cross-build-linux.yml").read_text(encoding="utf-8")

    assert "soldr rustup target add ${{ inputs.target }}" in cross
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

    download = cross[cross.index("      - name: Download shared bootstrap soldr artifact") :]
    download = download[: download.index("      - name:", 10)]
    assert "inputs.bootstrap_artifact_name != ''" in download
    assert "contains(inputs.target" not in download

    expose = cross[cross.index("      - name: Expose bootstrap soldr on PATH") :]
    expose = expose[: expose.index("      #", 10)]
    assert "inputs.bootstrap_artifact_name != ''" in expose


def test_manual_cross_compile_workflows_use_blessed_supported_targets() -> None:
    build_all = (WORKFLOWS / "build-all-from-linux.yml").read_text(encoding="utf-8")
    cross_all = (WORKFLOWS / "cross-compile-all-targets.yml").read_text(encoding="utf-8")

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


def test_mac_x64_cross_build_and_release_policy_are_explicit() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    release = (WORKFLOWS / "release-auto.yml").read_text(encoding="utf-8")
    install = (REPO_ROOT / "scripts" / "install.js").read_text(encoding="utf-8")
    npm_docs = (REPO_ROOT / "docs" / "NPM_PUBLISHING.md").read_text(encoding="utf-8")
    verification_docs = (REPO_ROOT / "docs" / "RELEASE_VERIFICATION.md").read_text(encoding="utf-8")

    mac_build = _job_block(ci, "e2e-macos-x64-build", "e2e-macos-x64")
    assert "if: false" not in mac_build
    assert "target: x86_64-apple-darwin" in mac_build

    assert "x86_64-apple-darwin is intentionally omitted" in release
    assert '"darwin-x64"' in install
    assert "intentionally not published" in install
    assert "x86_64-apple-darwin" in npm_docs
    assert "intentionally not published" in npm_docs
    assert "x86_64-apple-darwin" in verification_docs
    assert "intentionally omitted" in verification_docs
