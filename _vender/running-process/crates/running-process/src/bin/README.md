# `src/bin` — running-process binaries

Five binaries ship from this crate. Each has a different feature gate and
runtime role.

## Binaries

| binary | what it does | feature gate |
|---|---|---|
| `running-process-daemon` | The per-user daemon that owns process state, accepts CLI requests, manages backend lifecycle | `daemon` (requires `client` + `client-async`) |
| `runpm` | The user-facing CLI — list / kill / inspect tracked processes | `client` |
| `running-process-broker-v1` | v1 broker — FROZEN per #228; admin RPCs, serve modes, doctor checks (514 LOC) | `client` |
| `running-process-broker-v2` | v2 broker — accept loop + ServiceDefinitionLoader integration (running-process#532 slice 1) | `client` |
| `running-process-cleanup` | Standalone manifest-registry GC tool | `client` |
| `daemon-trampoline` | Detach-on-spawn helper (no required features) | — |

## v1 ↔ v2 broker coexistence

`running-process-broker-v1` is FROZEN FOREVER per #228 — kept verbatim
for ecosystem stability. `running-process-broker-v2` is the forward
path: same wire envelope (`broker::protocol` types reused) but a
distinct pipe namespace (`rpb-v2-<program>-<sid>-<idx>` via
`names_v2`) so the two brokers can run side-by-side on the same host.

The v2 broker reads `.servicedef.v2` files via
`broker::protocol_v2::ServiceDefinitionLoader` (slice 23-C of
zccache#782) — the read-side complement to consumers'
`ServiceDefinitionBuilder::install()` writes (slice 22b). Both file
extensions cohabit the same per-OS service-definition directory; each
broker picks the matching file via extension.

## Conventions

- Every binary's `main()` is `fn main() -> ExitCode` (no panics on
  startup; print a diagnostic to stderr + return non-zero).
- CLI parsing is intentionally hand-rolled (no `clap` here) so the
  startup path stays allocation-light + fast.
- `--no-bind` / `--once` flags exist for integration tests on
  binaries that bind sockets.
