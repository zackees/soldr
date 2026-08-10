#Requires -Version 5.1

[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [ValidateSet("Install", "Uninstall", "Start", "Stop", "Status")]
    [string] $Action = "Install",

    [string] $ServiceName = "running-process-broker-v1",

    [string] $DisplayName = "running-process broker v1",

    [string] $BinaryPath = "$env:ProgramFiles\running-process\running-process-broker-v1.exe",

    [System.Management.Automation.PSCredential] $Credential,

    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    $admin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) {
        throw "Run this script from an elevated PowerShell session."
    }
}

function Get-BrokerService {
    Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
}

function Install-BrokerService {
    Assert-Administrator

    if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
        throw "Broker binary not found: $BinaryPath"
    }

    $existing = Get-BrokerService
    if ($null -ne $existing) {
        if (-not $Force) {
            throw "Service '$ServiceName' already exists. Re-run with -Force to replace it."
        }
        Uninstall-BrokerService
    }

    $quotedBinary = '"' + $BinaryPath + '" --service'
    $parameters = @{
        Name = $ServiceName
        DisplayName = $DisplayName
        BinaryPathName = $quotedBinary
        StartupType = "Manual"
        Description = "Optional Windows service wrapper for running-process-broker-v1."
    }
    if ($PSBoundParameters.ContainsKey("Credential")) {
        $parameters.Credential = $Credential
    }

    if ($PSCmdlet.ShouldProcess($ServiceName, "install Windows service")) {
        New-Service @parameters | Out-Null
        & sc.exe failure $ServiceName reset= 60 actions= restart/2000/restart/5000/""/0 | Out-Null
        & sc.exe failureflag $ServiceName 1 | Out-Null
    }
}

function Uninstall-BrokerService {
    Assert-Administrator

    $service = Get-BrokerService
    if ($null -eq $service) {
        Write-Host "Service '$ServiceName' is not installed."
        return
    }

    if ($service.Status -ne "Stopped") {
        if ($PSCmdlet.ShouldProcess($ServiceName, "stop Windows service")) {
            Stop-Service -Name $ServiceName -Force
            $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(30))
        }
    }

    if ($PSCmdlet.ShouldProcess($ServiceName, "delete Windows service")) {
        & sc.exe delete $ServiceName | Out-Null
    }
}

function Start-BrokerService {
    Assert-Administrator
    if ($PSCmdlet.ShouldProcess($ServiceName, "start Windows service")) {
        Start-Service -Name $ServiceName
    }
}

function Stop-BrokerService {
    Assert-Administrator
    if ($PSCmdlet.ShouldProcess($ServiceName, "stop Windows service")) {
        Stop-Service -Name $ServiceName
    }
}

function Show-BrokerServiceStatus {
    $service = Get-BrokerService
    if ($null -eq $service) {
        Write-Host "Service '$ServiceName' is not installed."
        return
    }
    $service | Format-List Name, DisplayName, Status, StartType, ServiceType
}

switch ($Action) {
    "Install" { Install-BrokerService }
    "Uninstall" { Uninstall-BrokerService }
    "Start" { Start-BrokerService }
    "Stop" { Stop-BrokerService }
    "Status" { Show-BrokerServiceStatus }
}
