# Run as Administrator to cleanly uninstall ROOT\ApExclusiveAudio, its Driver Store package,
# and the local per-machine test-signing certificate.
# Pass -DisableTestSigning to also turn off Windows Test Mode (reboot required afterward).
param([switch]$DisableTestSigning)
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

foreach ($hwid in @("ROOT\ApExclusiveAudio", "ROOT\ApTidalTest")) {
    $devs = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains $hwid }
    foreach ($d in $devs) {
        Write-Host "Removing $($d.InstanceId)..."
        pnputil /remove-device $d.InstanceId
    }
}

foreach ($pat in @("ap_exclusive_audio\.inf", "ap_tidal_test\.inf")) {
    $oemLines = pnputil /enum-drivers | Select-String -Pattern $pat -Context 3,0
    foreach ($m in $oemLines) {
        $block = ($m.Context.PreContext -join "`n")
        if ($block -match "(oem\d+\.inf)") {
            pnputil /delete-driver $Matches[1] /uninstall /force
        }
    }
}

foreach ($store in @("Cert:\LocalMachine\Root", "Cert:\LocalMachine\TrustedPublisher", "Cert:\LocalMachine\My", "Cert:\CurrentUser\My")) {
    Get-ChildItem $store -ErrorAction SilentlyContinue | Where-Object {
        $_.Subject -eq "CN=Audiophile Player Local Test Signing" -or $_.Subject -eq "CN=Audiophile Player Tidal Test"
    } | Remove-Item -Force -ErrorAction SilentlyContinue
}

if ($DisableTestSigning) {
    Write-Host "Disabling Windows Test Mode: bcdedit /set testsigning off" -ForegroundColor Cyan
    bcdedit /set testsigning off
    Write-Host "Reboot Windows once to complete leaving Test Mode." -ForegroundColor Yellow
}

Write-Host "Cleanly uninstalled!" -ForegroundColor Green
