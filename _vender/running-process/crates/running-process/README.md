# running-process crate

This crate contains the Rust implementation for `running-process`, including
the v1 broker protocol schemas and lifecycle helpers.

## v1 Broker Specification

Core broker docs:

- [Architecture overview](../../docs/v1-architecture-overview.md)
- [Frozen commitments](../../docs/v1-frozen-commitments.md)
- [Pipe naming](../../docs/v1-pipe-naming.md)
- [Platform behavior](../../docs/v1-platform-behavior.md)
- [Security model](../../docs/v1-security-model.md)

Schema docs:

- [Wire envelope](../../docs/v1-wire-envelope.md)
- [Cache manifest](../../docs/v1-cache-manifest.md)
- [Service definition](../../docs/v1-service-definition.md)
- [Lifecycle events](../../docs/v1-lifecycle-events.md)

Consumer adoption guides:

- [Dashboard](../../docs/v1-consumer-adoption-dashboard.md)
- [clud](../../docs/consumer-adoption-clud.md)
- [zccache](../../docs/consumer-adoption-zccache.md)
- [soldr](../../docs/consumer-adoption-soldr.md)
- [fbuild](../../docs/consumer-adoption-fbuild.md)

Operations and rollout docs:

- [Broker internal architecture](../../docs/v1-broker-architecture.md)
- [Admin verbs](../../docs/v1-admin-verbs.md)
- [Backend lifecycle](../../docs/v1-backend-lifecycle.md)
- [Handoff optimization](../../docs/v1-handoff-optimization.md)
- [Observability](../../docs/v1-observability.md)
- [Rollout policy](../../docs/v1-rollout-policy.md)
- [Escape hatch](../../docs/v1-escape-hatch.md)
- [Troubleshooting](../../docs/v1-troubleshooting.md)

Examples:

- [Minimal consumer](../../examples/minimal-consumer/)
- [Release-handles CLI](../../examples/release-handles-cli/)
- [Custom isolation](../../examples/custom-isolation/)

Contrib service templates:

- [systemd user service](../../contrib/systemd/running-process-broker-v1.service)
- [macOS LaunchAgent](../../contrib/launchd/com.zackees.running-process-broker-v1.plist)
- [Windows service installer](../../contrib/windows-service/install.ps1)

The authoritative v1 proto files live under `proto/broker_v1/`.
