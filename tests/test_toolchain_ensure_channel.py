"""The target-run lane must pin the channel, not just two paths (soldr#2879).

`CARGO`/`RUSTC` are absolute paths into the provisioned toolchain. They pin
what the job runs directly and nothing about what the soldr children spawned by
fixtures resolve — those walk ancestors for a `rust-toolchain.toml`, find none
under the OS temp dir, and take rustup's default. On the darwin runners that is
`stable`, whose `rust-objcopy` cannot load `@rpath/libLLVM.dylib`, which failed
three tests in a job that had just installed and smoke-verified 1.95.0.

These cover the reader and the wiring. The reader refuses to guess a channel:
exporting a wrong `RUSTUP_TOOLCHAIN` would be worse than exporting none, since
it would silently redirect every child instead of leaving today's behaviour.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
import yaml
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "toolchain_ensure_channel.py"
TARGET_RUN = REPO_ROOT / ".github" / "workflows" / "_ci-target-run.yml"

reader = load_script_module(SCRIPT, "toolchain_ensure_channel")


def write_payload(tmp_path: Path, body: object) -> Path:
    path = tmp_path / "ensure.json"
    path.write_text(json.dumps(body), encoding="utf-8")
    return path


def test_the_channel_is_read_from_the_payload() -> None:
    assert reader.channel_from({"schema_version": 1, "channel": "1.95.0"}) == "1.95.0"


def test_surrounding_whitespace_is_trimmed() -> None:
    # It becomes `RUSTUP_TOOLCHAIN=<value>` in a `$GITHUB_ENV` line, where a
    # stray space would travel into rustup as part of the channel name.
    assert reader.channel_from({"channel": "  1.95.0\n"}) == "1.95.0"


@pytest.mark.parametrize(
    "payload",
    [
        {"schema_version": 1},
        {"channel": None},
        {"channel": ""},
        {"channel": "   "},
        {"channel": 195},
        ["not", "an", "object"],
        None,
    ],
)
def test_a_payload_without_a_channel_yields_none(payload: object) -> None:
    """`channel` is `Option<String>` upstream, so absent is a real answer.

    Returning a default here would export a channel nobody provisioned.
    """
    assert reader.channel_from(payload) is None


def test_main_fails_loudly_rather_than_printing_nothing(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    # The workflow assigns this to a shell variable. Exiting 0 with empty
    # output would export `RUSTUP_TOOLCHAIN=` — an empty pin, which reads as
    # "unset" to some consumers and as a broken channel name to others.
    assert reader.main([str(write_payload(tmp_path, {"schema_version": 1}))]) == 1
    assert "refusing to guess" in capsys.readouterr().err


def test_main_prints_the_channel_alone(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    payload = write_payload(tmp_path, {"schema_version": 1, "channel": "1.95.0"})
    assert reader.main([str(payload)]) == 0
    assert capsys.readouterr().out.strip() == "1.95.0"


def test_an_unreadable_payload_is_an_error_not_a_silent_default(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    assert reader.main([str(tmp_path / "missing.json")]) == 1
    assert "could not read" in capsys.readouterr().err


def target_run_steps() -> list[dict]:
    doc = yaml.safe_load(TARGET_RUN.read_text(encoding="utf-8"))
    return next(iter(doc["jobs"].values()))["steps"]


def test_the_lane_exports_the_channel_beside_cargo_and_rustc() -> None:
    step = next(
        step
        for step in target_run_steps()
        if "toolchain_ensure_channel.py" in (step.get("run") or "")
    )
    run = step["run"]
    assert "RUSTUP_TOOLCHAIN=$channel" in run, run
    # Beside, not instead of: the absolute paths still pin what the job runs
    # directly, and dropping them would trade one gap for another.
    assert "CARGO=$cargo_bin" in run, run
    assert "RUSTC=$rustc_bin" in run, run


def test_an_empty_channel_stops_the_job() -> None:
    """`test -n` is what keeps a failed read from exporting an empty pin."""
    step = next(
        step
        for step in target_run_steps()
        if "toolchain_ensure_channel.py" in (step.get("run") or "")
    )
    assert 'test -n "$channel"' in step["run"], step["run"]


# ---- the stream that actually arrives (soldr#2879) --------------------------
#
# `--json` does not mean "only JSON on stdout": the child rustup inherits
# soldr's stdout, so the payload is preceded by rustup's own lines. Captured
# verbatim from `soldr toolchain ensure --json` in the Linux runner.

REAL_STREAM = (
    "\n"
    "  1.95.0-x86_64-unknown-linux-gnu unchanged - rustc 1.95.0 (59807616e 2026-04-14)\n"
    "\n"
    '{\n  "schema_version": 1,\n  "channel": "1.95.0",\n  "elapsed_ms": 1041\n}\n'
)


def test_a_rustup_preamble_does_not_hide_the_payload() -> None:
    """The exact failure that made the darwin lane reject a good payload.

    `json.load` on this raises `Extra data: line 2 column 7 (char 7)` — line 1
    parses as nothing and the decoder trips on the rustup line.
    """
    assert reader.channel_from(reader.extract_payload(REAL_STREAM)) == "1.95.0"


def test_the_preamble_is_what_plain_json_load_cannot_take() -> None:
    # Pins the reason the tolerance exists. If `--json` is ever fixed to emit
    # only JSON, this test starts failing and says so, rather than the
    # tolerance quietly outliving its cause.
    with pytest.raises(json.JSONDecodeError):
        json.loads(REAL_STREAM)


def test_a_stream_with_no_object_is_still_an_error() -> None:
    # Tolerant of a preamble, not of a missing payload.
    with pytest.raises(json.JSONDecodeError):
        reader.extract_payload("rustup said things and then stopped\n")


def test_trailing_output_after_the_object_is_ignored() -> None:
    # `raw_decode` stops at the end of the first object, so anything rustup
    # prints afterwards cannot break the read either.
    payload = reader.extract_payload(
        '{"schema_version": 1, "channel": "1.95.0"}\ninfo: done\n'
    )
    assert reader.channel_from(payload) == "1.95.0"


def test_main_reads_the_real_stream_end_to_end(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    path = tmp_path / "ensure.json"
    path.write_text(REAL_STREAM, encoding="utf-8")
    assert reader.main([str(path)]) == 0
    assert capsys.readouterr().out.strip() == "1.95.0"
