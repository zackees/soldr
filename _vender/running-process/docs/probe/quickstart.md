# Probe quickstart

Every command below is real and was run against this repo. Copy them in order.

## 1. Build the probe binaries

```bash
soldr cargo build -p running-process-probe-daemon --bins
```

That produces two binaries under `target/<triple>/debug/`:

- `rpprobed` — the daemon. One per user.
- `rpprobe` — the CLI you talk to it with.

## 2. Start the daemon

```bash
rpprobed
```

It prints two lines:

```
role=daemon pid=22924 beacon=52341
http=http://127.0.0.1:59519/?token=210d6c34…
```

The second is a Jupyter-style URL. **The token is the credential** — open that
URL and you get the browser UI. See [security-and-privacy](security-and-privacy.md)
for why loopback alone is not enough.

`role=client` instead of `role=daemon` means another daemon already owns this
user's endpoint, which is the intended single-instance behaviour.

### Running an isolated instance

For a test or a second checkout, give it its own runtime directory and let the
OS pick the election port:

```bash
rpprobed --runtime-dir /tmp/my-probe --beacon-port 0
```

Without `--beacon-port 0` a second instance finds the first one on the shared
per-user port and resolves to `client`, which is correct for the real daemon
and useless for an isolated one.

## 3. Enrol a process

The probe model is **enrolment, not discovery**: a process is findable because
it opted in, not because it is running.

Rust:

```rust
// Cargo.toml: running-process = { version = "…", features = ["probe"] }
let _guard = running_process::probe::install(running_process::probe::Config::default());
```

Python:

```python
from running_process import probe
guard = probe.install()
```

Both return a guard that deregisters on drop. Enrolment never blocks: if no
daemon is running, `install()` still returns immediately and the process runs
unprobed.

## 4. Look at what is enrolled

```bash
rpprobe ps
```

```
PID    NAME       CLASS  REG  CWD          ENV
22924  clud.exe   clud   yes  /work/clud
```

With no daemon running you get an actionable failure rather than a hang:

```
rpprobe: no probe daemon found (looked for …/rpprobed.json). Start one with
`rpprobed`, or point RUNNING_PROCESS_PROBE_DISCOVERY at its runtime directory.
```

## 5. Capture a stack

```bash
rpprobe dump 22924
rpprobe dump --name '*worker*' --all
```

`--name` without `--all` **refuses an ambiguous match** and lists the
candidates. Capturing "the first match" would be a coin flip, and a stack from
the wrong worker looks exactly like a stack from the right one.

An unenrolled pid is refused by name, not by error code:

```
rpprobe: probe daemon refused the request: pid 99999 is not registered with this daemon
```

## 6. Profile

```bash
rpprobe profile --seconds 5 --format collapsed
```

```
captured 81 sample(s) across 9 thread(s), 0.3% overhead
wrote 4213 bytes to profile-1.collapsed
flame graph: http://127.0.0.1:59519/v1/profiles/1/flamegraph
```

Open that URL for an interactive flame graph. `--format pprof` and
`--format json` (Firefox Profiler) are also available. See
[profiling](profiling.md) for the bounds and what the overhead number means.

## 7. Browse crashes

```bash
rpprobe crashes --class-like 'clud%'
rpprobe crashes --stats
```

Use `--class-like` rather than `--class` when you mean a family: "all clud
crashes" includes `clud-worker`, and an exact match silently drops half the
incident.

`--stats` rolls up by signature. It is a separate flag rather than a column
because `--limit` truncates: counting rows out of a limited page reports "10
crashes" for any bucket bigger than the page, confidently.

## 8. Check the plumbing

```bash
rpprobe doctor
```

```
      CHECK           DETAIL
ok    discovery file  …/rpprobed.json (daemon pid 26516)
ok    control socket  \\.\pipe\rpp-probe-…
FAIL  registrations   no processes are registered: nothing can be captured.
                      An app must call probe::install() and reach ARMED.
ok    http surface    127.0.0.1:59660
ok    symbolizer      …/running-process-probe-worker.exe
```

It reports every check rather than stopping at the first fault, and exits
non-zero when any fails — so a script can branch on the verdict without
parsing the table.

## Where to go next

- [overview](overview.md) — what the pieces are and how they fit
- [security-and-privacy](security-and-privacy.md) — what is disclosed, to whom
- [profiling](profiling.md) — CPU profiling, bounds, exports
- [crash-dumps](crash-dumps.md) — durable crash history and retention
- [symbol-discovery](symbol-discovery.md) — how symbols are found and matched
