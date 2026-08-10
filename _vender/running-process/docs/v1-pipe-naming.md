# v1 Pipe Naming

This document defines the v1 broker pipe and socket naming contract.

The implementation uses compact names so every path stays inside Windows named
pipe limits and Unix `sockaddr_un.sun_path` limits.

## Name Inputs

| Input | Rule |
|---|---|
| `user_sid_hash` | 16 lowercase hex characters |
| `service` | `[a-z0-9-]{1,64}` |
| `instance` | `[a-z0-9-]{1,64}` |
| `random128` | 16 random bytes rendered as 32 lowercase hex characters |

Uppercase service and instance names are rejected. This prevents case-only
collisions on Windows named pipes and case-insensitive filesystems.

## Canonical Classes

| Class | Canonical leaf |
|---|---|
| Shared broker | `rpb-v1-{user_sid_hash}-shared` |
| Private broker | `rpb-v1-{user_sid_hash}-svc-{service}` |
| Explicit instance | `rpb-v1-{user_sid_hash}-inst-{instance}` |
| Backend pipe | `rpb-v1-{user_sid_hash}-be-{random128}` |

Backend pipes use a random 128-bit suffix generated per backend bind. This
prevents predictable backend pipe squatting.

`server::BackendEndpointAllocator` owns runtime allocation for backend pipes.
It draws 16 bytes from the OS random source, calls the frozen
`backend_pipe(user_sid_hash, random128)` formatter, records the generated path
as reserved for this broker process, and retries on duplicate reservations.
The allocator returns an `Endpoint` whose `namespace_id` is the owning broker
instance and whose `path` is the platform pipe/socket string clients receive in
`Negotiated.backend_pipe`.

## Windows

Windows uses named pipes:

```text
\\.\pipe\rpb-v1-deadbeefdeadbeef-shared
\\.\pipe\rpb-v1-deadbeefdeadbeef-svc-zccache
\\.\pipe\rpb-v1-deadbeefdeadbeef-inst-ci-trusted
\\.\pipe\rpb-v1-deadbeefdeadbeef-be-abababababababababababababababab
```

The path stays below `MAX_PATH` without the `\\?\` prefix.

## Linux

Linux uses filesystem Unix-domain sockets under the broker runtime directory:

```text
$XDG_RUNTIME_DIR/running-process/broker/rpb-v1-deadbeefdeadbeef-shared.sock
$XDG_RUNTIME_DIR/running-process/broker/rpb-v1-deadbeefdeadbeef-svc-zccache.sock
$XDG_RUNTIME_DIR/running-process/broker/rpb-v1-deadbeefdeadbeef-inst-ci-trusted.sock
$XDG_RUNTIME_DIR/running-process/broker/rpb-v1-deadbeefdeadbeef-be-abababababababababababababababab.sock
```

When `XDG_RUNTIME_DIR` is unset, the fallback directory is:

```text
/tmp/running-process-{uid}/broker/
```

The broker creates the directory with current-user-only permissions before
binding.

## macOS

macOS has a short `sun_path` limit, so the socket leaf is always a 16-character
hash of the canonical leaf:

```text
$TMPDIR/.rp-{uid}/{hash16}.sock
```

Each canonical class still hashes from the same v1 leaf strings listed above.
The raw `rpb-v1` prefix and SID hash do not appear in the final macOS path.

## User Identity Hash

The user identity hash scopes broker names to one user on one host:

| Platform | Identity material |
|---|---|
| Windows | Current process token SID bytes |
| Linux | UID and machine id |
| macOS | UID and platform UUID |

The hash is the first 8 bytes of a BLAKE3 digest, rendered as 16 lowercase hex
characters.

## Validation Failures

Invalid names fail before any pipe bind:

- empty name
- name longer than 64 bytes
- uppercase ASCII letters
- dots, underscores, spaces, slashes, and non-ASCII characters
- invalid semantic version when a caller validates `wanted_version`
- derived path longer than the platform limit

Validation failures are configuration errors. The broker reports them with a
stable `Refused` error code or local configuration error.
