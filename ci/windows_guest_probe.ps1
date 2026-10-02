# One-shot, non-gating guest probe. Results and the native nextest log return
# through dockur's /shared mount (Z:); no guest disk or activation state
# outlives the dispatch run that created it.
#
# -Mode cold runs once from dockur's FirstLogonCommands on the freshly
# installed guest. It installs the verified runtime and MinGit, replays the
# archive, and registers a logon task for -Mode warm.
# -Mode warm runs from that task when the same installed disk boots again
# (restored from the run's disk-image artifact on a fresh runner). It measures
# a cached-image boot and replays the archive again without reinstalling.
param([ValidateSet('cold', 'warm')][string]$Mode = 'cold')

$ErrorActionPreference = 'Stop'
$prefix = if ($Mode -eq 'warm') { 'warm-' } else { '' }
$probeRoot = 'C:\soldr-probe'
$result = @{
    mode = $Mode
    capabilities = @{}
    nextest = @{ status = 'not-run'; run = 0; passed = 0; failed = 0; exit_code = $null }
}

function Write-Marker([string]$Name) {
    'ready' | Set-Content -Path (Join-Path 'Z:\' "$prefix$Name") -Encoding ASCII
}

function Wait-SharedDrive {
    # A cold FirstLogonCommands run follows dockur's `net use Z:`. On a warm
    # logon the persistent mapping may still be reconnecting, so retry and
    # remap the same share rather than failing on the first probe.
    for ($attempt = 0; $attempt -lt 90; $attempt++) {
        if (Test-Path 'Z:\') { return }
        if ($attempt % 10 -eq 5) {
            & cmd.exe /c 'net use Z: \\host.lan\Data /persistent:yes' | Out-Null
        }
        Start-Sleep -Seconds 2
    }
    throw 'dockur shared drive Z: is unavailable'
}

function Get-Capabilities {
    $caps = @{}
    $caps.powershell = $PSVersionTable.PSVersion.ToString()
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $caps.admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $caps.vcruntime140 = Test-Path "$env:WINDIR\System32\vcruntime140.dll"

    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ProbeWin32 {
    [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr CreateJobObject(IntPtr attrs, string name);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll", CharSet=CharSet.Ansi)] public static extern IntPtr GetModuleHandle(string name);
    [DllImport("kernel32.dll", CharSet=CharSet.Ansi)] public static extern IntPtr GetProcAddress(IntPtr module, string name);
}
'@
    $job = [ProbeWin32]::CreateJobObject([IntPtr]::Zero, $null)
    $caps.job_objects = ($job -ne [IntPtr]::Zero)
    if ($job -ne [IntPtr]::Zero) { [void][ProbeWin32]::CloseHandle($job) }
    $kernel = [ProbeWin32]::GetModuleHandle('kernel32.dll')
    $caps.conpty = ([ProbeWin32]::GetProcAddress($kernel, 'CreatePseudoConsole') -ne [IntPtr]::Zero)
    $caps.webview2 = [bool](Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft\EdgeWebView\Application" -ErrorAction SilentlyContinue | Where-Object { $_.PSIsContainer })
    # Display-only and software adapters cannot accelerate rendering. Report
    # the adapter names so a reader can check the classification.
    $adapters = @((Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }))
    $software = 'Basic Display|Basic Render|VirtIO GPU|Standard VGA|QXL|Bochs|ramfb'
    $caps.gpu_rendering = @{
        accelerated = [bool]($adapters | Where-Object { $_ -and $_ -notmatch $software })
        adapters = $adapters
    }
    return $caps
}

function Invoke-Installer([string]$Path, [string[]]$Arguments, [int]$TimeoutSeconds) {
    # CreateProcess, not ShellExecute: a shell launch of an executable from
    # the \\host.lan share shows the Internet-zone "Open File - Security
    # Warning" dialog on a desktop edition and waits forever for a click.
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $Path
    $info.Arguments = ($Arguments -join ' ')
    $info.UseShellExecute = $false
    $process = [Diagnostics.Process]::Start($info)
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        try { $process.Kill() } catch { }
        throw "$([IO.Path]::GetFileName($Path)) did not exit within $TimeoutSeconds s"
    }
    return $process.ExitCode
}

function Invoke-Replay {
    $nextest = 'Z:\cargo-nextest.exe'
    $archive = 'Z:\tests.tar.zst'
    $workspace = 'Z:\workspace'
    if (-not (Test-Path $nextest)) { throw 'native cargo-nextest.exe is missing from Z:' }
    if (-not (Test-Path $archive)) { throw 'nextest archive is missing from Z:' }
    if (-not (Test-Path "$workspace\Cargo.toml")) { throw 'workspace remap manifest is missing from Z:' }
    $env:NEXTEST_EXPERIMENTAL_LIBTEST_JSON = '1'
    # Windows PowerShell 5.1 promotes a native program's stderr to a
    # terminating error under Stop, even when the program only prints progress.
    # Scope Continue to this call and judge the native exit code explicitly.
    $savedErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $lines = @(& $nextest nextest run --archive-file $archive --workspace-remap $workspace -E 'package(soldr-core)' --message-format libtest-json --message-format-version 0.1 2>&1 | ForEach-Object { $_.ToString() })
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedErrorActionPreference
    }
    $lines | Set-Content -Path "Z:\${prefix}nextest.log" -Encoding UTF8
    $passed = 0
    $failed = 0
    foreach ($line in $lines) {
        try { $event = $line | ConvertFrom-Json -ErrorAction Stop } catch { continue }
        if ($event.type -eq 'test' -and $event.event -eq 'ok') { $passed++ }
        if ($event.type -eq 'test' -and $event.event -eq 'failed') { $failed++ }
    }
    return @{ status = 'finished'; run = $passed + $failed; passed = $passed; failed = $failed; exit_code = $exitCode }
}

try {
    Wait-SharedDrive
    # Host timestamps this before any capability probe or Nextest replay.
    Write-Marker 'guest-shell-ready.txt'
    $result.capabilities = Get-Capabilities
    $mingitRoot = Join-Path $probeRoot 'mingit'

    if ($Mode -eq 'cold') {
        $runtime = 'Z:\vc_redist.x64.exe'
        if (-not (Test-Path $runtime)) { throw 'verified VC++ Redistributable is missing from Z:' }
        $signature = Get-AuthenticodeSignature -FilePath $runtime
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
            throw "VC++ Redistributable Authenticode signature is not Microsoft-valid: $($signature.Status)"
        }
        New-Item -ItemType Directory -Force -Path $probeRoot | Out-Null
        $localRuntime = Join-Path $probeRoot 'vc_redist.x64.exe'
        Copy-Item -LiteralPath $runtime -Destination $localRuntime -Force
        Write-Marker 'runtime-install-start.txt'
        $exit = Invoke-Installer $localRuntime @('/install', '/quiet', '/norestart', '/log', 'Z:\vc-redist-install.log') 900
        $result.capabilities.vcruntime_install_exit = $exit
        $result.capabilities.vcruntime140_after = Test-Path "$env:WINDIR\System32\vcruntime140.dll"
        if ($exit -notin @(0, 3010) -or -not $result.capabilities.vcruntime140_after) {
            throw "VC++ Redistributable install did not provide vcruntime140.dll (exit $exit)"
        }
        Write-Marker 'runtime-ready.txt'

        $mingitArchive = 'Z:\mingit.zip'
        if (-not (Test-Path $mingitArchive)) { throw 'verified MinGit archive is missing from Z:' }
        Write-Marker 'git-install-start.txt'
        Expand-Archive -LiteralPath $mingitArchive -DestinationPath $mingitRoot -Force
    }

    $git = Join-Path $mingitRoot 'cmd\git.exe'
    if (-not (Test-Path $git)) { throw 'MinGit archive did not contain cmd\git.exe' }
    $env:PATH = "$mingitRoot\cmd;$env:PATH"
    $result.capabilities.git = (& $git --version | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0) { throw 'MinGit failed its native version check' }
    Write-Marker 'tools-ready.txt'

    $result.nextest = Invoke-Replay

    if ($Mode -eq 'cold') {
        # The restore job boots this same disk. dockur's FirstLogonCommands
        # never run again, so an at-logon task is the only thing that can
        # announce a usable shell on the cached image.
        $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\OEM\probe.ps1 -Mode warm'
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
        $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Hours 2)
        Register-ScheduledTask -TaskName 'SoldrProbeWarm' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
        $result.capabilities.warm_task = $true
    }
} catch {
    $result.error = $_.Exception.Message
} finally {
    if (Test-Path 'Z:\') {
        $result | ConvertTo-Json -Depth 6 | Set-Content -Path "Z:\${prefix}guest-result.json" -Encoding UTF8
    }
}
