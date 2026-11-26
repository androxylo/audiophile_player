# Run as Administrator to hot-update ap_exclusive_audio.sys (NO REBOOT NEEDED).
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

$root = Split-Path -Parent $PSScriptRoot
$out  = Join-Path $root "out"

pnputil /add-driver "$out\ap_exclusive_audio.inf" /install
$devs = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" }
foreach ($d in $devs) {
    pnputil /restart-device $d.InstanceId
}
Start-Sleep -Seconds 1
Get-PnpDevice -Class MEDIA | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" } | Format-Table -AutoSize Status, FriendlyName, InstanceId
