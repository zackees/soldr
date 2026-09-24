# Windows guest feasibility probe (#3295)

`windows-guest-probe.yml` is dispatch-only. It reuses the established Linux
cross-build of `x86_64-pc-windows-msvc`, then boots an ephemeral Windows Server
2025 Core or Windows 11 Enterprise evaluation guest under upstream
dockur/windows v6.03. The container
image is pinned by OCI digest in `ci/windows_guest_probe.py`; no guest image is
uploaded, cached, or kept after the job. The script stages the verified
`cargo-nextest.exe` and cross-built archive, runs `soldr-core` tests in the
guest, and reports actual run/passed/failed counts plus host/guest capabilities.
Server Core lacks `vcruntime140.dll` in the first observed run, so the probe
downloads [Microsoft's documented x64 VC++ Redistributable permalink](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170) on the
host, verifies the pinned SHA-256 in `ci/windows_guest_probe.py`, and checks
its Microsoft Authenticode signature in the guest before an unattended
install. The original `vcruntime140` capability records the cold image;
`vcruntime140_after` records whether the installer supplied it. No installer
or guest disk is committed or cached.
Server Core also lacks `git.exe`: four of the 43 selected `soldr-core`
tests failed on `git init` in the first nonempty replay. The probe stages
[official MinGit v2.55.0(5)](https://github.com/git-for-windows/git/releases/tag/v2.55.0.windows.5)
with the release asset's SHA-256 pin, extracts it into the ephemeral guest,
and runs the same test selection without exclusions. The report measures
this preparation separately from VC++ installation and Nextest replay.

Run it from Actions → **Windows guest feasibility probe** → Run workflow. The
`guest_edition` input defaults to `server-core`; select `win11-enterprise` for
a full Windows 11 desktop comparison using upstream dockur's `11e` image
selector. Both selections reuse the same Linux-built test archive and the
same guest script, so boot/disk/capability differences are directly comparable.
Each dispatch uses a fresh disk and discards it after reporting.

The report and container log are in the job summary and the
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
unmeasured phase met the budget. Host-timestamped markers separate capability
and signature preparation, the VC++ installer, and native Nextest replay.
The script records the allocated and apparent
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

For the desktop comparison, [Microsoft's Windows 11 Enterprise evaluation](https://www.microsoft.com/en-us/evalcenter/evaluate-windows-11-enterprise)
is a full-featured 90-day trial. That permits a one-shot feasibility study,
not an assumption that an indefinitely recurring unactivated CI guest is
licensed. A recurring lane needs a separate licensing decision.

The Server Core result does not prove Windows 11 UI/WebView2 capability.
Use the `win11-enterprise` selection to measure the desktop guest;
`webview2` and `gpu_rendering` are reported separately. A consumer requiring
screenshots still needs an actual UI-test and license decision, not a
capability-name inference.
