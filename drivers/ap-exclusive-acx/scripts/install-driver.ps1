# Run as Administrator to install Audiophile Player's Exclusive ACX Virtual Render Driver
# (ROOT\ApExclusiveAudio -> "Line (Audiophile Player)" in Tidal Desktop Exclusive Mode).
# Automatically cleans up any earlier ROOT\ApTidalTest prototype device first.
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

$root = Split-Path -Parent $PSScriptRoot
$out  = Join-Path $root "out"
$devgen = @(
    "$root\.wdk\kit\tools\10.0.26100.0\x64\devgen.exe",
    "C:\Program Files (x86)\Windows Kits\10\Tools\10.0.26100.0\x64\devgen.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $devgen) { throw "devgen.exe not found" }

if (-not (Test-Path "$out\ap_exclusive_audio.sys") -or -not (Test-Path "$out\ApExclusiveTestCert.cer")) {
    throw "Build artifacts not found in $out. Run build-and-sign.ps1 first."
}

$sso = (Get-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control" -Name "SystemStartOptions").SystemStartOptions
if ($sso -notmatch "TESTSIGNING") {
    Write-Host "Enabling Windows Test Mode: bcdedit /set testsigning on" -ForegroundColor Cyan
    bcdedit /set testsigning on
    if ($LASTEXITCODE -ne 0) { throw "bcdedit failed (ensure Secure Boot is disabled)" }
    Write-Host "`nPlease REBOOT WINDOWS once, then re-run this script as Administrator." -ForegroundColor Green
    exit 0
}

# Clean up any leftover prototype ROOT\ApTidalTest device so Tidal only sees "Audiophile Player"
$oldDevs = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApTidalTest" }
foreach ($d in $oldDevs) {
    Write-Host "Removing previous prototype device $($d.InstanceId)..." -ForegroundColor Yellow
    pnputil /remove-device $d.InstanceId | Out-Null
}
$oldInfs = pnputil /enum-drivers | Select-String -Pattern "ap_tidal_test\.inf" -Context 3,0
foreach ($m in $oldInfs) {
    $block = ($m.Context.PreContext -join "`n")
    if ($block -match "(oem\d+\.inf)") {
        pnputil /delete-driver $Matches[1] /uninstall /force | Out-Null
    }
}

Write-Host "=== 1. Trusting local test signing certificate ===" -ForegroundColor Cyan
Import-Certificate -FilePath "$out\ApExclusiveTestCert.cer" -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
Import-Certificate -FilePath "$out\ApExclusiveTestCert.cer" -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null

Write-Host "`n=== 2. Installing ap_exclusive_audio.inf into Driver Store ===" -ForegroundColor Cyan
pnputil /add-driver "$out\ap_exclusive_audio.inf" /install

Write-Host "`n=== 3. Creating ROOT\ApExclusiveAudio virtual audio endpoint ===" -ForegroundColor Cyan
$existing = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" }
if ($existing) {
    Write-Host "ROOT\ApExclusiveAudio already exists ($($existing.InstanceId)); restarting..." -ForegroundColor Yellow
    pnputil /restart-device $existing.InstanceId
} else {
    & $devgen /add /bus ROOT /hardwareid "ROOT\ApExclusiveAudio"
    if ($LASTEXITCODE -ne 0) { throw "devgen /add failed ($LASTEXITCODE)" }
    Start-Sleep -Seconds 2
    pnputil /scan-devices
}

Write-Host "`n=== 4. Verifying device ===" -ForegroundColor Cyan
Get-PnpDevice -Class MEDIA | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" } | Format-Table -AutoSize Status, Class, FriendlyName, InstanceId

Write-Host "`nInstalled! In Tidal Desktop, select 'Line (Audiophile Player)' with Exclusive Mode ON (Force Volume OFF)." -ForegroundColor Green
