from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


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
    assert "Enforce running-process local-socket name boundary" in workflow
    assert "Test local-socket name boundary lint" in workflow
    assert "nightly-2026-05-28-x86_64-unknown-linux-gnu" in workflow
    # soldr#2303: the driver cdylibs still build in the release profile (dylint
    # loads them from that path), now carrying the policy exemption marker.
    assert "--profile release  # allow-release:" in workflow
    assert '"${GITHUB_WORKSPACE}/target/dylint/libraries/' in workflow
    assert '"${CARGO_HOME}/bin/cargo-dylint"' in workflow
    assert "dylint --no-build --all" in workflow
    assert "-- --workspace --all-targets" in workflow
    assert "--manifest-path _vender/running-process/Cargo.toml" in workflow
    assert (
        "libban_raw_local_socket_name@" "nightly-2026-05-28-x86_64-unknown-linux-gnu.so"
    ) in workflow
    assert "Test daemon process-creation boundary lint" in workflow
    assert "working-directory: dylints/ban_raw_process_creation" in workflow
    # All five boundary lints build and test in the required CI lane.
    assert workflow.count("soldr rustup run") == 6
    assert (
        "nightly-2026-05-28-x86_64-unknown-linux-gnu\n"
        "          cargo test\n"
        "          --manifest-path Cargo.toml"
    ) in workflow
    assert "--manifest-path Cargo.toml" in workflow
    assert "RUSTUP_TOOLCHAIN: nightly-2026-05-28-x86_64-unknown-linux-gnu" in workflow
    assert workflow.count('SOLDR_NO_GC_TARGET: "1"') == 7
    assert workflow.count("SOLDR_LINKER: default") == 13
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
    source = (ROOT / "crates" / "soldr-cli" / "src" / "dylint_toolchain.rs").read_text(
        encoding="utf-8"
    )
    assert source.count("fn apply_driver_runtime_environment(") == 1
    assert source.count("fn apply_driver_runtime_environment_impl(") == 4
    for platform in ("windows", "macos", "linux"):
        assert f'#[cfg(target_os = "{platform}")]' in source
    assert "#[cfg(any(" not in source
    assert 'not(target_os = "windows")' in source
    assert "Dylint drivers are supported only" in source
