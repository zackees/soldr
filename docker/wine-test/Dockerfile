# Wine lane image (zackees/ci.yml#202 / #198, soldr#3534 phase 3): runs
# soldr's x86_64-pc-windows-msvc unit-test executables, cross-built on the
# Linux host, under Wine -- inside this container, never on the host
# (GATE-005). Pilot (2026-10-02): wine 10.0 passed soldr-platform 48/48,
# soldr-fetch 346/346 and soldr-nextest-wrapper 6/6; 3415/3581 across the
# workspace, the rest Wine API gaps (directory ACLs, named pipes, junctions)
# or tests needing a Windows toolchain -- those stay on native runners.
FROM debian:trixie@sha256:9cc080028c43b27d2074d63a5f9caf7166d731494965616c1a6d2827a004585c
RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends wine wine64 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
ENV WINEDEBUG=-all \
    WINEPREFIX=/opt/wineprefix \
    XDG_RUNTIME_DIR=/tmp
# Initialize the prefix once at build time (~4 s per run otherwise).
RUN wineboot -i && wineserver -w
