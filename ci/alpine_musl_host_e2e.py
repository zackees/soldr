#!/usr/bin/env python3
"""Prove that Soldr can bootstrap and build on an Alpine musl host."""

from __future__ import annotations

import argparse
import os
import subprocess


def container_command(image: str, github_token: str | None) -> list[str]:
    """Build the Docker command without invoking it, for unit-test coverage."""
    script = r"""
set -eu
if command -v zig >/dev/null; then
  echo "unexpected zig on Alpine host" >&2
  exit 1
fi
for tool in cc gcc rustc cargo; do
  if command -v "$tool" >/dev/null; then
    echo "unexpected preinstalled $tool on Alpine host" >&2
    exit 1
  fi
done
apk add --no-cache gcc musl-dev
mkdir -p /tmp/alpine-soldr-host/src
cd /tmp/alpine-soldr-host
printf '%s\n' '[package]' 'name = "alpine-soldr-host"' 'version = "0.1.0"' 'edition = "2021"' > Cargo.toml
printf '%s\n' '[toolchain]' 'channel = "1.95.0"' > rust-toolchain.toml
printf '%s\n' 'fn main() { println!("alpine musl host"); }' > src/main.rs
soldr toolchain ensure --json
soldr cargo build --target x86_64-unknown-linux-musl
test -x target/x86_64-unknown-linux-musl/debug/alpine-soldr-host
if find "${SOLDR_HOME}" -iname '*zig*' -print -quit | grep -q .; then
  echo "unexpected zig in Soldr state" >&2
  exit 1
fi
"""
    command = ["docker", "run", "--rm"]
    if github_token:
        command.extend(["--env", f"SOLDR_GITHUB_TOKEN={github_token}"])
    command.extend([image, "sh", "-ec", script])
    return command


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--timeout-seconds", type=int, default=600)
    args = parser.parse_args()
    command = container_command(args.image, os.environ.get("SOLDR_GITHUB_TOKEN"))
    print(f"$ {' '.join(command[:-1])} <script>", flush=True)
    try:
        return subprocess.run(
            command, check=False, timeout=args.timeout_seconds
        ).returncode
    except subprocess.TimeoutExpired:
        print(
            f"Alpine musl host acceptance exceeded {args.timeout_seconds}s", flush=True
        )
        return 124


if __name__ == "__main__":
    raise SystemExit(main())
