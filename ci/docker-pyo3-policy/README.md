# PyO3 policy Docker harness

This harness validates the target-aware policy from a Linux container using
the current source tree. It covers a native PEP 517 build and import, a
PyO3-free Windows cross-build, and ABI3 extension builds for Windows x64 and
both supported macOS targets. The final assertion proves none of these paths
materialized the opt-in target-Python compatibility sysroot.

Run from the repository root with the standard clud soldr development
container. The tool creates and reuses the named build volumes:

```sh
clud tool run docker/docker-build.py soldr "$PWD" init
clud tool run docker/docker-build.py soldr "$PWD" up
clud tool run docker/docker-build.py soldr "$PWD" run -- \
  bash ci/docker-pyo3-policy/build.sh
```

`SOLDR_PYO3_COMPATIBILITY=sysroot` is tested separately by
`python_compat_sysroot`, which uses a controlled local HTTP catalogue and
bundle server and verifies the exact two-request sequence plus SHA-256.
