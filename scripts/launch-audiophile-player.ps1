# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

<#
.SYNOPSIS
    Launches Audiophile Player (if not already running) and opens its local HTTP control UI.

.DESCRIPTION
    - Queries the running player via `audiophile-player status --json` (backed by run.json and
      authenticated /api/v1/status) so the active Web Control UI opens immediately without
      disturbing playback or launching a duplicate instance.
    - Otherwise, starts audiophile-player.exe in a clean console window (close or press Ctrl+C
      to stop and release the DAC), polls `status --json` every 200 ms for up to 6 seconds, and
      opens the UI in the default browser.
#>

$ErrorActionPreference = 'SilentlyContinue'

$repoRoot    = Split-Path -Parent $PSScriptRoot
$receiptPath = Join-Path $repoRoot 'install-receipt.yaml'
$receiptExe  = $null
if (Test-Path -LiteralPath $receiptPath) {
    $receiptRaw = Get-Content -LiteralPath $receiptPath -Raw
    if ($receiptRaw -match "(?m)^exe_path:\s*'?([^'`r`n]+)'?") {
        $cand = $Matches[1].Trim()
        if ($cand -and (Test-Path -LiteralPath $cand)) {
            $receiptExe = $cand
        }
    }
}
$releaseExe = Join-Path $repoRoot 'audiophile-player.exe'
$cargoExe   = Join-Path $repoRoot 'target\release\audiophile-player.exe'
$exePath    = if ($receiptExe) { $receiptExe } elseif (Test-Path -LiteralPath $releaseExe) { $releaseExe } else { $cargoExe }

function Show-PlayerWarning([string]$Message) {
    Add-Type -AssemblyName PresentationFramework
    [System.Windows.MessageBox]::Show(
        $Message,
        "Audiophile Player",
        [System.Windows.MessageBoxButton]::OK,
        [System.Windows.MessageBoxImage]::Warning
    ) | Out-Null
}

function Get-PlayerUiUrl([string]$Exe) {
    try {
        $raw = & $Exe status --json 2>$null
        if ($LASTEXITCODE -ne 0 -or -not $raw) {
            return $null
        }
        $info = ($raw -join "`n") | ConvertFrom-Json
        if ($info -and $info.url -and ($info.url -match '^http://127\.0\.0\.1:\d+$')) {
            return $info.url.TrimEnd('/')
        }
    } catch {
        return $null
    }
    return $null
}

if (-not (Test-Path -LiteralPath $exePath)) {
    Show-PlayerWarning "audiophile-player.exe was not found at:`n$exePath`n`nBuild it first with:`ncargo build --release --bin audiophile-player"
    exit 1
}

# 1. If already running, NEVER launch a second instance (which would fail to lock the DAC).
$existingUrl = Get-PlayerUiUrl $exePath
if ($existingUrl) {
    Start-Process $existingUrl
    exit 0
}
$running = @(Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) {
    Show-PlayerWarning "audiophile-player.exe is already running, but 'status --json' did not report a ready Web Control UI.`n`nCheck the player console window instead of starting a duplicate instance."
    exit 1
}

# 2. Launch audiophile-player.exe in a normal console window so status is visible and Ctrl+C/close stops cleanly.
$proc = Start-Process -FilePath $exePath -WorkingDirectory $repoRoot -PassThru

# 3. Poll `audiophile-player status --json` every 200 ms for up to 6 seconds.
$deadline = (Get-Date).AddSeconds(6)
$uiUrl = $null
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 200
    $uiUrl = Get-PlayerUiUrl $exePath
    if ($uiUrl -or $proc.HasExited) {
        break
    }
}

if ($uiUrl) {
    Start-Process $uiUrl
} else {
    Show-PlayerWarning "audiophile-player.exe was started, but its localhost Web Control UI did not become ready within 6 seconds.`n`nCheck the console window for DAC or configuration errors."
    exit 1
}
