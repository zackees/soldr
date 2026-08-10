# Datalake release-musl regression tracker

Status: fixed / no longer reproducible on `soldr 0.8.0+`.

The original downstream workaround bypassed Soldr's compiler cache for the
release job. That workaround is no longer part of the supported runbook.
Release/LTO builds use the same mandatory broker SESSION route as other
cacheable compiles.

## Validation command

```bash
soldr cargo build --release --locked --target x86_64-unknown-linux-musl
```

Run the same toolchain, target, features, and release profile as production.
The entire Cargo invocation may run for hours; the compile-reply timeout is a
per-unit no-response backstop, not a whole-build deadline.

For an unusually long but healthy LTO unit, raise the positive backstop:

```bash
SOLDR_COMPILE_REPLY_TIMEOUT_SECS=3600 soldr cargo build --release --locked --target x86_64-unknown-linux-musl
```

For a suspected wedge, shorten the backstop on a diagnostic run so it fails
with attributable evidence. Capture `soldr logs paths`, `soldr daemon status`,
the broker/daemon route identity, and the build/compile journals.

Recovery is broker-owned: use `soldr daemon stop`, then `soldr daemon start`.
Do not manually place or spawn a daemon, and do not switch a cacheable compile
onto a second execution mode.
