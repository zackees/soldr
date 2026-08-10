# Probe security and privacy

The probe can show you another process's stacks, working directory, and
environment. That is a lot of authority for a diagnostic tool, so the rules
below are enforced in code rather than documented as guidance.

## Enrolment, not discovery

A process is findable because it called `probe::install()`, not because it is
running. `rpprobe ps` with `--include-unregistered` will additionally list
what the OS shows any local process for free — but those rows carry no
declared metadata and, critically, no environment values.

Only a registration that reached **ARMED** — identity verified, connection
live — is ever reported as registered.

## Environment values are default-deny, and copied by the registrant

The daemon never reads a registered process's environment. The *registrant*
sends the values it chose to disclose, and the registry **refuses the whole
registration** if any of them is outside its own `AllowPolicy.env_allowlist`.

That refusal is the load-bearing check. The query surface reads straight out
of the disclosed set, so anything stored there is disclosable — the only safe
place to stop an unauthorised value is before it lands.

Consequences worth stating plainly:

- A value match against a **non-allowlisted** key is always false, and that
  key never appears in a result. It is invisible to both filtering and output.
- An **unregistered** process discloses no env value at all — not by name, not
  by value — and cannot be filtered on one. A query cannot be used as an
  oracle against a process that never opted in.
- Asking is not authorisation. `include_env` requests values; the allowlist
  decides whether any exist to send.

## Who can talk to the daemon

| Ingress | Authorised by |
|---|---|
| Control socket | **Peer credentials** — the OS reports who connected; nothing the client sends can change that answer, and no secret is transmitted. |
| HTTP | **Bearer token**, read from an owner-only discovery file. |

Peer credentials are read off the socket, never synthesised from the daemon's
own configuration — a fabricated identity would make the owner check compare
the owner against itself and pass unconditionally.

Prefer the socket. The token is a secret in flight, and a secret in flight is
a secret that can leak.

### Loopback is not a user boundary

Binding to `127.0.0.1` keeps the HTTP surface off the network. It does *not*
keep it away from other local accounts, and on Windows not from other
sessions. A TCP listener has no peer credentials to lean on.

So the token is mandatory on **every** route, the landing page included —
there is no "just the UI" tier, because the UI is what calls the API.
Comparison is constant-time: a `==` returns as soon as two bytes differ, which
over enough requests reveals the token a byte at a time.

A non-loopback bind is refused **before the socket is created**, so a rejected
address never briefly exists as a listening socket.
`RUNNING_PROCESS_PROBE_BIND_ALL=1` opts out deliberately; it publishes every
registered process, every crash artifact, and a stack-capture trigger to the
network behind one token.

## Crash history is redacted metadata only

A crash query returns class, name, version, instance, pid, signature, timing,
fault kind, and size. It does not return:

- the **inline crash report** — that is what the redaction rule exists for;
- the **artifact path** — a daemon-private path discloses the owner's
  directory layout, so callers get an opaque id and fetch bytes through the
  artifact endpoint, which resolves the id itself.

Artifacts themselves are owner-private on disk, and the fetch path refuses to
serve any file the daemon did not write.

## The browser UI phones nobody

Every asset is compiled into the binary and served by the daemon. The flame
graph page carries `Content-Security-Policy: default-src 'none'`, so an
external reference introduced later fails loudly in the browser rather than
working only where its author tested it. Tests assert on the served bytes that
no asset references an external host.

Two reasons: the machine you are debugging is disproportionately likely to
have no working network, and a diagnostic page that phoned a third party would
disclose *that* you are debugging, and what, to whoever served it.

## What the probe deliberately does not do

- **Never auto-elevates.** No privilege escalation, and no modification of
  `ptrace_scope`.
- **Never injects into the main crate.** All injection symbols live in
  `running-process-probe`; the published `running-process` crate is free of
  `CreateRemoteThread` and `dlopen`-of-interposer, which matters for AV/EDR
  static analysis.
- **Never parses symbol files in the daemon.** A malformed symbol file can
  crash a parser outright rather than returning an error, so symbolization
  runs in a short-lived child process. Isolation is a process boundary, not a
  `catch_unwind`.
