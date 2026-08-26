#!/usr/bin/env python3
"""Print the channel `soldr toolchain ensure --json` resolved (soldr#2879).

The target-run lane installs and verifies a pinned toolchain, then exports
`CARGO` and `RUSTC` as absolute paths into it. Those pin what the *job* runs
directly; they do not pin what the soldr children spawned by test fixtures
resolve. Those walk ancestors for a `rust-toolchain.toml`, find none under the
OS temp dir, and fall back to whatever rustup calls default — the runner
image's `stable`. The whole test tree sets `SOLDR_ALLOW_UNPINNED` (soldr#1766),
so nothing complains about it.

That is how three darwin tests came to strip with `stable`'s `rust-objcopy`,
whose `@rpath/libLLVM.dylib` is absent from the image, in a job that had just
installed and smoke-verified 1.95.0.

Exporting `RUSTUP_TOOLCHAIN` fixes it — `probe_direct_toolchain_binary` bails
out when it is set, so resolution goes through rustup on the provisioned
channel. This reads that channel out of the payload the lane already captured.

Usage:
    python .github/scripts/toolchain_ensure_channel.py <ensure.json>
"""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


def extract_payload(text: str) -> Any:
    """The JSON object inside `soldr toolchain ensure --json` output.

    `--json` does not mean "only JSON on stdout". The child `rustup` inherits
    soldr's stdout, so a real payload arrives preceded by rustup's own lines:

        (blank)
          1.95.0-x86_64-unknown-linux-gnu unchanged - rustc 1.95.0 (...)
        (blank)
        {
          "schema_version": 1,
          ...

    A plain `json.load` fails on that with `Extra data: line 2 column 7`,
    which is how the darwin lane rejected an otherwise correct payload.

    So: skip to the first `{` and decode one object from there. That is
    tolerant of a preamble without being tolerant of a *missing* payload —
    anything that is not a decodable object still raises, and the caller still
    refuses to guess a channel.

    The leak itself is a soldr bug, not something to normalise here; this
    function reads the stream that exists today.
    """
    start = text.find("{")
    if start < 0:
        raise json.JSONDecodeError("no JSON object in payload", text, 0)
    payload, _ = json.JSONDecoder().raw_decode(text[start:])
    return payload


def channel_from(payload: Any) -> str | None:
    """The resolved channel, or None if the payload does not name one.

    `channel` is `Option<String>` on the Rust side and the schema is stable at
    `schema_version: 1`, so a null is a legitimate answer meaning "no manifest
    channel" — distinct from a malformed payload, and both are the caller's
    problem rather than something to paper over with a default.
    """
    if not isinstance(payload, dict):
        return None
    channel = payload.get("channel")
    if not isinstance(channel, str):
        return None
    channel = channel.strip()
    return channel or None


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("payload", help="path to `toolchain ensure --json` output")
    args = parser.parse_args(argv)

    try:
        with open(args.payload, encoding="utf-8") as handle:
            payload = extract_payload(handle.read())
    except (OSError, json.JSONDecodeError) as error:
        print(f"could not read {args.payload}: {error}", file=sys.stderr)
        return 1

    channel = channel_from(payload)
    if channel is None:
        print(
            f"{args.payload} names no toolchain channel; refusing to guess one",
            file=sys.stderr,
        )
        return 1

    print(channel)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
