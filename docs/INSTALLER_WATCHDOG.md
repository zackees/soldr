# Installer progress watchdog

Long installation work is supervised by progress, not by a short default
wall-clock deadline. The shared watcher terminates an installer only when it
has made no observable progress for `SOLDR_INSTALLER_STALL_TIMEOUT_SECS`
(default: 15 minutes), or when it crosses the deliberately large
`SOLDR_INSTALLER_SAFETY_TIMEOUT_SECS` runaway-process ceiling (default:
24 hours).

Output is streamed to the terminal and resets the stall clock. On Linux, CPU
movement in the process tree also counts as progress, so a quiet compiler does
not look hung merely because it has no status line to print.

## Installer inventory

| Installer family | Phase in diagnostics | Legacy explicit ceiling | Normal progress evidence |
|---|---|---|---|
| Bootstrap program | `bootstrap` | `SOLDR_RUSTUP_INIT_TIMEOUT_SECS` | program output or CPU activity |
| Target provisioning | `target-install` | `SOLDR_RUSTUP_TARGET_ADD_TIMEOUT_SECS` | program output or CPU activity |
| Toolchain and plugin provisioning | `toolchain-prepare` | `SOLDR_TOOLCHAIN_COMMAND_TIMEOUT_SECS` | program output or CPU activity |
| Source-built tool installation | `source-build` | `SOLDR_BUILD_FROM_SOURCE_INSTALL_TIMEOUT_SECS` | compiler/output activity |

The legacy variables remain supported. A positive value is now an explicit
maximum-runtime ceiling for that operation; without one, all four families use
the shared 24-hour safety ceiling. Invalid, zero, or empty values fall back to
the shared defaults.

When the watcher intervenes it names `category` (`stall` or
`safety-ceiling`), `phase`, `total_elapsed`, and `since_progress`, along with
the effective stall timeout and safety ceiling. `soldr doctor` reports the two
shared controls and whether their overrides were honoured.
