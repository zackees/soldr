# Probe documentation

The probe: live stacks, crash history, and CPU profiles for enrolled
processes, without a debugger attached or elevated privileges.

**Start here:** [quickstart](quickstart.md) — runnable start to finish.

## Guides

| Document | Covers |
|---|---|
| [overview](overview.md) | The pieces, how a capture works, why the daemon is standalone, why HTTP exists alongside the socket |
| [quickstart](quickstart.md) | Build → start the daemon → enrol → `ps` → dump → profile → crashes → `doctor` |
| [security-and-privacy](security-and-privacy.md) | Enrolment vs discovery, env default-deny, peer credentials vs bearer token, what crash queries redact |
| [profiling](profiling.md) | The sampling pipeline, the 60s/1kHz bounds, metrics, pprof/Firefox/collapsed exports, flame graphs |
| [crash-dumps](crash-dumps.md) | Spool → durable record, filtering, why stats are separate, artifact fetch, retention |
| [symbol-discovery](symbol-discovery.md) | Manifest/path/cache/server lookup with exact build-id, UUID, and PDB GUID+age gates |

## Command reference

```
rpprobed [--runtime-dir DIR] [--beacon-port PORT]

rpprobe ps        [--name GLOB] [--include-unregistered] [--env] [--limit N]
rpprobe dump      [PID] [--name GLOB] [--instance NAME] [--all] [--max-depth N]
rpprobe snapshot  PID [--max-depth N]
rpprobe crashes   [--class C] [--class-like PAT] [--signature S] [--stats] [--limit N]
rpprobe profile   [--seconds N] [--hz N] [--format pprof|json|collapsed] [--out PATH]
rpprobe fetch     ID [--out PATH]
rpprobe doctor

Global: --discovery PATH  --json  --http
```

Exit code is `0` on success and non-zero otherwise, so `doctor` and scripts
can branch on the result rather than parsing output.

## HTTP surface

All routes require the bearer token — including `/`. See
[security-and-privacy](security-and-privacy.md#loopback-is-not-a-user-boundary).

```
GET  /                                  browser UI
GET  /v1/ps                             live process query
GET  /v1/crashes                        crash history
GET  /v1/crashes/stats                  rollup by signature
POST /v1/snapshot                       request a capture
POST /v1/profile                        capture a CPU profile
GET  /v1/profiles                       retained profile ids
GET  /v1/profiles/{id}/flamegraph       self-contained flame graph
GET  /v1/profiles/{id}/export/{format}  pprof | json | collapsed
GET  /v1/artifacts/{id}                 streaming artifact download
```

## Environment controls

| Variable | Effect |
|---|---|
| `RUNNING_PROCESS_PROBE_DISCOVERY` | Directory holding the discovery file. Lets a test target its own daemon. |
| `RUNNING_PROCESS_PROBE_BEACON_PORT` | Fixed election port. `0` lets the OS pick, which is what an isolated instance wants. |
| `RUNNING_PROCESS_PROBE_BIND_ALL` | `1` permits a non-loopback HTTP bind. Publishes everything behind one token — read the security guide first. |

## Design notes worth knowing before changing things

- **Enrolment, not discovery.** A process is findable because it opted in.
- **One core, three front doors.** CLI, HTTP, and socket all call the same
  `ProbeOps` functions, so policy cannot drift between them.
- **Resume before you unwind.** The suspension window covers copying only;
  unwinding and symbolization happen with the target running again.
- **Bounds everywhere.** Every surface that could return an unbounded amount
  of anything refuses to. A diagnostic tool runs on machines already having a
  bad day; the failure mode where the tool becomes the outage is designed out.
- **Symbol parsers stay out of the daemon.** A malformed symbol file can crash
  a parser outright, so symbolization is a short-lived child process.
  Isolation is a process boundary, not a `catch_unwind`.
