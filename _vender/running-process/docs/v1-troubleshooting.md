# v1 Troubleshooting

This guide lists operator-facing broker failure modes and the matching first
checks.

## First Check: `doctor`

Run the read-only environment diagnostics before anything else:

```text
running-process-broker-v1 doctor
running-process-broker-v1 doctor --json
```

`doctor` checks every `RUNNING_PROCESS_*` environment knob, broker endpoint
reachability (with a Hello probe reporting daemon version, protocol, and
capabilities), service-definition directory health and per-`.servicedef`
validation, stale Unix socket files, the platform path budget, and build
versions. It never repairs anything. Exit code `1` means at least one check
FAILed. See [v1 admin verbs](v1-admin-verbs.md) for the full check list.

## Broker Not Running

Symptoms:

- client falls back to direct daemon mode
- `Hello` connection fails
- `readyz` cannot connect

Checks:

1. Confirm `RUNNING_PROCESS_DISABLE` is not `1`.
2. Run `running-process-broker-v1 healthz`.
3. Check the broker pipe path in [v1 pipe naming](v1-pipe-naming.md).
4. Inspect the lifecycle log for `SPAWN_ATTEMPT`, `HELLO_REFUSED`, and
   `SECURITY_VIOLATION`.

## Service Unknown

Symptoms:

- `Refused.code` is `ERROR_SERVICE_UNKNOWN`
- `status --json` has no entry for the service

Checks:

1. Verify the `.servicedef` file exists in the platform service directory.
2. Verify the parent directory is current-user-only.
3. Verify `service_name` matches `[a-z0-9-]{1,64}`.
4. Run `config --effective --json` for the broker instance and inspect
   `paths.service_definition_dir`.

## Version Refused

Symptoms:

- `Refused.code` is `ERROR_VERSION_BLOCKED`
- the requested backend never starts

Checks:

1. Compare `wanted_version` with `ServiceDefinition.min_version`.
2. Check `version_allow_list`.
3. Confirm the backend binary exists under `per_version_binary_dir`.

## Stale Manifests

Symptoms:

- cleanup reports a live daemon that is no longer running
- restored CI cache points at an old boot id
- backend pipe paths in diagnostics no longer exist

Checks:

1. Compare manifest `boot_id` with the current boot id.
2. Run `running-process-cleanup verify`.
3. For GitHub Actions cache restore, run verification after cache restore and
   before starting consumers.

## Spawn Budget Exhausted

Symptoms:

- `Refused.code` is `ERROR_RATE_LIMITED`
- `retry_after_ms` is nonzero
- lifecycle log contains repeated `DIED_CRASH` or spawn failures

Checks:

1. Inspect backend stderr and lifecycle logs.
2. Verify the backend binary hash matches the manifest.
3. Wait for `retry_after_ms` before retrying.

## Pipe Permission Failure

Symptoms:

- `Refused.code` is `ERROR_PEER_REJECTED`
- lifecycle log contains `SECURITY_VIOLATION`

Checks:

1. Confirm client and broker run as the same intended user.
2. Verify Unix socket parent mode is `0700`.
3. Verify Windows named pipe ACL grants the current user.
4. Check for uppercase or non-canonical service names.

## GHA Cache Restore Issues

Symptoms:

- restored manifests reference another boot
- cleanup sees missing runtime paths
- service starts from stale cache state

Checks:

1. Run `running-process-cleanup verify --gha` after restore.
2. Drop manifests whose `boot_id` differs from the current runner.
3. Keep cache roots and manifest registry in the same cache key family.

## Cleanup Verification Checklist

`running-process-cleanup verify [--json] [--scope-hash <hash>]` (#391)
reconciles every artifact class the daemon can leave behind. It is
READ-ONLY: stale residue is reported, never deleted. Each location is
reported as `CLEAN`, `ACTIVE`, `PRESENT`, `STALE`, `ORPHANED`, or `ERROR`;
the exit code is nonzero only when a location could not be inspected
(`ERROR`).

| Class | Expected location | Stale / orphaned when |
|---|---|---|
| `socket` | Unix: `$XDG_RUNTIME_DIR/running-process/daemon{-hash}.sock`; Windows: `\\.\pipe\running-process-daemon-{user}{-hash}` (no filesystem residue) | socket file exists but nothing accepts connections |
| `pid-file` | Unix: socket dir, `daemon{-hash}.pid`; Windows: `%LOCALAPPDATA%\running-process\daemon{-hash}.pid` | recorded pid is dead or the file is unparsable |
| `servicedef` | platform service-definition dir (`*.servicedef`, see [v1 service definition](v1-service-definition.md)) | non-`.servicedef` entries in the directory are orphaned; the definitions themselves are persistent config |
| `database` | `tracked-pids{-hash}.sqlite3` in the daemon data dir (persists across runs) | `-wal` / `-shm` sidecars present with no live daemon (unclean shutdown) |
| `logs` | `*.log` files in the daemon data dir (none expected by default) | reported with count/size, never deleted |
| `emergency-reserve` | `emergency-reserve.bin` (32 MiB) next to the SQLite db (#390) | wrong size (partial pre-allocation); absence is clean — it is re-armed at every daemon startup |
| `shadow` | shadow-copy dir (`run/` under the platform data/runtime dir) | contents persist by design; reported with entry count for manual pruning |

The `--json` document extends the registry verify output with an additive
`artifacts` object: `{"schema_version":1,"exit_code":0,"findings":0,
"checks":[{"class":"socket","location":"...","status":"CLEAN","detail":"..."}]}`.

## systemd KillMode Reaps Spawned Children

Symptoms (Linux, daemon running as a systemd unit):

- spawned children die when the daemon's unit stops or restarts
- startup log contains `KillMode=control-group` warning

systemd's default `KillMode=control-group` kills every process in the
unit's cgroup on stop. The daemon detects this at startup (#391): when
`INVOCATION_ID` indicates systemd management it resolves the owning unit
from `/proc/self/cgroup` and queries `systemctl show -p KillMode <unit>`,
emitting a WARN when the mode is `control-group` or undeterminable. The
same assessment surfaces as the `platform:systemd-killmode` doctor check.

Fix: set `KillMode=process` (or `mixed`) in the unit file so children can
outlive the daemon (see `contrib/systemd/`).

## Network Filesystem Refusal

Symptoms:

- broker refuses to start
- spawn lock acquisition reports unsupported filesystem

Checks:

1. Move lock and runtime directories to a local filesystem.
2. Keep NFS, SMB, CIFS, FAT32, and exFAT outside broker-managed lock roots.

## Emergency Disable

Set:

```text
RUNNING_PROCESS_DISABLE=1
```

Then rerun the workload. If the workload succeeds in direct mode, collect a
diagnostic bundle before filing the broker issue.
