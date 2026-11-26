# Run as Administrator to cleanly uninstall ROOT\ApExclusiveAudio, its Driver Store package,
# admin staging folder, and all local test-signing certificates (+ private keys).
#
# Test Mode behavior (Review #8 D7):
#   - If install-driver.ps1 enabled Windows Test Mode (recorded in HKLM:\SOFTWARE\AudiophilePlayer\Driver),
#     uninstall automatically turns Test Mode back OFF unless you pass -KeepTestSigning.
#   - Pass -DisableTestSigning to explicitly turn Test Mode OFF even if it was enabled manually.
#   - Pass -StopPlayer to automatically stop a running audiophile-player.exe before uninstalling.
param(
    [switch]$DisableTestSigning,
    [switch]$KeepTestSigning,
    [switch]$StopPlayer
)
$ErrorActionPreference = "Stop"

$regStateKey = "HKLM:\SOFTWARE\AudiophilePlayer\Driver"
$stageDir    = Join-Path $env:ProgramData "AudiophilePlayer\driver"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

$failures = New-Object System.Collections.Generic.List[string]
$rebootRequired = $false

function Invoke-UninstallNative {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Arguments,
        [int[]]$OkExitCodes = @(0, 259, 3010)
    )
    & $Exe @Arguments
    $rc = $LASTEXITCODE
    if ($rc -eq 3010) {
        $script:rebootRequired = $true
    } elseif ($OkExitCodes -notcontains $rc) {
        $script:failures.Add("'$Exe $($Arguments -join ' ')' exited with code $rc")
    }
    return $rc
}

# Check for running audiophile-player.exe holding \\.\ApExclusiveAudio
$runningAp = @(Get-Process -Name "audiophile-player" -ErrorAction SilentlyContinue)
if ($runningAp.Count -gt 0) {
    if ($StopPlayer) {
        Write-Host "Stopping running audiophile-player.exe..." -ForegroundColor Cyan
        $runningAp | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    } else {
        Write-Warning "audiophile-player.exe is currently running (and Tidal Desktop may hold the audio pin). Close them first (or pass -StopPlayer) to avoid a reboot."
    }
}

foreach ($hwid in @("ROOT\ApExclusiveAudio", "ROOT\ApTidalTest")) {
    $devs = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains $hwid })
    foreach ($d in $devs) {
        Write-Host "Removing $($d.InstanceId)..."
        Invoke-UninstallNative -Exe "pnputil.exe" -Arguments @("/remove-device", $d.InstanceId) | Out-Null
    }
}

foreach ($pat in @("ap_exclusive_audio\.inf", "ap_tidal_test\.inf")) {
    $oemLines = pnputil /enum-drivers | Select-String -Pattern $pat -Context 3,0
    foreach ($m in $oemLines) {
        $block = ($m.Context.PreContext -join "`n")
        if ($block -match "(oem\d+\.inf)") {
            Write-Host "Deleting Driver Store package $($Matches[1])..."
            Invoke-UninstallNative -Exe "pnputil.exe" -Arguments @("/delete-driver", $Matches[1], "/uninstall", "/force") | Out-Null
        }
    }
}

# Clean up admin-only staging directory
if (Test-Path -LiteralPath $stageDir) {
    try {
        Remove-Item -Recurse -Force -LiteralPath $stageDir
    } catch {
        $failures.Add("Could not remove staging directory '$stageDir': $($_.Exception.Message)")
    }
}

# Read install-time state marker (thumbprints + whether THIS installer enabled Test Mode)
$recordedThumbs = @()
$weEnabledTestSigning = $false
if (Test-Path -LiteralPath $regStateKey) {
    $props = Get-ItemProperty -Path $regStateKey -ErrorAction SilentlyContinue
    if ($props) {
        if ($props.CertThumbprints) { $recordedThumbs = @($props.CertThumbprints) }
        if ($props.EnabledTestSigning -eq 1) { $weEnabledTestSigning = $true }
    }
}

# Review #8 D3 & D7: Remove certificates by recorded thumbprint + subject fallback, WITH -DeleteKey on My stores!
$removedCertCount = 0
foreach ($store in @("Cert:\LocalMachine\Root", "Cert:\LocalMachine\TrustedPublisher", "Cert:\LocalMachine\My", "Cert:\CurrentUser\My")) {
    $isMy = $store.EndsWith("\My")
    $matches = @(Get-ChildItem $store -ErrorAction SilentlyContinue | Where-Object {
        ($recordedThumbs -contains $_.Thumbprint) -or
        ($_.Subject -eq "CN=Audiophile Player Local Test Signing") -or
        ($_.Subject -eq "CN=Audiophile Player Tidal Test")
    })
    foreach ($c in $matches) {
        try {
            if ($isMy) {
                Remove-Item -LiteralPath $c.PSPath -DeleteKey -Force
            } else {
                Remove-Item -LiteralPath $c.PSPath -Force
            }
            $removedCertCount++
        } catch {
            $failures.Add("Failed removing certificate $($c.Thumbprint) from ${store}: $($_.Exception.Message)")
        }
    }
}
Write-Host "Removed $removedCertCount local test-signing certificate entry/entries (with private keys purged)." -ForegroundColor Cyan

# Conditional Test Mode revert (Review #8 D7): disable automatically if WE enabled it (unless -KeepTestSigning),
# or if the user explicitly passed -DisableTestSigning.
$shouldDisableTestSigning = $DisableTestSigning -or ($weEnabledTestSigning -and -not $KeepTestSigning)
if ($shouldDisableTestSigning) {
    Write-Host "Disabling Windows Test Mode: bcdedit /set testsigning off" -ForegroundColor Cyan
    $rcBcd = Invoke-UninstallNative -Exe "bcdedit.exe" -Arguments @("/set", "testsigning", "off") -OkExitCodes @(0)
    if ($rcBcd -eq 0) {
        $rebootRequired = $true
        Write-Host "Test Mode disabled! Reboot Windows once to leave Test Mode." -ForegroundColor Yellow
        Write-Host "Reminder: After rebooting, you may re-enable UEFI Secure Boot in BIOS/UEFI (if BitLocker is enabled, suspend BitLocker or keep your recovery key handy first)." -ForegroundColor Yellow
    }
}

# Remove install state registry key if no failures occurred
if ($failures.Count -eq 0 -and (Test-Path -LiteralPath $regStateKey)) {
    Remove-Item -Path $regStateKey -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failures.Count -gt 0) {
    Write-Host "`nUninstall INCOMPLETE:" -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  - $f" -ForegroundColor Red }
    exit 1
}

if ($rebootRequired) {
    Write-Host "`nUninstalled — REBOOT REQUIRED (exit 3010) to finish removing the driver / leaving Test Mode." -ForegroundColor Yellow
    exit 3010
}

Write-Host "`nCleanly uninstalled!" -ForegroundColor Green
exit 0
