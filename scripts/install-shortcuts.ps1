# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

<#
.SYNOPSIS
    Installs Windows Desktop and Start Menu shortcuts for Audiophile Player.
#>

$ErrorActionPreference = 'Stop'

$repoRoot   = Split-Path -Parent $PSScriptRoot
$launcher   = Join-Path $repoRoot 'scripts\launch-audiophile-player.ps1'
$iconPath   = Join-Path $repoRoot 'assets\audiophile-player.ico'
$desktopDir = [Environment]::GetFolderPath('Desktop')
$startDir   = [Environment]::GetFolderPath('Programs')

if (-not (Test-Path -LiteralPath $launcher)) {
    throw "Launcher script missing: $launcher"
}
if (-not (Test-Path -LiteralPath $iconPath)) {
    throw "Icon file missing: $iconPath"
}

$wsh = New-Object -ComObject WScript.Shell
$psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$argsStr = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$launcher`""

foreach ($targetDir in @($desktopDir, $startDir)) {
    if (-not (Test-Path -LiteralPath $targetDir)) {
        continue
    }
    $lnkPath = Join-Path $targetDir 'Audiophile Player.lnk'
    $sc = $wsh.CreateShortcut($lnkPath)
    $sc.TargetPath       = $psExe
    $sc.Arguments        = $argsStr
    $sc.WorkingDirectory = $repoRoot
    $sc.IconLocation     = "$iconPath,0"
    $sc.Description      = 'Audiophile Player (WDM-KS WaveRT Zero-Mixer + Web Control UI)'
    $sc.WindowStyle      = 7  # Minimized/hidden launcher shim; player console itself opens normally
    $sc.Save()
    Write-Output "Created shortcut: $lnkPath"
}
