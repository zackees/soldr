from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NIGHTLY = "nightly-2026-05-28-x86_64-unknown-linux-gnu"
BUILD_STEPS = (
    "Build daemon process-creation boundary lint",
    "Build fetch network boundary lint",
    "Build local-socket name boundary lint",
    "Build raw IPC transport boundary lint",
    "Build platform-cfg directory boundary lint",
    "Build env-flag boundary lint",
)
TEST_STEPS = (
    "Test env-flag boundary lint",
    "Test daemon process-creation boundary lint",
    "Test fetch network boundary lint",
    "Test local-socket name boundary lint",
    "Test raw IPC transport boundary lint",
    "Test platform-cfg directory boundary lint",
)


def _step_body(workflow: str, name: str) -> str:
    marker = f"      - name: {name}\n"
    start = workflow.index(marker)
    end = workflow.find("\n      - name: ", start + len(marker))
    return workflow[start : end if end != -1 else len(workflow)]


def test_root_workspace_loads_process_boundary_dylint() -> None:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    assert "[workspace.metadata.dylint]" in manifest
    assert 'libraries = [{ path = "dylints/*" }]' in manifest
    assert (ROOT / "dylints" / "ban_raw_process_creation" / "src" / "lib.rs").is_file()


def test_required_ci_runs_root_dylint_policy() -> None:
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "Enforce daemon process-creation boundary" in workflow
    assert "Build Soldr Dylint front door" in workflow
    assert "Prepare catalogued Dylint components through Soldr" in workflow
    assert "Install catalogued Dylint command binaries" in workflow
    assert (
        "DYLINT_DRIVER_PATH: ${{ github.workspace }}/target/dylint/drivers" in workflow
    )
    assert (
        '"${GITHUB_WORKSPACE}/target/x86_64-unknown-linux-gnu/debug/soldr"' in workflow
    )
    executable = "\n".join(
        line for line in workflow.splitlines() if not line.lstrip().startswith("#")
    )
    assert "cargo install cargo-dylint" not in executable
    assert "cargo install dylint-link" not in executable
    assert "Cache Dylint binaries" not in workflow
    assert "Install Dylint toolchain" in workflow
    assert "soldr rustup toolchain install" in workflow
    assert "--component rustc-dev" in workflow
    assert "--component llvm-tools-preview" in workflow
    assert "--component rust-src" in workflow
    assert "Configure Dylint driver Cargo shim" not in workflow
    assert "Build daemon process-creation boundary lint" in workflow
    assert "Build local-socket name boundary lint" in workflow
    assert "Enforce running-process local-socket name boundary" not in workflow
    assert "Test local-socket name boundary lint" in workflow
    assert "nightly-2026-05-28-x86_64-unknown-linux-gnu" in workflow
    # soldr#2303: the driver cdylibs still build in the release profile (dylint
    # loads them from that path), now carrying the policy exemption marker.
    assert "--profile release  # allow-release:" in workflow
    assert '"${GITHUB_WORKSPACE}/target/dylint/libraries/' in workflow
    assert '"${CARGO_HOME}/bin/cargo-dylint"' in workflow
    assert "dylint --no-build --all" in workflow
    assert "-- --workspace --all-targets" in workflow
    assert "Test daemon process-creation boundary lint" in workflow
    assert "working-directory: dylints/ban_raw_process_creation" in workflow
    # soldr#2740 added the env-flag boundary lint, so its build and test
    # steps must be wired beside the others.
    assert "Build env-flag boundary lint" in workflow
    assert "Test env-flag boundary lint" in workflow
    assert "working-directory: dylints/ban_raw_env_flag" in workflow
    # Dylint intentionally keeps its pinned nightly toolchain. All six lint
    # crates share one nightly-keyed test target, without mixing those
    # artifacts into the project's Rust 1.95 target tree.
    dylint_steps = workflow.split(
        "      - name: Build daemon process-creation boundary lint", 1
    )[1].split("      - name: Assert Dylint tests used the shared target directory", 1)[0]
    assert dylint_steps.count("soldr cargo build") == 6
    assert dylint_steps.count("soldr cargo test") == 6
    library_target = f'"${{GITHUB_WORKSPACE}}/target/dylint/libraries/{NIGHTLY}"'
    test_target = f'"${{GITHUB_WORKSPACE}}/target/dylint/tests/{NIGHTLY}"'
    for name in BUILD_STEPS:
        step = _step_body(workflow, name)
        assert step.count("soldr cargo build") == 1, name
        assert "soldr cargo test" not in step, name
        assert step.count("RUSTUP_TOOLCHAIN:") == 1, name
        assert f"RUSTUP_TOOLCHAIN: {NIGHTLY}" in step, name
        assert step.count("--target-dir") == 1, name
        assert library_target in step, name
        assert test_target not in step, name
    for name in TEST_STEPS:
        step = _step_body(workflow, name)
        assert step.count("soldr cargo test") == 1, name
        assert "soldr cargo build" not in step, name
        assert step.count("RUSTUP_TOOLCHAIN:") == 1, name
        assert f"RUSTUP_TOOLCHAIN: {NIGHTLY}" in step, name
        assert step.count("--target-dir") == 1, name
        assert test_target in step, name
        assert library_target not in step, name
    assert "--manifest-path Cargo.toml" in workflow
    assert f"RUSTUP_TOOLCHAIN: {NIGHTLY}" in workflow
    # Seven for the original lints, plus soldr#2740's test step.
    assert workflow.count('SOLDR_NO_GC_TARGET: "1"') == 7
    # Thirteen for the original lints, plus soldr#2740's build and test.
    assert workflow.count("SOLDR_LINKER: default") == 14
    dylint_config = (
        ROOT / "dylints" / "ban_raw_process_creation" / ".cargo" / "config.toml"
    ).read_text(encoding="utf-8")
    assert 'rustflags = ["-C", "linker=dylint-link"]' in dylint_config
    dylint_manifest = (
        ROOT / "dylints" / "ban_raw_process_creation" / "Cargo.toml"
    ).read_text(encoding="utf-8")
    assert "[profile.release]" in dylint_manifest
    assert "opt-level = 0" in dylint_manifest
    assert "lto = false" in dylint_manifest

    for manifest_path in (ROOT / "dylints").glob("*/Cargo.toml"):
        manifest_text = manifest_path.read_text(encoding="utf-8")
        assert 'dylint_testing = "=6.0.3"' in manifest_text


def test_process_boundary_has_required_ui_fixtures() -> None:
    ui = ROOT / "dylints" / "ban_raw_process_creation" / "ui"
    expected = {
        "disallowed_spawn.rs",
        "disallowed_spawn_ufcs.rs",
        "disallowed_spawn_function_item.rs",
        "disallowed_output.rs",
        "disallowed_status.rs",
        "disallowed_tokio_status.rs",
        "disallowed_creation_flags.rs",
        "disallowed_create_process.rs",
    }
    assert expected <= {path.name for path in ui.glob("*.rs")}


def test_dylint_runtime_environment_uses_one_platform_facade() -> None:
    # soldr#2493 replaced the four cfg-duplicated impls with a single
    # runtime match on the host facts facade; no host cfg remains in
    # the cli source.
    source = (ROOT / "crates" / "soldr-cli" / "src" / "dylint_toolchain.rs").read_text(
        encoding="utf-8"
    )
    assert source.count("fn apply_driver_runtime_environment(") == 1
    assert source.count("fn apply_driver_runtime_environment_impl(") == 1
    assert source.count("#[cfg(") == source.count("#[cfg(test)]")
    for host in ("HostOs::Windows", "HostOs::Linux", "HostOs::MacOs"):
        assert host in source
    assert "match crate::platform::host::facts::os()" in source
