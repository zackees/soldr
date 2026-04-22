# setup-soldr Exporter

This repository cannot publish its root `action.yml` directly to GitHub Marketplace, but it contains an exporter that materializes the releaseable `setup-soldr` action bundle for the public `zackees/setup-soldr` repository.

## Command

From the `soldr` repository root:

```powershell
python tools/export_setup_soldr_action.py ..\setup-soldr
```

You can also target another checkout explicitly:

```powershell
python tools/export_setup_soldr_action.py C:\tmp\setup-soldr --source-root C:\src\soldr
```

## Exported Files

The exporter writes the standalone repository shape defined in `docs/SETUP_SOLDR_PUBLIC_ACTION.md`:

- `action.yml`
- `README.md`
- `LICENSE`
- `.github/actions/setup-soldr/resolve_setup.py`
- `.github/actions/setup-soldr/ensure_rust_toolchain.py`
- `.github/actions/setup-soldr/ensure_soldr.py`
- `.github/actions/setup-soldr/verify_soldr.py`

## Behavior

- rewrites the root `action.yml` into the public action form by removing the internal-only `repo` input and its `INPUT_REPO` wiring
- keeps the helper-script layout unchanged so extraction stays mechanical
- generates a public-facing `README.md` with Linux, macOS, and Windows workflow examples plus the supported action inputs and outputs
- refuses to export into the `soldr` source repository root

## Intended Release Flow

1. Validate action changes in `zackees/soldr`.
2. Run the exporter into a clean `zackees/setup-soldr` checkout.
3. Review the generated bundle diff there.
4. Tag and release the public action from that dedicated repository.

The contract and release-versioning rules still live in [docs/SETUP_SOLDR_PUBLIC_ACTION.md](./SETUP_SOLDR_PUBLIC_ACTION.md).
