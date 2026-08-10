# Probe CPU profiling

```bash
rpprobe profile --seconds 5 --hz 99 --format collapsed
```

## The pipeline

```
  sampler ──push──▶ [bounded ring] ──drain──▶ symbolizer ──▶ exporters
 (hot path)          drop + count            (off hot path)
```

The split at the ring is the whole design. Sampling suspends threads in the
target, so every microsecond on the sampling side is a microsecond the
profiled program is not running — and symbolization is the expensive part,
because it parses PDB/DWARF/Mach-O and touches the filesystem. Doing it
between samples would make the profiler's own cost dominate, and the flame
graph would be a picture of the profiler.

So the sampler only ever pushes raw instruction pointers. Names are attached
afterwards from `(module, relative address)`, which also means **a profile
symbolizes after the sampled threads have exited**.

## Bounds

| Bound | Value | Why |
|---|---|---|
| Duration | **60s max** | A cap, not a raisable default. An unbounded session is one an operator can start, forget, and leave degrading a production process. |
| Frequency | 1–1000 Hz | Above ~1 kHz the suspend/resume cost stops being negligible against the interval, and the profile measures the profiler. |
| Ring | 65536 samples | Fixed. Overflow drops and counts. |

Requests are **clamped, not refused** — someone who typed `--seconds 300`
wants a profile, and 60 seconds of one beats an error — and the clamped values
are reported back rather than quietly substituted.

The default is **99 Hz, not 100**, so the sampler drifts across anything else
on a 100 Hz timer (schedulers, animation loops, poll intervals) instead of
phase-locking with it and reporting a periodic artifact as a hot path.

## Backpressure is a dropped sample

Never a blocked sampler, never a grown buffer. Blocking would push the
profiler's cost onto the thing it is measuring; growing would let a slow
consumer turn a profile into an OOM. A dropped sample is a small, *measured*
loss of fidelity, reported so you know the profile is thinned rather than
discovering it in a misleading graph.

## What the metrics mean

```json
{"samples_captured":81,"samples_dropped":0,"threads_seen":9,
 "thread_coverage":1.0,"overhead_ratio":0.0030,"hz":200,"clamped":false}
```

- **`thread_coverage`** — fraction of live threads the profile saw. A flame
  graph covering two of eight threads looks exactly like one covering all of a
  two-threaded program, so this is reported rather than assumed.
- **`overhead_ratio`** — pause time the target actually paid, over session wall
  time. Measured, not asserted: a profiler that hides its own cost lets you
  misread an overhead-shaped profile as a program-shaped one.
- **`clamped`** — whether your request was reduced to fit the bounds.

## Exports

| Format | Use |
|---|---|
| `pprof` | `go tool pprof` and most viewers. Gzipped, per the `.pb.gz` convention. |
| `json` | Firefox Profiler processed-profile format. Drag onto profiler.firefox.com. |
| `collapsed` | Brendan Gregg folded stacks. Readable with `sort` and `grep`; also the flame-graph feed. |

All three are folded from **one** function, so they cannot disagree about what
was hot.

The pprof schema is vendored as a `.proto` and encoded directly rather than
via the `pprof` crate, which carries an open RUSTSEC unsoundness advisory. All
that was wanted from it was a wire format.

```
GET /v1/profiles/{id}/export/pprof
GET /v1/profiles/{id}/export/json
GET /v1/profiles/{id}/export/collapsed
GET /v1/profiles/{id}/flamegraph
```

## Profiles are ephemeral

Retained in memory for 15 minutes, at most 8 at a time, oldest evicted first.

A crash record is evidence about something that already happened and is worth
a month. A profile is a working artifact of an investigation happening right
now, and it is large. Keeping them durably would quietly turn a diagnostic
tool into a disk-consumption problem on the machine it is meant to be helping.
Save the one you care about — the CLI writes it for you.

Eviction drops the *oldest*, not the newest: someone who just ran a profile
wants that one, and refusing it to preserve one they have finished with would
be backwards.

## Flame graph

`GET /v1/profiles/{id}/flamegraph` returns one self-contained document —
renderer and data both inlined, nothing fetched — under
`Content-Security-Policy: default-src 'none'`. Siblings are ordered by weight
so the hottest path is the widest leftmost run; a flame graph is read by eye,
and the eye goes left.
