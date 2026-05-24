# soldr Python package

Tooling that lives next to the Rust crates but is invoked from Python
(release wheels, CI scripts, the public action exporter).

## Modules

- `setup_soldr_exporter.py` — generates the standalone `zackees/setup-soldr`
  GitHub Action bundle from this repository's `action.yml`, helper scripts,
  and `LICENSE`. The exporter renders a fixed public README and strips the
  `repo:` input that only the source repo uses for local testing.
