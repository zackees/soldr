# Embedded native link/archive routing audit (#1534)

## Result

The embedded soldr service currently exposes compile requests to the native
shim, while zccache's link/archive cache is a separate request family. Do not
inject archive or linker wrappers until the embedded protocol and lifecycle
can carry the full deterministic request contract.

## Current boundary

- Soldr's embedded native path injects the zccache shim through `CC` and `CXX`
  for compiler invocations from native build scripts.
- The embedded daemon API carries `CompileRequest`; the standalone zccache
  protocol separately has `LinkEphemeral`, and zccache has dedicated
  archiver/linker parsers and cache keys.
- AR/RANLIB and final linker tools are therefore not silently routed through
  the compile-only embedded service. This preserves the existing fallback
  behavior and avoids treating an order-sensitive or multi-output operation as
  a compile cache hit.

## Why this is not enabled

Archive and link caching needs ordered inputs, response-file contents, tool
identity, relevant flags/environment, output side effects, deterministic
archive mode (`ar D`), MSVC `/BREPRO`, and mutation isolation. The embedded
path would also need scoped daemon accounting, error/fallback semantics, and
shutdown coverage for the new request family.

The requested sqlite-link and 100-object experiment is the right validation,
but no embedded implementation or three-repetition wall-time evidence exists
in the current tree. Wiring only `AR`/`RANLIB` would create a partial path with
unclear hit semantics and would not satisfy the determinism requirements.

## Reopening criteria

Reopen after the embedded protocol can carry deterministic link/archive
requests and tests cover response files, input order, changed members, link
flags, secondary outputs, and daemon fallback. Then compare cold and warm wall
time against standalone zccache and the current soldr path; close the
hypothesis if warm time does not improve materially.
