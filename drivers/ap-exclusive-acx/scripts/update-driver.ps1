# Run as Administrator to hot-update ap_exclusive_audio.sys (NO REBOOT NEEDED when idle).
# Works both inside a source checkout and inside an unpacked release zip.
# Enforces SHA-256 verification, admin-only staging, ephemeral signing, exit code checking,
# and post-update DEVPKEY_Device_DriverVersion verification (Review #8 D2, D3, D6, D8).
param(
    [switch]$ForceResign
)
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

$devs = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" })
if ($devs.Count -eq 0) {
    throw "ROOT\ApExclusiveAudio is not installed on this machine. Run install-driver.ps1 first."
}

# Remove older staged OEM INF entries for ap_exclusive_audio.inf so same-day rebuilds always bind
# the fresh package (Review #8 D8).
$oldInfs = pnputil /enum-drivers | Select-String -Pattern "ap_exclusive_audio\.inf" -Context 3,0
foreach ($m in $oldInfs) {
    $block = ($m.Context.PreContext -join "`n")
    if ($block -match "(oem\d+\.inf)") {
        & pnputil.exe /delete-driver $Matches[1] /uninstall /force | Out-Null
    }
}

$installScript = Join-Path $PSScriptRoot "install-driver.ps1"
if (-not (Test-Path -LiteralPath $installScript)) {
    throw "install-driver.ps1 not found next to update-driver.ps1"
}

$argsMap = @{ NoEnableTestSigning = $true }
if ($ForceResign) { $argsMap.ForceResign = $true }
& $installScript @argsMap
exit $LASTEXITCODE
