# Guest half of the `winvm` local-gate lane (ci/winvm_lane.py).
#
# Runs inside the warm dockur/windows VM over SSH, read from the host's
# shared folder (\\host.lan\Data\soldr-winvm). It replays the Linux-built
# x86_64-pc-windows-msvc nextest archive natively, the way
# _ci-target-run.yml does on windows-2025, reusing the guest probe's tool
# staging (ci/windows_guest_probe.ps1: signed VC++ runtime, pinned MinGit).
#
#   -Phase prepare  pin the VM (Windows Update off, Defender exclusion),
#                   install missing tools, copy the payload to local disk,
#                   provision the pinned Rust toolchain through soldr.exe,
#                   extract the archive and write the full test inventory.
#   -Phase run      run the host-computed owned filter; copy JUnit back.
#
# PowerShell 5.1: a native program's stderr becomes an error record under
# 'Stop', so native exit codes are judged explicitly instead.
param(
    [Parameter(Mandatory = $true)][ValidateSet('prepare', 'run')][string]$Phase,
    [Parameter(Mandatory = $true)][string]$Channel
)
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$share = '\\host.lan\Data\soldr-winvm'
# Short root: soldr's tests probe MAX_PATH behaviour and nest deep caches.
$root = 'C:\swv'
$run = "$root\run"
$ws = "$run\ws"
$extract = "$run\x"
$tools = "$root\tools"
$mingit = "$tools\mingit-2.55.0.5"

function Fail([string]$message) {
    Write-Output "winvm guest: $message"
    exit 1
}

function Step([string]$message) {
    Write-Output "winvm guest: $(Get-Date -Format HH:mm:ss) $message"
}

function Disable-WindowsUpdate {
    # Reproducible runs: KB5050575 auto-installed into the warm image once.
    # Policy first (survives a service restart), then the services. Medic
    # and the orchestrator tasks are protected on some builds; report, never
    # fail the lane on them.
    $au = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU'
    New-Item -Path $au -Force | Out-Null
    New-ItemProperty -Path $au -Name NoAutoUpdate -Value 1 -PropertyType DWord -Force | Out-Null
    New-ItemProperty -Path $au -Name AUOptions -Value 1 -PropertyType DWord -Force | Out-Null
    foreach ($svc in 'wuauserv', 'UsoSvc', 'WaaSMedicSvc') {
        Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue
        try {
            Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\$svc" -Name Start -Value 4 -ErrorAction Stop
        } catch {
            Write-Output "winvm guest: note: could not disable ${svc}: $($_.Exception.Message)"
        }
    }
    Get-ScheduledTask -TaskPath '\Microsoft\Windows\UpdateOrchestrator\' -ErrorAction SilentlyContinue |
        ForEach-Object { Disable-ScheduledTask -InputObject $_ -ErrorAction SilentlyContinue | Out-Null }
}

function Install-Tools {
    if (-not (Test-Path "$env:WINDIR\System32\vcruntime140.dll")) {
        $redist = "$share\vc_redist.x64.exe"
        if (-not (Test-Path $redist)) { Fail 'vcruntime140.dll is missing and the host staged no VC++ runtime' }
        $signature = Get-AuthenticodeSignature -FilePath $redist
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
            Fail "VC++ runtime signature is not Microsoft-valid: $($signature.Status)"
        }
        Step 'installing the VC++ runtime'
        # soldr#3295 (4e69caab): CreateProcess on a local copy, not
        # ShellExecute from \\host.lan -- a desktop edition shows the
        # Internet-zone "Open File - Security Warning" for a share launch and
        # waits forever for a click. Bounded, like the probe.
        $local = "$tools\vc_redist.x64.exe"
        Copy-Item -LiteralPath $redist -Destination $local -Force
        $info = New-Object Diagnostics.ProcessStartInfo
        $info.FileName = $local
        $info.Arguments = '/install /quiet /norestart'
        $info.UseShellExecute = $false
        $proc = [Diagnostics.Process]::Start($info)
        if (-not $proc.WaitForExit(900000)) {
            $proc.Kill()
            Fail 'VC++ runtime installer did not exit within 900 s'
        }
        if ($proc.ExitCode -notin @(0, 3010) -or -not (Test-Path "$env:WINDIR\System32\vcruntime140.dll")) {
            Fail "VC++ runtime install did not provide vcruntime140.dll (exit $($proc.ExitCode))"
        }
    }
    if (-not (Test-Path "$mingit\cmd\git.exe")) {
        if (-not (Test-Path "$share\mingit.zip")) { Fail 'git is missing and the host staged no MinGit' }
        Step 'installing MinGit'
        Expand-Archive -LiteralPath "$share\mingit.zip" -DestinationPath $mingit -Force
        if (-not (Test-Path "$mingit\cmd\git.exe")) { Fail 'MinGit archive has no cmd\git.exe' }
    }
}

function Set-RunEnvironment {
    $env:PATH = "$mingit\cmd;$env:PATH"
    # Mirrors _ci-target-run.yml's job env and "Resolve packaged target tools".
    $env:CI = 'true'
    $env:SOLDR_TEST_ISOLATED = '1'
    $env:SOLDR_BIN = "$run\package\soldr.exe"
    $env:SOLDR_INTERNAL_DAEMON_EXE = "$run\package\soldr-daemon.exe"
    $env:SOLDR_TEST_WORKSPACE_ROOT = $ws
    $env:SOLDR_TEST_FIXTURES_DIR = "$ws\crates\soldr-cli\tests\fixtures"
    $env:SOLDR_TARGET_WARN_FREE_GB = '1'
    $env:SOLDR_TARGET_BLOCK_FREE_GB = '1'
    $env:SOLDR_USE_SYSTEM_CMAKE = '1'
    $env:RUSTUP_TOOLCHAIN = $Channel
}

function Set-ToolchainEnvironment {
    $cargo = (& $env:SOLDR_BIN rustup which cargo | Select-Object -Last 1)
    $rustc = (& $env:SOLDR_BIN rustup which rustc | Select-Object -Last 1)
    if (-not $cargo -or -not (Test-Path $cargo) -or -not $rustc -or -not (Test-Path $rustc)) {
        Fail "soldr rustup which did not name a provisioned cargo/rustc ($cargo, $rustc)"
    }
    # <RUSTUP_HOME>\toolchains\<toolchain>\bin\cargo.exe (soldr#3195).
    $home4 = Split-Path (Split-Path (Split-Path (Split-Path $cargo)))
    if ((Split-Path -Leaf (Split-Path (Split-Path (Split-Path $cargo)))) -ne 'toolchains') {
        Fail "cannot derive RUSTUP_HOME from $cargo"
    }
    $env:CARGO = $cargo
    $env:RUSTC = $rustc
    $env:RUSTUP_HOME = $home4
    # A windows-2025 runner ships rustup with CARGO_HOME exported and its
    # bin on PATH; `soldr exec` resolves cargo there. soldr's provisioned
    # rustup lives beside RUSTUP_HOME.
    $cargoHome = Join-Path (Split-Path $home4) 'cargo'
    if (-not (Test-Path "$cargoHome\bin\rustup.exe")) { Fail "no rustup under $cargoHome\bin" }
    $env:CARGO_HOME = $cargoHome
    $env:PATH = "$cargoHome\bin;$env:PATH"
    Step "toolchain: CARGO=$cargo RUSTUP_HOME=$home4 CARGO_HOME=$cargoHome"
}

function Get-ReuseArgs {
    $binaries = Get-ChildItem -Path $extract -Recurse -Depth 4 -Filter 'binaries-metadata.json' -ErrorAction SilentlyContinue | Select-Object -First 1
    $cargoMeta = Get-ChildItem -Path $extract -Recurse -Depth 4 -Filter 'cargo-metadata.json' -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $binaries -or -not $cargoMeta -or -not (Test-Path "$extract\target")) {
        return @('--archive-file', "$run\tests.tar.zst", '--extract-to', $extract, '--extract-overwrite')
    }
    return @('--binaries-metadata', $binaries.FullName, '--cargo-metadata', $cargoMeta.FullName, '--target-dir-remap', "$extract\target")
}

$nextest = "$run\cargo-nextest.exe"
if ($Phase -eq 'prepare') {
    Step 'pinning the VM (Windows Update off, Defender exclusion)'
    Disable-WindowsUpdate
    New-Item -ItemType Directory -Force -Path $root, $tools | Out-Null
    Add-MpPreference -ExclusionPath $root -ErrorAction SilentlyContinue
    Install-Tools

    Step 'copying the payload to local disk'
    if (Test-Path $run) { Remove-Item -LiteralPath $run -Recurse -Force -ErrorAction SilentlyContinue }
    if (Test-Path $run) { Fail "could not clear the previous run at $run (a process still holds it?)" }
    New-Item -ItemType Directory -Force -Path $run, $ws, $extract, "$run\package" | Out-Null
    Copy-Item "$share\tests.tar.zst", "$share\cargo-nextest.exe", "$share\workspace.tar" -Destination $run
    Copy-Item "$share\soldr.exe" -Destination "$run\package\soldr.exe"
    Copy-Item "$share\soldr.exe" -Destination "$run\package\soldr-daemon.exe"
    & tar.exe -xf "$run\workspace.tar" -C $ws
    if ($LASTEXITCODE -ne 0) { Fail "workspace extraction failed (exit $LASTEXITCODE)" }
    Remove-Item -LiteralPath "$run\workspace.tar" -Force

    Set-RunEnvironment
    Push-Location $ws
    try {
        Step "provisioning Rust $Channel through soldr.exe"
        & $env:SOLDR_BIN toolchain ensure --json
        if ($LASTEXITCODE -ne 0) { Fail "soldr toolchain ensure failed (exit $LASTEXITCODE)" }
        & $env:SOLDR_BIN toolchain link --shim-dir "$run\shims" --json
        if ($LASTEXITCODE -ne 0) { Fail "soldr toolchain link failed (exit $LASTEXITCODE)" }
        Set-ToolchainEnvironment

        Step 'extracting the archive and listing every test'
        & $nextest nextest list --archive-file "$run\tests.tar.zst" --extract-to $extract --workspace-remap $ws --profile target-run --message-format json-pretty > "$run\list.json"
        if ($LASTEXITCODE -ne 0) { Fail "nextest list failed (exit $LASTEXITCODE)" }
        # Windows PowerShell 5.1 '>' writes UTF-16; hand the host UTF-8.
        Get-Content -LiteralPath "$run\list.json" | Set-Content -LiteralPath "$share\list.json" -Encoding UTF8
    } finally {
        Pop-Location
    }
    Step 'prepared'
    exit 0
}

# -Phase run
Set-RunEnvironment
Push-Location $ws
try {
    Set-ToolchainEnvironment
    $filter = (Get-Content -LiteralPath "$share\filter.txt" -Raw).Trim()
    if (-not $filter) { Fail 'the host staged an empty filter' }
    $reuse = Get-ReuseArgs
    Step "running the owned Windows MSVC partition ($($filter.Length)-char filter)"
    & $nextest nextest run @reuse --workspace-remap $ws --profile target-run -E $filter --no-fail-fast
    $code = $LASTEXITCODE
    $junit = Get-ChildItem -Path $extract, $ws -Recurse -Filter 'junit.xml' -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match 'nextest\\target-run' } | Select-Object -First 1
    if ($junit) { Copy-Item $junit.FullName -Destination "$share\junit.xml" -Force }
    else { Write-Output 'winvm guest: nextest wrote no junit.xml' }
    Step "nextest exit $code"
    exit $code
} finally {
    Pop-Location
}
