"""Contract tests for the Alpine musl-host acceptance command."""

import importlib.util
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "ci" / "alpine_musl_host_e2e.py"
_spec = importlib.util.spec_from_file_location("alpine_musl_host_e2e", SCRIPT)
assert _spec and _spec.loader
module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(module)


def test_acceptance_command_installs_only_documented_host_prerequisites() -> None:
    command = module.container_command("soldr-baseline-musl:test", "token")
    assert command[:5] == ["docker", "run", "--rm", "--env", "SOLDR_GITHUB_TOKEN=token"]
    script = command[-1]
    assert "apk add --no-cache gcc musl-dev" in script
    assert "for tool in cc gcc rustc cargo; do" in script
    assert "unexpected preinstalled $tool on Alpine host" in script
    assert "soldr toolchain ensure --json" in script
    assert "soldr cargo build --target x86_64-unknown-linux-musl" in script
    assert "if command -v zig >/dev/null; then" in script
    assert "if find \"${SOLDR_HOME}\" -iname '*zig*'" in script
