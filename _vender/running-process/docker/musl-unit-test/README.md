# musl x86 unit-test docker harness

Local reproduction of the `x86-musl / unit-test (linux-x86-musl)` CI
lane (PR #514 / issue #513), modeled on soldr's
`docker/cook-shared-cache/Dockerfile` + `bench/cook_in_docker.sh`.

## Why

CI's `setup-soldr@v0` runs `cargo test --target x86_64-unknown-linux-musl`
on an Ubuntu glibc runner. The managed rustup home doesn't always have
the musl rust-std installed, so the build fails with `can't find crate
for std`. This image lets you run the same test invocation on a host
where the rust-std for musl is the native rust-std (Alpine, host triple
`x86_64-unknown-linux-musl`) — confirming whether a failure is a real
workspace issue vs. a CI environment issue.

## Volume layout

Three named volumes — soldr's #593 pattern — so cargo's mtime-based
fingerprint check survives container restarts (without this, Windows +
Docker Desktop's WSL2 9P layer rewrites mtimes on every run and cargo
rebuilds the workspace from scratch):

- `rp-musl-target` → `/work/target` (cargo build state, persistent).
- `rp-musl-cargo-home` → `/root/.cargo` (cargo registry index, persistent).
- `rp-musl-soldr-home` → `/root/.soldr` (soldr state, persistent).

Source tree at `/work` is bind-mounted from the host (read-write —
the build scripts in `crates/running-process/build.rs` write generated
prost output back there). All three volumes can be wiped with:

    docker volume rm rp-musl-target rp-musl-cargo-home rp-musl-soldr-home

## Usage

    ci/musl_in_docker.sh                        # default: cargo nextest run
    ci/musl_in_docker.sh cargo test --workspace # any other cargo invocation
    ci/musl_in_docker.sh shell                  # interactive bash

The runner script handles `docker build` (cached after the first call)
and the named-volume `docker run` flags.
