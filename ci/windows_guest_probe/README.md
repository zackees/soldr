# Windows guest feasibility probe (#3295)

`windows-guest-probe.yml` is dispatch-only. It reuses the established Linux
cross-build of `x86_64-pc-windows-msvc`, then boots an ephemeral Windows Server
2025 Core evaluation guest under upstream dockur/windows v6.03. The container
image is pinned by OCI digest in `ci/windows_guest_probe.py`; no guest image is
uploaded, cached, or kept after the job. The script stages the verified
`cargo-nextest.exe` and cross-built archive, runs `soldr-core` tests in the
guest, and reports actual run/passed/failed counts plus host/guest capabilities.

Run it from Actions → **Windows guest feasibility probe** → Run workflow. The
report and container log are in the job summary and the
`windows-guest-probe-measurements` artifact. A no-go is a legitimate probe
result, so the workflow itself is not a required check. The local host in
which this probe was developed has no `/dev/kvm`; bosn verifies the host-side
contracts and no-KVM path, while only an Ubuntu runner can supply boot/replay
evidence.

## Measurement limits

The runner measures ISO download from dockur log announcements and elapsed
time to the first OEM PowerShell script. Dockur does not expose a reliable
boundary between unattended install and first usable boot; the report labels
that combined interval `install_seconds` and leaves `boot_seconds` explicitly
unmeasured. Do not turn an absent marker into a zero or claim that an
unmeasured phase met the budget. The script records the allocated and apparent
disk size and streams zstd compression to a counter without creating a cache
artifact. Its cache-viability heuristic is only a size estimate; a later
implementation would still need restore-time and eviction measurements.

## Evaluation media and licensing

Dockur does **not** distribute Windows or grant a Windows license. Its `2025`
selector obtains Microsoft evaluation media, and this one-shot probe uses the
Server Core edition. [Microsoft's Evaluation Center](https://www.microsoft.com/en-us/evalcenter/download-windows-server-2025)
says Windows Server 2025 evaluation expires after 180 days and must be
activated online within the first 10 days to avoid shutdown. [Microsoft's
trial page](https://info.microsoft.com/ww-landing-evaluate-windows-server-2025.html)
describes a no-cost 180-day trial. A fresh, disposable evaluation boot is used
only to evaluate CI feasibility; these sources do **not** establish a right to
run an indefinite nightly fleet of guests. If the probe is technically viable,
confirm the applicable license terms before replacing native Windows runners
with a recurring guest lane. Do not cache or reuse an activated evaluation
disk to evade expiry.

The Server Core result does not prove Windows 11 UI/WebView2 capability.
`webview2` and `gpu_rendering` are reported separately; a future consumer
requiring them needs its own full-desktop probe and license decision.
