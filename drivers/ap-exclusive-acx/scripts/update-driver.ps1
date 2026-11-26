# Run as Administrator to hot-update ap_exclusive_audio.sys (NO REBOOT NEEDED).
# Works both inside a source checkout and inside an unpacked release zip.
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

if ((Test-Path (Join-Path $PSScriptRoot "ap_exclusive_audio.sys")) -and (Test-Path (Join-Path $PSScriptRoot "ap_exclusive_audio.inf"))) {
    $pkgDir = $PSScriptRoot
} elseif (Test-Path (Join-Path (Split-Path -Parent $PSScriptRoot) "out\ap_exclusive_audio.sys")) {
    $pkgDir = Join-Path (Split-Path -Parent $PSScriptRoot) "out"
} else {
    throw "Driver files not found next to this script or in ..\out."
}

# Re-sign locally if artifacts are unsigned or not yet trusted
$sysPath = Join-Path $pkgDir "ap_exclusive_audio.sys"
$catPath = Join-Path $pkgDir "ap_exclusive_audio.cat"
if ((Get-AuthenticodeSignature $sysPath).Status -ne "Valid" -or (Get-AuthenticodeSignature $catPath).Status -ne "Valid") {
    & (Join-Path $PSScriptRoot "install-driver.ps1")
    exit $LASTEXITCODE
}

pnputil /add-driver (Join-Path $pkgDir "ap_exclusive_audio.inf") /install
$devs = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" }
foreach ($d in $devs) {
    pnputil /restart-device $d.InstanceId
}
Start-Sleep -Seconds 1
Get-PnpDevice -Class MEDIA | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" } | Format-Table -AutoSize Status, FriendlyName, InstanceId
