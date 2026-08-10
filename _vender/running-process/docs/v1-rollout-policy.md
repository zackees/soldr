# v1 Rollout Policy

Broker rollout is staged. The broker becomes default-on only after the
published gates are green.

## Stages

| Stage | Default | Purpose |
|---|---|---|
| Phase 0 | off | Schemas, framing, and tests. |
| Phase 1 | off | Pipe naming and shared lifecycle primitives. |
| Phase 2 | off | Manifest and cleanup tooling. |
| Phase 3 | off | Consumer wire migrations. |
| Phase 4 | opt-in | Broker binary and `Hello` negotiation. |
| Phase 5 | opt-in | Backend lifecycle management. |
| Phase 6 | opt-in | Handoff optimization. |
| Phase 7 | default-on canary | Controlled default-on rollout. |
| Phase 8 | default-on | Escape hatch removal window begins after sustained stability. |

## Gates

Default-on requires:

- protocol compatibility tests passing
- per-platform pipe-name tests passing
- peer credential tests passing
- no-network integration test passing
- manifest round-trip tests passing
- lifecycle event size tests passing
- `Hello` latency P50 at or below 200 microseconds
- `Hello` latency P99 at or below 1 millisecond
- two weeks of green perf guard on default-on canary
- documented rollback procedure

## Canary Discipline

Canary rollout tracks:

- handshake success rate
- refusal code distribution
- backend spawn attempts
- spawn budget exhaustion
- direct fallback usage
- escape hatch usage
- p50 and p99 handshake latency

Any regression in correctness gates stops promotion.

## Rollback

Rollback is setting:

```text
RUNNING_PROCESS_DISABLE=1
```

Consumers then use their direct daemon path. The direct path stays tested during
broker rollout.

## Phase 7 / Phase 8 disposition

Phase 7 (default-on canary) and Phase 8 (escape-hatch removal) are **explicitly
re-deferred**. They do not advance under the current regime, for three reasons:

- **No default-on flag or canary fleet exists.** Every first-party consumer
  (soldr, zccache, fbuild, clud) adopted the broker on an *opt-in* basis with
  direct daemon fallback retained. There is no default-on code path to canary,
  and no operator-owned canary fleet to collect the two-week perf/operator
  evidence the gates require.
- **No calendar-gate infrastructure exists.** The "two weeks of green perf guard
  on default-on canary" gate presumes a dated stage plan with operator signoff.
  That machinery is not built, so dated stage gates cannot be observed.
- **`RUNNING_PROCESS_DISABLE=1` is load-bearing.** The escape hatch is the
  correctness fallback for all consumers today, not a deprecated convenience.
  Removing it (Phase 8) is unsafe until Phase 7 has shipped and stabilized, which
  has not happened.

Re-scoping Phase 7 to default-on with real calendar gates, and the subsequent
Phase 8 removal, will be tracked as fresh work when an operator-owned canary
fleet and a default-on toggle exist. Until then the staged table above stands and
the escape hatch is retained.

## Documentation Rule

Each phase updates the relevant v1 docs in the same PR as code changes. A phase
does not ship with stale broker documentation.
