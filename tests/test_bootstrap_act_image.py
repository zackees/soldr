"""Docker-based integration test that reproduces issue #406.

Spawns `catthehacker/ubuntu:act-24.04` (the default medium image nektos/act
uses), mounts a locally-built linux soldr binary, and verifies that

    SOLDR_CACHE_DIR=/tmp/soldr soldr bootstrap

successfully installs rustup into the soldr-managed bin dir on an image that
has no preinstalled toolchain manager. Without the fix from issue #406, the
soldr CLI would exit 127 with `rustup: command not found` the moment it tried
to resolve a toolchain binary.

The test is opt-in: it requires docker, pulls a ~1 GB image, and downloads
rustup-init over the network. Run with::

    uv run pytest tests/test_bootstrap_act_image.py --act-integration

or::

    uv run pytest -m act_integration

The test is skipped by default via `tests/conftest.py`.

The mounted binary must be a Linux ELF — building soldr on Windows produces an
`.exe` that the test will refuse to use, so this test only meaningfully runs on
linux x86_64 hosts (or any host that has produced a linux x86_64 soldr binary
at `target/x86_64-unknown-linux-gnu/{debug,release}/soldr`).
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ACT_IMAGE = "catthehacker/ubuntu:act-24.04"
REPO_ROOT = Path(__file__).resolve().parents[1]


def _locate_linux_soldr_binary() -> Path | None:
    """Find a locally-built linux x86_64 soldr binary, if any."""
    candidates = [
        REPO_ROOT / "target" / "x86_64-unknown-linux-gnu" / "release" / "soldr",
        REPO_ROOT / "target" / "x86_64-unknown-linux-gnu" / "debug" / "soldr",
        REPO_ROOT / "target" / "release" / "soldr",
        REPO_ROOT / "target" / "debug" / "soldr",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def _docker_available() -> bool:
    if shutil.which("docker") is None:
        return False
    try:
        result = subprocess.run(
            ["docker", "info"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


@pytest.mark.act_integration
def test_soldr_bootstrap_installs_rustup_on_act_image(tmp_path: Path) -> None:
    if not _docker_available():
        pytest.skip("docker daemon not reachable")

    soldr_bin = _locate_linux_soldr_binary()
    if soldr_bin is None:
        pytest.skip(
            "no linux soldr binary found at "
            "target/x86_64-unknown-linux-gnu/{release,debug}/soldr or "
            "target/{release,debug}/soldr — build with `cargo build "
            "--release --target x86_64-unknown-linux-gnu -p soldr-cli` "
            "first (on a linux host, or via `docker run rust:1.94.1-slim` "
            "from a Windows host with Docker Desktop)."
        )

    soldr_cache = tmp_path / "soldr-cache"
    soldr_cache.mkdir()

    cmd = [
        "docker",
        "run",
        "--rm",
        "--network=bridge",
        "-v",
        f"{soldr_bin}:/usr/local/bin/soldr:ro",
        "-v",
        f"{soldr_cache}:/soldr",
        "-e",
        "SOLDR_CACHE_DIR=/soldr",
        # Belt-and-braces: explicitly disable the opt-out so the test exercises
        # the bootstrap path even if a future default flips.
        "-e",
        "SOLDR_NO_BOOTSTRAP=0",
        ACT_IMAGE,
        "bash",
        "-c",
        # Repro the exact failure surface from issue #406: image has no rustup
        # preinstalled, soldr must bootstrap one. We then verify the managed
        # rustup binary exists and is executable.
        "set -euxo pipefail; "
        "if command -v rustup >/dev/null; then "
        '  echo "FAIL: act image unexpectedly ships rustup" >&2; exit 99; '
        "fi; "
        "soldr bootstrap --json; "
        "test -x /soldr/bin/rustup; "
        "/soldr/bin/rustup --version",
    ]

    result = subprocess.run(
        cmd, capture_output=True, text=True, timeout=600, check=False
    )
    assert result.returncode == 0, (
        f"docker run failed (exit {result.returncode})\n"
        f"stdout:\n{result.stdout}\n"
        f"stderr:\n{result.stderr}"
    )
    # JSON line should report a real install, not the idempotent already_installed=true.
    assert '"already_installed": false' in result.stdout, (
        "expected first-run install report; stdout was:\n" + result.stdout
    )
    assert (
        "rustup" in result.stdout
    ), "expected `rustup --version` output to mention rustup"
