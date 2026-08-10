# v1 Broker Architecture Overview

This document defines the v1 broker architecture for `running-process`.
The broker is a control-plane component. It is not a data-plane router.

## Goals

- Keep daemon discovery and lifecycle coordination in one stable place.
- Keep steady-state client traffic on direct backend pipes.
- Support private broker instances for untrusted or third-party services.
- Support a shared broker instance for first-party services that share
  operational controls.
- Preserve a direct daemon path so the broker is an optimization, not a hard
  dependency.

## Two-Plane Design

The broker owns the control plane:

- protocol negotiation through `Hello` and `HelloReply`
- service-definition loading
- backend spawn coordination
- backend lifecycle supervision
- admin verbs and observability
- broadcast control operations such as maintenance handle release

Backends own the data plane:

- request and response traffic between client and backend
- service-specific RPC protocol
- service-specific cache behavior
- service-specific state machines

The broker returns a backend pipe address after negotiation. The client then
disconnects from the broker and connects to the backend directly.

```text
client process
  |
  | Hello { service_name, wanted_version, capabilities }
  v
running-process-broker-v1
  |
  | HelloReply::Negotiated { backend_pipe }
  v
client process ------ direct service RPC ------ service backend
```

## First Contact

1. The client derives the broker pipe for its selected isolation mode.
2. The client sends one v1 framed `Hello`.
3. The broker validates the peer identity, service name, requested version,
   and service definition.
4. The broker starts the backend when no matching backend is live.
5. The broker returns `Negotiated` with the backend pipe or `Refused` with a
   stable error code.
6. The client closes the broker connection and uses the backend pipe directly.

## Hello-Skip Fast Path

A client that already has a fresh backend pipe for its own version connects to
that backend directly. A failed direct connection falls back to broker
negotiation.

This keeps warm-cache workloads out of the broker path. The broker handles
coordination, while the backend handles high-volume service traffic.

The crate exposes this policy through `broker::client::connect_to_backend`:
callers pass a cached backend endpoint plus their `wanted_version` and
`self_version`; the helper uses the cached endpoint only when those versions
match.

## Trust Domains

Each service definition selects a broker isolation mode:

| Isolation | Use |
|---|---|
| `PRIVATE_BROKER` | Default. One broker instance per service. |
| `SHARED_BROKER` | First-party tools that share admin and lifecycle controls. |
| `EXPLICIT_INSTANCE` | Named trust grouping such as `ci-trusted` or `ci-untrusted`. |

The first-party default is `SHARED_BROKER` for `zccache`, `clud`, `fbuild`,
and `soldr-daemon`. Third-party consumers use `PRIVATE_BROKER` unless their
operator explicitly selects another trust domain.

## Broker Is Optional

The broker is optional by contract. Consumers retain a direct daemon mode and
an environment escape hatch. Broker adoption improves lifecycle coordination
and observability, but a broker defect does not remove the direct execution
path.

## Version Boundary

The v1 boundary is the binary name, proto package, framing byte, and pipe-name
format. A future breaking change ships as `running-process-broker-v2` with
separate v2 proto files. The v1 binary remains buildable and keeps serving v1
clients.
