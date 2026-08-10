# Probe overview

The probe answers three questions about a running program: *what is it doing
right now*, *what was it doing when it crashed*, and *where is its time
going*. It answers them without a debugger attached, without elevated
privileges, and without the program having to stop.

## The pieces

| Crate | What it is |
|---|---|
| `running-process-probe` | The `probe_diag.v1` protocol types, and the cooperative all-thread snapshot (Windows, Linux, macOS; x86_64 + aarch64). |
| `running-process-probe-daemon` | `rpprobed` (the daemon) and `rpprobe` (the CLI). Owns the registry, the crash store, the HTTP surface. |
| `running-process-probe-worker` | The off-process symbolizer. A stdin→stdout filter taking one capture per invocation. |
| `running-process` (`probe` feature) | The client facade an application links: `probe::install()`. |

## How a capture works

The hard constraint is that suspending a thread is a cost the *target* pays,
so the suspension window has to be as short as physically possible.

1. Suspend or signal sibling threads.
2. Copy registers plus a bounded readable slice of each stack.
3. **Resume every thread.**
4. Unwind the copied stacks afterwards, with the target running again.
5. Symbolize later still, in a separate process.

Steps 4 and 5 are the interesting part: neither needs the target to be
stopped, and neither needs it to still exist. A capture symbolizes fine after
the process has exited, which is exactly when you most want it.

Linux's handler touches only atomics, and macOS copies through Mach VM reads,
so an invalidated mapping degrades to a dropped sample rather than a fault in
the host.

## Enrolment

A process becomes probeable by calling `probe::install()`. That is a
deliberate choice over scanning the process table:

- The registrant declares what it permits (`AllowPolicy`) and what it
  discloses (`Disclosure`), so the daemon is never guessing.
- Identity is verified — executable hash, boot id, liveness — before a
  registration reaches ARMED.
- A process that never opted in is not silently inspectable.

`install()` never blocks. It prepares the crash spool synchronously, then
enrols on a background thread, so an absent daemon costs nothing at startup.

## Why the daemon is standalone

`rpprobed` is *not* a broker backend. It borrows the broker's plumbing —
framing codec, peer-credential ACL, privilege refusal, private-directory
hardening — because re-deriving those would mean re-deriving their bug fixes
too. But it has its own wire and its own lifecycle, so the probe schema is not
coupled to a frozen protocol it does not otherwise need.

## One core, three front doors

The CLI, the HTTP API, and the control socket all call the same `ProbeOps`
functions. That is structural, not stylistic: if each ingress owned its own
policy logic they would drift, and the weaker one would quietly become the way
in. An environment value the socket will not disclose is one the CLI cannot
print, because the CLI was never the thing deciding.

## Why HTTP exists alongside the socket

Two things the framed socket cannot do:

- **Serve a browser.** Crash triage is a looking-at-things activity, and a
  flame graph is not something you read over a length-prefixed protobuf.
- **Move a large artifact.** The socket buffers a whole frame before parsing
  it and caps that at 16 MiB. A minidump is routinely larger.
  `GET /v1/artifacts/{id}` streams instead, so a 2 GiB dump and a 2 MiB dump
  cost the daemon the same resident bytes.

## Bounds, everywhere

Every surface that could return an unbounded amount of anything refuses to:

- Process and crash queries require an explicit, capped `limit`.
- Profiling sessions are capped at 60s and 1 kHz — a cap, not a default a
  caller can raise.
- The profile sample ring is fixed-size and drops-and-counts rather than
  blocking the sampler or growing.
- Crash history has age, per-app, byte, and row retention bounds.

The recurring reasoning: a diagnostic tool is deployed on machines that are
already having a bad day, and the failure mode where the tool becomes the
outage is the one worth designing out.

## Further reading

- [quickstart](quickstart.md) — runnable, start to finish
- [security-and-privacy](security-and-privacy.md) — what is disclosed, to whom
- [profiling](profiling.md) — CPU profiling and its exports
- [crash-dumps](crash-dumps.md) — durable history and retention
- [symbol-discovery](symbol-discovery.md) — exact-identity symbol matching
