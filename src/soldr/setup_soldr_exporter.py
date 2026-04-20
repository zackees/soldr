#!/usr/bin/env python3
from __future__ import annotations

import argparse
import shutil
from pathlib import Path


HELPER_SCRIPT_PATHS = (
    Path('.github/actions/setup-soldr/resolve_setup.py'),
    Path('.github/actions/setup-soldr/ensure_rust_toolchain.py'),
    Path('.github/actions/setup-soldr/ensure_soldr.py'),
    Path('.github/actions/setup-soldr/verify_soldr.py'),
)

PUBLIC_ACTION_REPO = 'zackees/setup-soldr'
PUBLIC_README = """# setup-soldr

Public GitHub Action for installing one released `soldr` binary, provisioning the resolved Rust toolchain with `rustup`, and restoring a cacheable runner-local root for Soldr, Cargo, and rustup state.

This repository is intended to be generated from `zackees/soldr`. The source-of-truth contract and release process still live in `soldr` issue #137 and `docs/SETUP_SOLDR_PUBLIC_ACTION.md`.

## Usage

### Linux

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-linux:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v1
        with:
          version: 0.7.4
          cache: true
      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

### macOS

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-macos:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v1
        with:
          version: 0.7.4
          cache: true
      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

### Windows

```yaml
name: ci

on:
  push:
  pull_request:

jobs:
  build-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: zackees/setup-soldr@v1
        with:
          version: 0.7.4
          cache: true
      - run: soldr cargo build --locked --release
      - run: soldr cargo test --locked
```

## Inputs

| Input | Meaning |
|---|---|
| `version` | Soldr release tag or version to install. Empty means latest release. |
| `cache` | Restore and save the action-managed cache/state root. |
| `cache-dir` | Override the runner-local cache/state root. |
| `cache-key-suffix` | Optional escape hatch appended to the cache key. |
| `toolchain` | Explicit Rust toolchain channel override. |
| `toolchain-file` | Alternate toolchain file path when `toolchain` is empty. |
| `trust-mode` | Optional `SOLDR_TRUST_MODE` value. |

## Outputs

| Output | Meaning |
|---|---|
| `soldr-path` | Installed Soldr binary path added to `PATH`. |
| `soldr-version` | Installed Soldr version reported by `soldr version --json`. |
| `cache-dir` | Action-managed runner-local cache/state root. |
| `cache-hit` | Whether the action restored an exact cache hit. |
| `toolchain` | Exact Rust toolchain channel configured for the action. |

## Notes

- The action installs exactly one released `soldr` binary for the active runner target.
- The normal path provisions Rust with `rustup`; on self-hosted runners, `rustup` must already be available.
- The action rehydrates `SOLDR_CACHE_DIR`, `CARGO_HOME`, and `RUSTUP_HOME` under the selected cache root.
- Managed `zccache` artifact storage still follows zccache's current supported/default behavior rather than a fully action-controlled custom artifact path.

## Development

Regenerate this repository bundle from the source repository with the exporter in `zackees/soldr`.
"""


def _module_repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding='utf-8')


def _copy_file(source_root: Path, destination_root: Path, relative_path: Path) -> None:
    source_path = source_root / relative_path
    if not source_path.is_file():
        raise FileNotFoundError(f'Missing source file for export: {source_path}')
    destination_path = destination_root / relative_path
    destination_path.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source_path, destination_path)


def _strip_repo_input(action_text: str) -> str:
    output_lines: list[str] = []
    skipping_repo_input = False

    for line in action_text.splitlines():
        if skipping_repo_input:
            if line.startswith('  ') and not line.startswith('    '):
                skipping_repo_input = False
            else:
                continue

        if line == '  repo:':
            skipping_repo_input = True
            continue

        if 'INPUT_REPO:' in line:
            continue

        output_lines.append(line)

    return '\n'.join(output_lines) + '\n'


def render_public_action_yaml(source_root: Path) -> str:
    return _strip_repo_input((source_root / 'action.yml').read_text(encoding='utf-8'))


def render_public_readme() -> str:
    return PUBLIC_README


def export_setup_soldr_bundle(source_root: Path, destination_root: Path) -> Path:
    source_root = source_root.resolve()
    destination_root = destination_root.expanduser().resolve()

    if destination_root == source_root:
        raise ValueError('Destination must not be the source repository root')

    destination_root.mkdir(parents=True, exist_ok=True)
    _write_text(destination_root / 'action.yml', render_public_action_yaml(source_root))
    _write_text(destination_root / 'README.md', render_public_readme())
    _copy_file(source_root, destination_root, Path('LICENSE'))

    for relative_path in HELPER_SCRIPT_PATHS:
        _copy_file(source_root, destination_root, relative_path)

    return destination_root


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Export the standalone public setup-soldr action bundle.'
    )
    parser.add_argument('destination', help='Directory to materialize as the future public action repository.')
    parser.add_argument(
        '--source-root',
        default=str(_module_repo_root()),
        help='Source soldr repository root. Defaults to the current checkout.',
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    destination = export_setup_soldr_bundle(Path(args.source_root), Path(args.destination))
    print(f'Exported setup-soldr bundle to {destination}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
