from __future__ import annotations

import ast
import importlib.util
import json
import re
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "actions" / "setup-soldr" / "resolve_setup.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("resolve_setup", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec is not None
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def test_load_toolchain_spec_reads_rust_toolchain_toml() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "")

    assert spec["channel"] == "1.94.1"
    assert spec["profile"] == "minimal"
    assert "rustfmt" in spec["components"]
    assert "clippy" in spec["components"]
    assert spec["source"] == "rust-toolchain.toml"
    assert spec["file_hash"] != "none"


def test_load_toolchain_spec_defaults_when_file_missing(tmp_path: Path) -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(tmp_path, "missing.toml", "")

    assert spec == {
        "channel": "stable",
        "profile": "minimal",
        "components": [],
        "targets": [],
        "source": "default",
        "file_hash": "none",
    }


def test_load_toolchain_spec_prefers_explicit_override() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "stable")

    assert spec["channel"] == "stable"
    assert spec["profile"] == "minimal"
    assert spec["source"] == "input"


def test_normalize_list_accepts_csv_strings() -> None:
    module = _load_module()

    value = module._normalize_list("rustfmt, clippy, ")

    assert value == ["rustfmt", "clippy"]


def test_toolchain_signature_payload_is_json_serializable() -> None:
    module = _load_module()

    spec = module.load_toolchain_spec(REPO_ROOT, "rust-toolchain.toml", "")

    payload = {
        "channel": spec["channel"],
        "profile": spec["profile"],
        "components": spec["components"],
        "targets": spec["targets"],
        "source": spec["source"],
        "file_hash": spec["file_hash"],
        "soldr_repo": "zackees/soldr",
        "soldr_version": "0.7.4",
    }

    assert json.loads(json.dumps(payload))["channel"] == "1.94.1"


def test_action_python_helpers_have_entrypoints() -> None:
    action_text = (REPO_ROOT / "action.yml").read_text(encoding="utf-8")
    script_paths = sorted(
        {
            REPO_ROOT / match.group(1)
            for match in re.finditer(
                r"github\.action_path \}\}/([^\"']+\.py)", action_text
            )
        }
    )

    assert script_paths, "expected action.yml to invoke Python helper scripts"
    for script_path in script_paths:
        tree = ast.parse(script_path.read_text(encoding="utf-8"))
        assert any(
            isinstance(node, ast.FunctionDef) and node.name == "main"
            for node in tree.body
        ), f"{script_path.relative_to(REPO_ROOT)} should define main()"
        assert any(
            isinstance(node, ast.If)
            and isinstance(node.test, ast.Compare)
            and isinstance(node.test.left, ast.Name)
            and node.test.left.id == "__name__"
            and any(
                isinstance(comparator, ast.Constant) and comparator.value == "__main__"
                for comparator in node.test.comparators
            )
            for node in tree.body
        ), f"{script_path.relative_to(REPO_ROOT)} should invoke main() as a script"


def test_setup_soldr_smoke_tests_disable_nested_cache() -> None:
    workflow = (
        REPO_ROOT / ".github" / "workflows" / "setup-soldr-action.yml"
    ).read_text(encoding="utf-8")

    assert "Remove-Item Env:ZCCACHE_CACHE_DIR" in workflow
    assert "soldr --no-cache cargo test -p soldr-cli --test cli --locked" in workflow
    assert "id: dogfood-build-cache" in workflow
    assert "setup-soldr-dogfood-zccache-v1-" in workflow
    assert "dogfood-build-cache-hit=" in workflow
    assert "dogfood-build-seconds=" in workflow
    assert "dogfood-test-seconds=" in workflow
    assert "Stop dogfood zccache before cache save" in workflow
    assert "& $zccache stop" in workflow


def test_main_creates_cache_layout_and_outputs(tmp_path: Path, monkeypatch) -> None:
    module = _load_module()
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", "")
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_DIR", "custom-target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")

    module.main()

    cache_root = runner_temp / "setup-soldr"
    assert cache_root.is_dir()
    assert (cache_root / "soldr").is_dir()
    assert (cache_root / "soldr" / "cache").is_dir()
    assert (cache_root / "soldr" / "cache" / "zccache").is_dir()
    assert (cache_root / "soldr" / "bin").is_dir()
    assert (cache_root / "cargo").is_dir()
    assert (cache_root / "cargo" / "bin").is_dir()
    assert (cache_root / "rustup").is_dir()
    assert (cache_root / "bin").is_dir()
    assert (cache_root / "shims").is_dir()

    outputs = github_output.read_text(encoding="utf-8")
    assert f"cache_root={cache_root}" in outputs
    assert "cache_key=setup-soldr-v0-linux-x64-" in outputs
    assert "cache_restore_prefix=setup-soldr-v0-linux-x64-" in outputs
    assert "build_cache_key=setup-soldr-buildcache-v1-linux-x64-" in outputs
    assert (
        "build_cache_restore_key_toolchain=setup-soldr-buildcache-v1-linux-x64-"
        in outputs
    )
    assert (
        "build_cache_restore_key_os_arch=setup-soldr-buildcache-v1-linux-x64-"
        in outputs
    )
    assert f"build_cache_path={cache_root / 'soldr' / 'cache' / 'zccache'}" in outputs
    assert f"target_cache_path={workspace / 'custom-target'}" in outputs
    bundle_path = runner_temp / "setup-soldr-target-thin"
    assert f"target_cache_bundle_path={bundle_path}" in outputs
    assert f"shim_dir={cache_root / 'shims'}" in outputs
    assert "target_cache_key=setup-soldr-targetcache-thin-v1-linux-x64-" in outputs
    assert "target_cache_enabled=false" in outputs
    assert "target_cache_mode=thin" in outputs
    assert f"target_cache_paths={bundle_path}" in outputs
    assert outputs.count("-abc123") >= 1
    assert "toolchain=stable" in outputs

    env_text = github_env.read_text(encoding="utf-8")
    assert f"SOLDR_CACHE_DIR={cache_root / 'soldr'}" in env_text
    assert f"ZCCACHE_CACHE_DIR={cache_root / 'soldr' / 'cache' / 'zccache'}" in env_text
    assert "SOLDR_TARGET_CACHE_MODE=off" in env_text
    assert "SOLDR_TARGET_CACHE_BACKEND=auto" in env_text


def test_main_treats_hot_as_thin_alias_with_lockfile(
    tmp_path: Path, monkeypatch
) -> None:
    module = _load_module()
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()
    (workspace / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    (workspace / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", "")
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_CACHE", "true")
    monkeypatch.setenv("INPUT_TARGET_CACHE_MODE", "hot")
    monkeypatch.setenv("INPUT_TARGET_DIR", "target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")

    module.main()

    cache_root = runner_temp / "setup-soldr"
    bundle_path = runner_temp / "setup-soldr-target-thin"
    outputs = github_output.read_text(encoding="utf-8")
    assert "target_cache_enabled=true" in outputs
    assert "target_cache_mode=thin" in outputs
    assert "target_cache_key=setup-soldr-targetcache-thin-v1-linux-x64-" in outputs
    assert f"target_cache_paths={bundle_path}" in outputs
    assert (
        "target_cache_restore_key_lock=setup-soldr-targetcache-thin-v1-linux-x64-"
        in outputs
    )

    env_text = github_env.read_text(encoding="utf-8")
    assert "SOLDR_TARGET_CACHE_MODE=thin" in env_text
    assert f"SOLDR_TARGET_CACHE_DIR={workspace / 'target'}" in env_text
    assert f"SOLDR_TARGET_CACHE_BUNDLE_DIR={bundle_path}" in env_text
    assert "SOLDR_TARGET_CACHE_BACKEND=auto" in env_text
    assert bundle_path == runner_temp / "setup-soldr-target-thin"


def _setup_main_env(
    tmp_path: Path,
    monkeypatch,
    *,
    version: str = "",
) -> tuple[Path, Path, Path]:
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", version)
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_DIR", "target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    return github_env, github_output, github_path


def _collect_outputs(output_path: Path) -> dict[str, str]:
    return dict(
        line.split("=", 1)
        for line in output_path.read_text(encoding="utf-8").splitlines()
        if "=" in line
    )


def test_latest_version_cache_key_changes_when_release_changes(
    tmp_path: Path, monkeypatch
) -> None:
    """When input is `latest`, the cache key must be derived from the
    resolved release tag so that a new soldr release invalidates the
    setup cache and managed zccache binaries get refreshed. See #214."""
    module = _load_module()

    calls = {"count": 0}

    def fake_resolver_0(_repo: str) -> str:
        calls["count"] += 1
        return "0.7.11"

    run_dir_a = tmp_path / "run-a"
    run_dir_a.mkdir()
    _, output_a, _ = _setup_main_env(run_dir_a, monkeypatch)
    monkeypatch.setattr(module, "resolve_latest_soldr_release", fake_resolver_0)
    module.main()
    outputs_a = _collect_outputs(output_a)

    run_dir_b = tmp_path / "run-b"
    run_dir_b.mkdir()
    _, output_b, _ = _setup_main_env(run_dir_b, monkeypatch)
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "0.7.12")
    module.main()
    outputs_b = _collect_outputs(output_b)

    assert outputs_a["cache_key"] != outputs_b["cache_key"]
    assert outputs_a["soldr_version_resolved"] == "0.7.11"
    assert outputs_b["soldr_version_resolved"] == "0.7.12"
    assert outputs_a["soldr_version_requested"] == ""
    assert calls["count"] == 1


def test_latest_version_cache_key_falls_back_to_literal_when_resolution_fails(
    tmp_path: Path, monkeypatch
) -> None:
    """If the GitHub API is unreachable, the resolver returns an empty
    string; the action must still produce a deterministic cache key so
    the warm-run pipeline keeps working. See #214."""
    module = _load_module()

    run_dir_a = tmp_path / "run-a"
    run_dir_a.mkdir()
    _, output_a, _ = _setup_main_env(run_dir_a, monkeypatch)
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()
    outputs_a = _collect_outputs(output_a)

    run_dir_b = tmp_path / "run-b"
    run_dir_b.mkdir()
    _, output_b, _ = _setup_main_env(run_dir_b, monkeypatch)
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")
    module.main()
    outputs_b = _collect_outputs(output_b)

    assert outputs_a["cache_key"] == outputs_b["cache_key"]
    assert outputs_a["soldr_version_resolved"] == ""


def test_pinned_version_input_skips_latest_resolution(
    tmp_path: Path, monkeypatch
) -> None:
    """A pinned `version: 0.7.10` input must not trigger a GitHub API
    call and must bake that exact version into the cache key."""
    module = _load_module()
    calls = {"count": 0}

    def _should_not_call(_repo: str) -> str:
        calls["count"] += 1
        return "should-not-be-used"

    _, github_output, _ = _setup_main_env(tmp_path, monkeypatch, version="0.7.10")
    monkeypatch.setattr(module, "resolve_latest_soldr_release", _should_not_call)
    module.main()

    outputs = _collect_outputs(github_output)
    assert calls["count"] == 0
    assert outputs["soldr_version_resolved"] == "0.7.10"
    assert outputs["soldr_version_requested"] == "0.7.10"


def test_normalize_release_tag_strips_v_prefix() -> None:
    module = _load_module()
    assert module._normalize_release_tag("v0.7.11") == "0.7.11"
    assert module._normalize_release_tag("0.7.11") == "0.7.11"
    assert module._normalize_release_tag(" v1.0.0 ") == "1.0.0"
    assert module._normalize_release_tag("") == ""


def test_is_empty_or_latest_matches_expected_values() -> None:
    module = _load_module()
    assert module._is_empty_or_latest("")
    assert module._is_empty_or_latest("  ")
    assert module._is_empty_or_latest("latest")
    assert module._is_empty_or_latest("LATEST")
    assert not module._is_empty_or_latest("v0.7.10")
    assert not module._is_empty_or_latest("0.7.10")


def test_full_target_cache_suffix_restore_prefix_matches_key_shape(
    tmp_path: Path, monkeypatch
) -> None:
    module = _load_module()
    workspace = tmp_path / "workspace"
    runner_temp = tmp_path / "runner-temp"
    workspace.mkdir()
    runner_temp.mkdir()
    (workspace / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    (workspace / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")

    github_env = tmp_path / "github.env"
    github_output = tmp_path / "github.output"
    github_path = tmp_path / "github.path"

    monkeypatch.setenv("ACTION_WORKSPACE", str(workspace))
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    monkeypatch.setenv("ACTION_OS", "Linux")
    monkeypatch.setenv("ACTION_ARCH", "X64")
    monkeypatch.setenv("INPUT_REPO", "zackees/soldr")
    monkeypatch.setenv("INPUT_VERSION", "")
    monkeypatch.setenv("INPUT_CACHE_DIR", "")
    monkeypatch.setenv("INPUT_CACHE_KEY_SUFFIX", "feature-a")
    monkeypatch.setenv("INPUT_TOOLCHAIN", "")
    monkeypatch.setenv("INPUT_TOOLCHAIN_FILE", "missing.toml")
    monkeypatch.setenv("INPUT_TRUST_MODE", "")
    monkeypatch.setenv("INPUT_TARGET_CACHE", "true")
    monkeypatch.setenv("INPUT_TARGET_CACHE_MODE", "full")
    monkeypatch.setenv("INPUT_TARGET_DIR", "target")
    monkeypatch.setenv("GITHUB_SHA", "abc123")
    monkeypatch.setenv("GITHUB_ENV", str(github_env))
    monkeypatch.setenv("GITHUB_OUTPUT", str(github_output))
    monkeypatch.setenv("GITHUB_PATH", str(github_path))
    monkeypatch.setattr(module, "resolve_latest_soldr_release", lambda _repo: "")

    module.main()

    outputs = dict(
        line.split("=", 1)
        for line in github_output.read_text(encoding="utf-8").splitlines()
        if "=" in line
    )
    key = outputs["target_cache_key"]
    restore_prefix = outputs["target_cache_restore_key_lock"]
    assert key.startswith(restore_prefix)
    assert key.endswith("abc123")
    assert "feature-a" in restore_prefix
