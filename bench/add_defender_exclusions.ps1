# bench/add_defender_exclusions.ps1
#
# Add Windows Defender real-time-scan exclusions for soldr's cache, bench
# scratch, and runtime self-relocation directories. Must be run as
# Administrator (Add-MpPreference requires elevation).
#
# Why this matters: zccache writes hundreds of MB of compiled artifacts
# (.rmeta, .rlib, .o, etc.) to its cache. Defender real-time scans every
# new file before the write completes. On a cold cache, this can add 10+
# minutes to the wall-clock of a soldr-wrapped Rust build on Windows.
# Excluding the soldr-owned directories from real-time scanning removes
# that penalty while leaving Defender active for everything else.
#
# Usage (Administrator PowerShell):
#     powershell -ExecutionPolicy Bypass -File bench/add_defender_exclusions.ps1
#
# Idempotent: re-running is safe. Paths already on the exclusion list are
# silently skipped.

#Requires -RunAsAdministrator

$ErrorActionPreference = "Stop"

$home_dir = [Environment]::GetFolderPath("UserProfile")
$paths = @(
    (Join-Path $home_dir ".soldr\cache"),
    (Join-Path $home_dir ".soldr\bench"),
    (Join-Path $home_dir ".soldr\runtime"),
    (Join-Path $home_dir ".soldr\state.redb")
)

Write-Host "Adding Windows Defender exclusions for soldr paths..."

$existing = @()
try {
    $existing = (Get-MpPreference).ExclusionPath
    if ($null -eq $existing) { $existing = @() }
} catch {
    Write-Warning "Could not read existing Defender exclusions: $_"
    Write-Warning "Will attempt to add anyway."
}

foreach ($p in $paths) {
    $already = $existing | Where-Object { $_ -ieq $p }
    if ($already) {
        Write-Host "  already excluded: $p"
        continue
    }
    try {
        Add-MpPreference -ExclusionPath $p
        Write-Host "  added:            $p"
    } catch {
        Write-Warning "  failed to add $p : $_"
    }
}

# Verify the resulting exclusion list contains every requested path.
$after = (Get-MpPreference).ExclusionPath
if ($null -eq $after) { $after = @() }
$missing = @()
foreach ($p in $paths) {
    if (-not ($after | Where-Object { $_ -ieq $p })) {
        $missing += $p
    }
}
if ($missing.Count -gt 0) {
    Write-Warning "The following paths are NOT on the exclusion list:"
    foreach ($m in $missing) {
        Write-Warning "  $m"
    }
    exit 1
}

Write-Host ""
Write-Host "All soldr paths are now excluded from Defender real-time scanning."
Write-Host "To list current Defender exclusions:"
Write-Host "    Get-MpPreference | Select-Object -ExpandProperty ExclusionPath"
Write-Host ""
Write-Host "To undo (Administrator):"
foreach ($p in $paths) {
    Write-Host "    Remove-MpPreference -ExclusionPath '$p'"
}
