# v1 Platform Behavior

The v1 broker exposes one logical contract across Linux, macOS, and Windows.
Each platform uses its native process, pipe, credential, and filesystem
primitives.

## Behavior Table

| Concern | Linux | macOS | Windows |
|---|---|---|---|
| PID identity | `pidfd_open` on kernel 5.3 and newer; `/proc/<pid>/exe` fallback | `kqueue` with `NOTE_EXIT` | `OpenProcess` plus `QueryFullProcessImageName` |
| Process lifetime | `PR_SET_PDEATHSIG` registered by child after fork | Supervisor child uses `kqueue` to watch parent death | Job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and `CREATE_BREAKAWAY_FROM_JOB` |
| Idle clock | `CLOCK_BOOTTIME` | `CLOCK_UPTIME_RAW` | `GetTickCount64` |
| Atomic file swap | `rename(2)` plus parent directory `fsync` | `rename(2)` plus parent directory `fsync` | `ReplaceFileW` |
| File lock | `flock` with fd-attached lifetime | `flock` | `LockFileEx` with bounded ghost-lock recovery |
| Runtime dir | `$XDG_RUNTIME_DIR`, then `/tmp/running-process-{uid}` fallback | `$TMPDIR/.rp-{uid}` | `%LOCALAPPDATA%\running-process\run` |
| Manifest dir | `$XDG_DATA_HOME/running-process/manifests` | `~/Library/Application Support/running-process/manifests` | `%APPDATA%\running-process\manifests` |
| Service-def dir | `$XDG_CONFIG_HOME/running-process/services` | `~/Library/Application Support/running-process/services` | `%APPDATA%\running-process\services` |
| Pipe or socket | Filesystem Unix-domain socket | Filesystem Unix-domain socket with hashed leaf | Named pipe |
| Peer credential | `SO_PEERCRED` | `LOCAL_PEERCRED` | `GetNamedPipeClientProcessId` plus process token |
| Quarantine strip | Not applicable | Remove `com.apple.quarantine` from relocated binaries | Remove `Zone.Identifier` alternate data stream |
| Shutdown signal | SIGTERM, drain, force exit | SIGTERM, drain, force exit | `SetConsoleCtrlHandler` for console and session events |
| Boot id | `/proc/sys/kernel/random/boot_id` | `kern.boottime` sysctl | boot epoch derived from unbiased interrupt time |
| Container detection | `/.dockerenv`, `/run/.containerenv`, cgroup parsing | Not applicable | Windows container environment markers |
| Minimum version | Linux 5.3 for pidfd fast path, with fallback | macOS 10.15 | Windows 10 1809 |

## Rationale

The broker uses native primitives instead of emulating a lowest-common
denominator:

- Unix-domain sockets and Windows named pipes provide local IPC with OS-level
  permissions.
- Peer credential APIs let the broker verify the caller identity independently
  from self-reported fields.
- Platform lifetime primitives let child backends exit when the launching
  context dies.
- Atomic file replacement uses APIs with documented durability semantics on
  each platform.

## Drift Rule

Every cross-platform behavior has an explicit platform row. A feature that
exists on only one platform stays documented as platform-specific until all
supported platforms have an equivalent behavior.
