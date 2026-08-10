# Probe symbol discovery

The probe symbolizer selects symbol artifacts by exact build identity. A
filename is only a location hint: it never makes a candidate trustworthy.
Untrusted symbol artifacts and heavy symbol-file parsers stay inside the
short-lived `running-process-probe-worker` process. The target performs only
bounded, lightweight parsing of mapped image identity and module metadata.

## Discovery order

The worker checks these sources in order and stops at the first candidate whose
own bytes have the exact expected identity:

1. symbols embedded in the captured module;
2. a manifest adjacent to the module;
3. a manifest or symbol path declared during process registration;
4. the platform's adjacent native symbol file;
5. an identity-keyed local cache;
6. configured local stores, followed by opt-in HTTP(S) symbol servers.

An absent candidate leaves the frame as a raw module-relative address. A
candidate with the wrong ELF build-id, Mach-O UUID, or PDB GUID and age is
refused and discovery continues. It never produces a best-effort function
name.

The daemon owns registration state and copies its symbol declarations into the
capture lease. A target cannot replace those declarations in capture data.
The capture records the authoritative build identity from loaded-module
metadata: ELF `PT_NOTE`, Mach-O `LC_UUID`, or mapped PE CodeView. It also
records a bounded SHA-256 of every referenced module pathname as a later
TOCTOU check. The worker refuses symbolization if that path changes, but never
uses the pathname to redefine the captured build.

## Manifest

Adjacent manifests use either `<image>.rpprobe-symbols.json` or the image path
with its extension replaced by `.rpprobe-symbols.json`. Registered manifests
may live elsewhere. `_native.tiny-pdb.json` is a build-time public-symbol
filter list and is not this runtime format.

```json
{
  "schema": "running-process-probe-symbol-manifest/v1",
  "modules": [
    {
      "name": "app",
      "identity": {
        "kind": "elf_build_id",
        "hex": "0123456789abcdef"
      },
      "artifacts": [
        {
          "format": "elf_dwarf",
          "storage": {
            "kind": "relative_path",
            "path": "app.debug"
          },
          "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
          "expected_size": 123456
        }
      ]
    }
  ]
}
```

`name` is display-only. `identity` selects an entry and must be one of
`elf_build_id`, `macho_uuid`, or `pe_pdb` (with `guid` and `age`). Hexadecimal
identities are case-insensitive; PDB GUIDs and Mach-O UUIDs accept either
32-digit compact or conventional hyphenated spelling. The worker also extracts
the candidate artifact's identity from its bytes before parsing symbols.
Manifest-relative paths must not be absolute, drive-relative, or contain `..`.
Manifests, entry counts, artifact counts, local artifacts (256 MiB), and
downloads are bounded. Manifest artifacts are canonicalized and must remain
beneath the canonical manifest directory, so a relative symlink cannot escape.

ELF and Mach-O symbols already in the native image are the first production
source. For GNU MiniDebugInfo, the worker extracts the XZ-compressed
`.gnu_debugdata` ELF into worker-private temporary storage, applies the same
256 MiB decompression cap, and verifies its build-id before parsing it.

## Registration and local stores

Library users can declare a manifest or one or more symbol locations on probe
configuration:

```rust
let config = running_process::probe::Config::default()
    .with_symbol_manifest("symbols/process.rpprobe-symbols.json")
    .with_symbol_path("symbols");
```

Relative registration paths are converted to absolute paths before
registration. Platform cache layouts are identity keyed:

- ELF: `<root>/.build-id/ab/cdef.debug`;
- PDB: `<root>/<pdb-name>/<GUIDAGE>/<pdb-name>`;
- Mach-O: `<root>/<uuid>/<dSYM-relative-object-path>`.

The identity cache defaults to
`<running-process state>/probe-symbol-cache`. On Unix the state base follows
`XDG_STATE_HOME`, then `~/.local/state`; on Windows it follows
`LOCALAPPDATA`. If none of those variables are available, the fallback is
`/tmp/running-process-state/probe-symbol-cache` on Unix and
`C:\ProgramData\running-process\probe-symbol-cache` on Windows.
`RUNNING_PROCESS_PROBE_BUILD_ID_CACHE` replaces that default with an explicit
path list, and `RUNNING_PROCESS_PROBE_SYMBOL_PATH` supplies additional
local-store roots.

## Symbol servers

Network discovery is off by default. A daemon administrator can opt in with a
comma-separated `RUNNING_PROCESS_PROBE_SYMBOL_SERVERS` list. Only
credential-free HTTP(S) base URLs without query strings or fragments are
accepted. Redirects and content decoding are disabled. At most eight origins
are consulted sequentially; each response is capped at 64 MiB and the total at
128 MiB. Each download lands in worker-private temporary storage and is
identity-checked and parsed immediately, stopping at the first usable exact
build.

Server routes use native identity keys:

- ELF: `/buildid/<build-id>/debuginfo`;
- PDB: `/<pdb-name>/<GUIDAGE>/<pdb-name>`;
- Mach-O: `/<uuid>/<dSYM-relative-object-path>`.
