# v1 Escape Hatch

The v1 broker escape hatch is:

```text
RUNNING_PROCESS_DISABLE=1
```

Participating consumers read this variable before attempting broker discovery.
When it is set to `1`, the consumer uses its direct daemon path.
Rust consumers can use `running_process::broker::client::broker_disabled_by_env`
to parse the shared contract and then select their own direct fallback path.

## Values

| Value | Behavior |
|---|---|
| unset | Follow the rollout default for the consumer version. |
| `1` | Disable broker usage and use the direct daemon path. |

Unknown values are configuration errors and are logged with the consumer name.
Forced-broker canaries use consumer-specific rollout configuration, not the
emergency disable variable.

## Intended Use

Use the escape hatch for:

- production rollback
- isolating broker defects from backend defects
- bisecting performance regressions
- CI jobs that require direct daemon mode during rollout

## Operator Examples

Unix shell:

```sh
RUNNING_PROCESS_DISABLE=1 cargo test
```

PowerShell:

```powershell
$env:RUNNING_PROCESS_DISABLE = "1"
cargo test
```

GitHub Actions:

```yaml
env:
  RUNNING_PROCESS_DISABLE: "1"
```

## Related Test Seam: `RUNNING_PROCESS_FAKE_BACKEND`

`RUNNING_PROCESS_FAKE_BACKEND=<path>` is a TEST-ONLY seam recognized by
`running_process::broker::client::connect_to_backend`
(`RUNNING_PROCESS_FAKE_BACKEND_ENV`). When set to a non-empty endpoint, the
client connects directly to `<path>` over the same local-socket transport as
the Hello-skip cache path and skips broker discovery, Hello negotiation, and
version checks entirely. The connection reports
`BackendConnectionRoute::HelloSkip` with no negotiation metadata.

Rules:

- **Never set this in production.** It bypasses every broker safety check.
- `RUNNING_PROCESS_DISABLE=1` takes precedence: when the broker is disabled,
  the fake-backend seam is ignored too.
- If connecting to the fake endpoint fails, the error is returned as-is
  (`BrokerClientError::BackendConnect`). There is no fallback to the real
  broker path — tests that set the seam get determinism, not recovery.
- Unset or empty values disable the seam.

## Deprecation Timeline

| Stage | Escape hatch state |
|---|---|
| Phase 4 through Phase 6 | Required and tested. |
| Phase 7 | Required and monitored. |
| Phase 8 | Removal PR opens after default-on is stable for the published window. |
| v1.0 | Final state recorded in release notes. |

The direct daemon path remains available until the removal stage is complete.
