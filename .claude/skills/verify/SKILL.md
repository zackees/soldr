---
name: verify
description: Verify Soldr CLI target lifecycle changes through the public CLI.
---

# Verify Soldr CLI changes

1. Build the CLI via `soldr --no-cache cargo build -p soldr-cli`.
2. For catalogue-backed toolchains, make an isolated `SOLDR_CACHE_DIR` and seed `<root>/bin/syslib/<tool>/<version>/<slug>/package/` with a `.complete` stamp, target-prefixed required tools, and expected sysroot directories. This avoids external fetches while driving the real `soldr prepare` CLI.
3. Run `target/<host>/debug/soldr.exe env --target <triple> --json` to observe the capability plan, then `soldr.exe prepare --target <triple> --github-env <file>` to observe the emitted environment.
4. Probe unsupported target floor suffixes through the CLI and capture the nonzero error.
