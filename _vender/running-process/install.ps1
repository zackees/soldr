[CmdletBinding()]
param(
    [switch]$Global,
    [switch]$User,
    [string]$BinDir,
    [string]$Version = $(if ($env:RP_INSTALL_VERSION) { $env:RP_INSTALL_VERSION } else { "latest" })
)

$ErrorActionPreference = "Stop"

function Write-Log {
    param([string]$Message)
    Write-Host "[running-process-install] $Message"
}

function Get-InstallMode {
    if ($Global.IsPresent) { return "global" }
    if ($User.IsPresent) { return "user" }
    if ($env:RP_INSTALL_MODE) { return $env:RP_INSTALL_MODE.ToLowerInvariant() }
    return "user"
}

function Get-RepoSlug {
    if ($env:RP_INSTALL_REPO) { return $env:RP_INSTALL_REPO }
    return "zackees/running-process"
}

function Resolve-LatestTag {
    $repo = Get-RepoSlug
    $apiUrl = "https://api.github.com/repos/$repo/releases/latest"
    $resp = Invoke-RestMethod -Uri $apiUrl -Headers @{ "User-Agent" = "running-process-install" }
    if (-not $resp.tag_name) {
        throw "Could not resolve latest release tag from $apiUrl"
    }
    return $resp.tag_name
}

function Get-NormalizedVersion {
    param([string]$Tag)
    if ($Tag.StartsWith("v")) { return $Tag.Substring(1) }
    return $Tag
}

function Get-TargetTriple {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        ([System.Runtime.InteropServices.Architecture]::X64) { $cpu = "x86_64" }
        ([System.Runtime.InteropServices.Architecture]::Arm64) { $cpu = "aarch64" }
        default { throw "Unsupported architecture: $arch" }
    }
    return "$cpu-pc-windows-msvc"
}

function Get-AssetUrl {
    param([string]$Tag, [string]$Asset)
    $baseUrl = if ($env:RP_INSTALL_BASE_URL) {
        $env:RP_INSTALL_BASE_URL.TrimEnd("/")
    } else {
        $repo = Get-RepoSlug
        "https://github.com/$repo/releases"
    }
    return "$baseUrl/download/$Tag/$Asset"
}

function Add-UserPath {
    param([string]$InstallDir)
    if (($env:PATH -split ';') -contains $InstallDir) {
        return
    }
    if ($env:RP_NO_MODIFY_PATH -eq "1") {
        return
    }
    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    $parts = @()
    if ($current) {
        $parts = $current -split ';' | Where-Object { $_ }
    }
    if ($parts -contains $InstallDir) {
        return
    }
    $newPath = if ($current) { "$current;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Log "Added $InstallDir to the user PATH."
}

$installMode = Get-InstallMode
$installDir = if ($BinDir) {
    $BinDir
} elseif ($env:RP_INSTALL_DIR) {
    $env:RP_INSTALL_DIR
} elseif ($installMode -eq "global") {
    Join-Path ${env:ProgramFiles} "running-process\bin"
} else {
    Join-Path $HOME ".local\bin"
}

if ($Version -eq "latest") {
    $tag = Resolve-LatestTag
} else {
    $tag = $Version
}
$ver = Get-NormalizedVersion -Tag $tag

$target = Get-TargetTriple
$asset = "running-process-$ver-$target.zip"
$url = Get-AssetUrl -Tag $tag -Asset $asset

$tmpRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("running-process-install-" + [guid]::NewGuid().ToString("N"))
$archivePath = Join-Path $tmpRoot $asset
$extractDir = Join-Path $tmpRoot "extract"

try {
    New-Item -ItemType Directory -Force -Path $tmpRoot | Out-Null
    Write-Log "Downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $archivePath
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force

    $archiveRoot = Join-Path $extractDir "running-process-$ver-$target"
    if (-not (Test-Path -LiteralPath $archiveRoot)) {
        throw "Archive layout was not recognized."
    }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item -LiteralPath (Join-Path $archiveRoot "runpm.exe") -Destination (Join-Path $installDir "runpm.exe") -Force
    Copy-Item -LiteralPath (Join-Path $archiveRoot "running-process-daemon.exe") -Destination (Join-Path $installDir "running-process-daemon.exe") -Force

    if ($installMode -eq "user") {
        Add-UserPath -InstallDir $installDir
    }

    Write-Log "Installed runpm + running-process-daemon to $installDir"
    if (-not (($env:PATH -split ';') -contains $installDir)) {
        Write-Log "Open a new shell or add $installDir to PATH before running runpm."
    }
} finally {
    if (Test-Path -LiteralPath $tmpRoot) {
        Remove-Item -LiteralPath $tmpRoot -Force -Recurse
    }
}
