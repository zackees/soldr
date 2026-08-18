from __future__ import annotations

from pathlib import Path


def test_smoke_local_is_the_dedicated_full_pipeline_entry_point() -> None:
    script = (Path(__file__).parents[1] / "ci" / "smoke_local.py").read_text(
        encoding="utf-8"
    )

    assert 'perf_local.main(["smoke"])' in script
    assert 'perf_local.main(["smoke-console"])' in script


def test_smoke_bootstraps_then_dogfoods_current_source() -> None:
    script = (Path(__file__).parents[1] / "ci" / "smoke_local.sh").read_text(
        encoding="utf-8"
    )

    bootstrap = "soldr cargo build -p soldr-cli --bin soldr"
    dogfood = 'export PATH="$(dirname "$current_soldr"):$PATH"'
    assert bootstrap in script
    assert dogfood in script
    assert (
        script.index(bootstrap)
        < script.index(dogfood)
        < script.index("exec bash ./test")
    )
    assert 'if [[ -x "$current_soldr" ]]' in script
    assert '"$current_soldr" cargo build -p soldr-cli --bin soldr' in script
    assert 'export TMPDIR="${CARGO_TARGET_DIR:-/target}/tmp"' in script
    assert 'mkdir -p "$TMPDIR"' in script
    assert "SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS=20" in script
    assert "smoke-tokio-console.json" in script
    assert "smoke-tokio-console.stop" in script
    assert 'wait "$dump_pid"' in script
