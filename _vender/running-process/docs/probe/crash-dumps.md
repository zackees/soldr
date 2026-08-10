# Probe crash history

```bash
rpprobe crashes --class-like 'clud%'
rpprobe crashes --stats
```

## How a crash becomes a record

1. The crashing process performs **one fixed-size write** to a spool file and
   exits. Nothing more: it is running in compromised context, so it does not
   allocate, format, or parse.
2. `rpprobed` picks that spool up — later, in a healthy process — parses it,
   writes a durable JSON artifact, and commits redacted metadata to SQLite.

The daemon ingests the spool even if it starts *after* the crash. A process
can die between `install()` and its registration completing, and its evidence
still has to survive that.

## Querying

Filters combine with AND: exact class, `LIKE` on class, app name, instance,
signature, and a half-open time window.

Use **`--class-like`** when you mean a family. "All clud crashes" includes
`clud-worker`, and an exact match silently drops half the incident. If you are
building a pattern from a literal class name, escape it — `_` is a
single-character wildcard, so an unescaped `my_app` also matches `myXapp`.

An inverted window is **refused**, not answered with an empty set: empty is
indistinguishable from "nothing crashed", which is the most misleading answer
this surface can give.

## Stats are a separate call, on purpose

```bash
rpprobe crashes --stats --class-like 'clud%'
```

```
14 crash(es) across 2 class(es), 2026-07-28 04:11:02Z → 2026-07-30 19:52:44Z

COUNT  SIGNATURE        FIRST                 LAST                  CLASSES
9      SIGSEGV@parse    2026-07-28 04:11:02Z  2026-07-30 19:52:44Z  clud,clud-worker
5      SIGABRT@assert   2026-07-29 11:03:19Z  2026-07-30 08:22:07Z  clud-worker
```

Because **`--limit` truncates**. Counting rows out of a limited page reports
"10 crashes" for any bucket bigger than the page, and reports it confidently.
`total` is computed by the database over the whole match set.

A rollup takes no limit at all — its size is bounded by the number of distinct
signatures, not the number of crashes.

The `CLASSES` column is the cross-class fact: a signature spanning classes
usually means shared library code, which is the most useful thing a rollup can
tell you.

## What a query returns, and what it never does

Returned: class, name, version, instance, pid, signature, fault kind, crash
time, artifact size, and an **opaque artifact id**.

Never returned:

- The **inline crash report**. That is what the redaction rule exists for, and
  a test asserts the column stays out of the `SELECT` so adding it back breaks
  a test rather than shipping a secret.
- The **artifact path**. A daemon-private path discloses the owner's directory
  layout on a surface whose whole contract is redacted metadata.

## Fetching the bytes

```bash
rpprobe fetch <id> --out crash.json
```

Always over HTTP: an artifact is routinely larger than the control socket's
16 MiB frame cap, which is why the streaming endpoint exists. The download
streams in ~8 KiB chunks, so daemon memory is independent of artifact size.

The fetch is **pinned**: GC serializes with it and will not remove a row being
read. Without that, a long download and a retention sweep could overlap, and
the sweep would delete the file out from under the reader.

The endpoint also refuses to serve any file the daemon did not itself write —
the id resolves through the database, and no caller-supplied path component
ever reaches the filesystem.

## Retention

| Bound | Default |
|---|---|
| Age | 30 days |
| Per `(class, name)` | newest 100 |
| Total artifact bytes | 1 GiB |
| Total rows | 10,000 |
| Single artifact | 64 MiB (rejected above this) |

Retention runs after each durable insert, best-effort: a retention failure
must not fail an insert that already succeeded, or a surviving spool would
retry and duplicate a crash that is already committed.
