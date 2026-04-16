# Machine-Facing API Boundary

This document defines the supported external integration boundary for `soldr`.

## Decision Summary

Current repo policy is:

- `soldr` is a binary-first product
- the supported external surface is the `soldr` executable, not the internal Rust workspace crates
- there is no supported public Rust crate API, C ABI, or FFI surface
- there is no supported long-running daemon or local service protocol today
- the first supported machine-facing API is explicit CLI commands with structured JSON output on selected commands

## What Is Supported Today

Supported use today means invoking the `soldr` binary as a subprocess through documented commands and flags such as:

- `soldr cargo ...`
- `soldr <tool>[@version] ...`
- `soldr status`
- `soldr status --json`
- `soldr cache`
- `soldr cache --json`
- `soldr clean`
- `soldr version`
- `soldr version --json`

The current CLI reference lives in [API.md](./API.md).

## What Is Not A Supported Public API

The following are implementation details and may change without a semver promise to automation consumers:

- the internal Rust crates in `crates/`
- direct use of `soldr-core`, `soldr-fetch`, `soldr-cache`, or `soldr-cli` as library dependencies
- undocumented environment variables or wrapper-mode conventions
- the exact on-disk cache/session layout beyond what is explicitly documented for users
- human-oriented stdout/stderr wording unless a command is explicitly documented as structured and stable
- commands that do not explicitly document `--json` as a supported protocol surface

## Future Direction

The intended progression is:

1. Keep `soldr` binary-first.
2. Expand explicit JSON output only for selected commands that need automation support.
3. Version that structured output as an external protocol.
4. Only consider a daemon or other long-running local protocol if a real use case appears that the CLI cannot serve cleanly.

If a daemon or other protocol is ever added, it should be documented and versioned as a separate external interface. It should not expose the internal Rust crate graph and call that the supported API.

## Non-Goals

This repo is not currently pursuing:

- a stable public Rust library API
- a stable embeddable C ABI
- undocumented machine parsing of human help/status text
- a background service that third parties are expected to integrate with by default
