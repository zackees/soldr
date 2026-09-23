# One-shot, non-gating guest probe. Results and the native nextest log return
# through dockur's /shared mount (Z:); no guest disk or activation state persists.
$ErrorActionPreference = 'Stop'
$result = @{
    capabilities = @{}
    nextest = @{ status = 'not-run'; run = 0; passed = 0; failed = 0; exit_code = $null }
}

try {
    $share = 'Z:\'
    if (-not (Test-Path $share)) { throw 'dockur shared drive Z: is unavailable' }
    # Host timestamps this before any capability probe or Nextest replay.
    'ready' | Set-Content -Path 'Z:\guest-shell-ready.txt' -Encoding ASCII
    $result.capabilities.powershell = $PSVersionTable.PSVersion.ToString()
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $result.capabilities.admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $result.capabilities.vcruntime140 = Test-Path "$env:WINDIR\System32\vcruntime140.dll"

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
    $result.capabilities.job_objects = ($job -ne [IntPtr]::Zero)
    if ($job -ne [IntPtr]::Zero) { [void][ProbeWin32]::CloseHandle($job) }
    $kernel = [ProbeWin32]::GetModuleHandle('kernel32.dll')
    $result.capabilities.conpty = ([ProbeWin32]::GetProcAddress($kernel, 'CreatePseudoConsole') -ne [IntPtr]::Zero)
    $result.capabilities.webview2 = [bool](Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft\EdgeWebView\Application" -ErrorAction SilentlyContinue | Where-Object { $_.PSIsContainer })
    $result.capabilities.gpu_rendering = @{
        accelerated = $false
        adapters = @((Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | ForEach-Object { $_.Name }))
    }

    $runtime = 'Z:\vc_redist.x64.exe'
    if (-not (Test-Path $runtime)) { throw 'verified VC++ Redistributable is missing from Z:' }
    $signature = Get-AuthenticodeSignature -FilePath $runtime
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
        throw "VC++ Redistributable Authenticode signature is not Microsoft-valid: $($signature.Status)"
    }
    'ready' | Set-Content -Path 'Z:\runtime-install-start.txt' -Encoding ASCII
    $install = Start-Process -FilePath $runtime -ArgumentList @('/install', '/quiet', '/norestart', '/log', 'Z:\vc-redist-install.log') -Wait -PassThru
    $result.capabilities.vcruntime_install_exit = $install.ExitCode
    $result.capabilities.vcruntime140_after = Test-Path "$env:WINDIR\System32\vcruntime140.dll"
    if ($install.ExitCode -notin @(0, 3010) -or -not $result.capabilities.vcruntime140_after) {
        throw "VC++ Redistributable install did not provide vcruntime140.dll (exit $($install.ExitCode))"
    }
    'ready' | Set-Content -Path 'Z:\runtime-ready.txt' -Encoding ASCII

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
    $lines | Set-Content -Path 'Z:\nextest.log' -Encoding UTF8
    $passed = 0
    $failed = 0
    foreach ($line in $lines) {
        try { $event = $line | ConvertFrom-Json -ErrorAction Stop } catch { continue }
        if ($event.type -eq 'test' -and $event.event -eq 'ok') { $passed++ }
        if ($event.type -eq 'test' -and $event.event -eq 'failed') { $failed++ }
    }
    $result.nextest = @{ status = 'finished'; run = $passed + $failed; passed = $passed; failed = $failed; exit_code = $exitCode }
} catch {
    $result.error = $_.Exception.Message
} finally {
    if (Test-Path 'Z:\') {
        $result | ConvertTo-Json -Depth 6 | Set-Content -Path 'Z:\guest-result.json' -Encoding UTF8
    }
}
