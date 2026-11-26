# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

# Assembles a self-contained Windows x64 release archive under dist\audiophile-player-v<ver>-win-x64.zip.
#
# The release bundle contains:
#   - audiophile-player.exe + audiophile-player.example.toml
#   - README.md, LICENSE, THIRD-PARTY-LICENSES.txt, docs\, assets\, scripts\ (Desktop/Start Menu shortcuts)
#   - driver\ (opt-in pure-Rust ACX virtual render driver for Tidal Desktop Exclusive Mode):
#       ap_exclusive_audio.sys (UNSIGNED), ap_exclusive_audio.inf, ap_exclusive_audio.cat (UNSIGNED),
#       SHA256SUMS, NOTICE, README.md,
#       install-driver.ps1 (verifies SHA-256, signs ephemerally in admin staging, deletes key),
#       update-driver.ps1, uninstall-driver.ps1
#
# Safety invariants (Review #8 D2, D10, D11, D12, D13):
#   - Fails loudly if InfVerif fails or is missing on the unsigned driver build.
#   - Embeds SHA-256 hashes of unsigned .sys/.inf/.cat into driver\SHA256SUMS and driver\install-driver.ps1.
#   - Generates THIRD-PARTY-LICENSES.txt from cargo metadata for all linked third-party crates.
#   - Rewrites relative links inside staged release docs so every link works inside the extracted zip.
#   - Enforces BOTH an expanded forbidden-extension blocklist AND a strict manifest allowlist.
param(
    [switch]$SkipAppBuild,
    [switch]$SkipDriverBuild,
    [switch]$WithoutDriver,
    [switch]$FetchWdk
)
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

$rootToml = Get-Content -LiteralPath (Join-Path $repoRoot "Cargo.toml") -Raw
if ($rootToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
    throw "Could not determine workspace version from Cargo.toml"
}
$version = $Matches[1]

$distDir    = Join-Path $repoRoot "dist"
$bundleName = "audiophile-player-v$version-win-x64"
$stageDir   = Join-Path $distDir $bundleName
$zipPath    = Join-Path $distDir "$bundleName.zip"
$shaPath    = "$zipPath.sha256"
$driverOut  = Join-Path $distDir "driver-unsigned"

if (-not $SkipAppBuild) {
    Write-Host "=== 1. Building audiophile-player.exe (release) ===" -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        cargo build --release --bin audiophile-player
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release --bin audiophile-player failed" }
    } finally {
        Pop-Location
    }
}

$exeSrc = Join-Path $repoRoot "target\release\audiophile-player.exe"
if (-not (Test-Path -LiteralPath $exeSrc)) {
    throw "Release binary not found: $exeSrc"
}

if (-not $WithoutDriver) {
    if (-not $SkipDriverBuild) {
        Write-Host "`n=== 2. Building UNSIGNED ap_exclusive_audio.sys / .inf / .cat ===" -ForegroundColor Cyan
        if (Test-Path $driverOut) { Remove-Item -Recurse -Force $driverOut }
        $drvBuild = Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\build-and-sign.ps1"
        $drvArgs = @{ Unsigned = $true; OutDir = $driverOut }
        if ($FetchWdk) { $drvArgs.FetchWdk = $true }
        & $drvBuild @drvArgs
    }

    foreach ($f in @("ap_exclusive_audio.sys", "ap_exclusive_audio.inf", "ap_exclusive_audio.cat")) {
        $p = Join-Path $driverOut $f
        if (-not (Test-Path -LiteralPath $p)) {
            throw "Unsigned driver artifact missing: $p"
        }
    }
}

Write-Host "`n=== 3. Staging release directory $stageDir ===" -ForegroundColor Cyan
if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir }
$stageDirs = @(
    $stageDir,
    (Join-Path $stageDir "docs"),
    (Join-Path $stageDir "assets"),
    (Join-Path $stageDir "scripts")
)
if (-not $WithoutDriver) { $stageDirs += (Join-Path $stageDir "driver") }
New-Item -ItemType Directory -Force -Path $stageDirs | Out-Null

Copy-Item -Force $exeSrc (Join-Path $stageDir "audiophile-player.exe")
Copy-Item -Force (Join-Path $repoRoot "audiophile-player.example.toml") $stageDir
Copy-Item -Force (Join-Path $repoRoot "README.md") $stageDir
Copy-Item -Force (Join-Path $repoRoot "LICENSE") $stageDir

foreach ($doc in @("QUICK_START.md", "GETTING_STARTED.md", "SOUND_ENGINE.md", "ULTIMATE_MODE.md", "CLI.md", "FILTER_PREFERENCES.md", "WHATS_NEW.md")) {
    Copy-Item -Force (Join-Path $repoRoot "docs\$doc") (Join-Path $stageDir "docs\$doc")
}
Copy-Item -Force (Join-Path $repoRoot "assets\audiophile-player.ico") (Join-Path $stageDir "assets\audiophile-player.ico")
foreach ($sc in @("launch-audiophile-player.ps1", "install-shortcuts.ps1")) {
    Copy-Item -Force (Join-Path $repoRoot "scripts\$sc") (Join-Path $stageDir "scripts\$sc")
}

# Generate THIRD-PARTY-LICENSES.txt for all third-party crates linked into audiophile-player.exe (Review #8 D11)
Write-Host "Generating THIRD-PARTY-LICENSES.txt from cargo metadata..."
Push-Location $repoRoot
try {
    $metaJson = cargo metadata --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed" }
} finally {
    Pop-Location
}
$meta = $metaJson | ConvertFrom-Json
$wsMemberIds = [System.Collections.Generic.HashSet[string]]::new()
foreach ($id in $meta.workspace_members) { [void]$wsMemberIds.Add($id) }

$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("Audiophile Player v$version - Third-Party Rust Crate Licenses & Notices")
[void]$sb.AppendLine("========================================================================")
[void]$sb.AppendLine("")
[void]$sb.AppendLine("Audiophile Player itself is licensed under GPL-3.0-or-later (see LICENSE).")
[void]$sb.AppendLine("The following third-party Rust crates are linked into audiophile-player.exe:")
[void]$sb.AppendLine("")

$thirdParty = $meta.packages | Where-Object { -not $wsMemberIds.Contains($_.id) } | Sort-Object name, version
foreach ($pkg in $thirdParty) {
    $lic = if ($pkg.license) { $pkg.license } else { "(see crate manifest)" }
    $repo = if ($pkg.repository) { $pkg.repository } else { "(no repository listed)" }
    [void]$sb.AppendLine("------------------------------------------------------------------------")
    [void]$sb.AppendLine("$($pkg.name) $($pkg.version)")
    [void]$sb.AppendLine("License   : $lic")
    [void]$sb.AppendLine("Repository: $repo")
    $crateDir = Split-Path -Parent $pkg.manifest_path
    $licFiles = @(Get-ChildItem -LiteralPath $crateDir -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)' } |
        Sort-Object Name)
    if ($licFiles.Count -gt 0) {
        foreach ($lf in $licFiles) {
            [void]$sb.AppendLine("")
            [void]$sb.AppendLine("--- $($lf.Name) ---")
            [void]$sb.AppendLine((Get-Content -LiteralPath $lf.FullName -Raw).Trim())
        }
    }
    [void]$sb.AppendLine("")
}
Set-Content -LiteralPath (Join-Path $stageDir "THIRD-PARTY-LICENSES.txt") -Value $sb.ToString() -Encoding UTF8

if (-not $WithoutDriver) {
    # Opt-in Exclusive ACX virtual render driver (unsigned + self-contained per-machine installer)
    Copy-Item -Force (Join-Path $driverOut "ap_exclusive_audio.sys") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $driverOut "ap_exclusive_audio.inf") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $driverOut "ap_exclusive_audio.cat") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\NOTICE") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\README.md") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\install-driver.ps1") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\uninstall-driver.ps1") (Join-Path $stageDir "driver\")
    Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\update-driver.ps1") (Join-Path $stageDir "driver\")

    # Review #8 D2: Compute SHA-256 of unsigned driver artifacts, write driver\SHA256SUMS, and embed into install-driver.ps1
    $drvFiles = @("ap_exclusive_audio.sys", "ap_exclusive_audio.inf", "ap_exclusive_audio.cat")
    $shaLines = New-Object System.Collections.Generic.List[string]
    $embedEntries = New-Object System.Collections.Generic.List[string]
    foreach ($df in $drvFiles) {
        $h = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $stageDir "driver\$df")).Hash.ToLowerInvariant()
        $shaLines.Add("$h  $df")
        $embedEntries.Add("    `"$df`" = `"$h`"")
    }
    Set-Content -LiteralPath (Join-Path $stageDir "driver\SHA256SUMS") -Value ($shaLines -join "`r`n") -Encoding ASCII

    $stagedInstallPath = Join-Path $stageDir "driver\install-driver.ps1"
    $installRaw = Get-Content -LiteralPath $stagedInstallPath -Raw
    $embedBlock = "# @EMBEDDED_SHA256SUMS_START@`r`n`$EmbeddedSha256Sums = @{`r`n" + ($embedEntries -join "`r`n") + "`r`n}`r`n# @EMBEDDED_SHA256SUMS_END@"
    $installPatched = [regex]::Replace(
        $installRaw,
        '# @EMBEDDED_SHA256SUMS_START@.*?# @EMBEDDED_SHA256SUMS_END@',
        $embedBlock,
        [System.Text.RegularExpressions.RegexOptions]::Singleline
    )
    if ($installPatched -eq $installRaw) {
        throw "Failed to embed SHA-256 hashes into staged driver\install-driver.ps1 (markers missing)"
    }
    Set-Content -LiteralPath $stagedInstallPath -Value $installPatched -Encoding UTF8

    # Review #8 D12: Rewrite relative links in staged release docs so every link resolves cleanly inside the zip.
    $stagedDrvReadme = Join-Path $stageDir "driver\README.md"
    $drvReadmeRaw = Get-Content -LiteralPath $stagedDrvReadme -Raw
    $drvReadmeRaw = $drvReadmeRaw.Replace("(../../LICENSE)", "(../LICENSE)")
    Set-Content -LiteralPath $stagedDrvReadme -Value $drvReadmeRaw -Encoding UTF8

    $stagedGettingStarted = Join-Path $stageDir "docs\GETTING_STARTED.md"
    $gsRaw = Get-Content -LiteralPath $stagedGettingStarted -Raw
    $gsRaw = $gsRaw.Replace("(../drivers/ap-exclusive-acx/README.md)", "(../driver/README.md)")
    $gsRaw = $gsRaw.Replace("``drivers/ap-exclusive-acx/README.md``", "``driver/README.md``")
    Set-Content -LiteralPath $stagedGettingStarted -Value $gsRaw -Encoding UTF8
}

Write-Host "`n=== 4. Verifying release hygiene (forbidden extensions, strict manifest allowlist, unsigned binaries) ===" -ForegroundColor Cyan
$forbiddenExts = @(
    ".cer", ".pfx", ".pvk", ".key", ".pdb",
    ".p12", ".pem", ".crt", ".der", ".p7b", ".p7c", ".spc", ".snk", ".sst"
)
$forbidden = Get-ChildItem -Recurse -File $stageDir | Where-Object { $_.Extension.ToLowerInvariant() -in $forbiddenExts }
if ($forbidden) {
    throw "Forbidden files found in release staging dir: $(($forbidden | ForEach-Object FullName) -join ', ')"
}

# Review #8 D13: Strict manifest allowlist - fail if ANY unexpected file would enter the release zip!
$allowedManifest = @(
    "audiophile-player.exe",
    "audiophile-player.example.toml",
    "README.md",
    "LICENSE",
    "THIRD-PARTY-LICENSES.txt",
    "assets\audiophile-player.ico",
    "docs\QUICK_START.md",
    "docs\GETTING_STARTED.md",
    "docs\SOUND_ENGINE.md",
    "docs\ULTIMATE_MODE.md",
    "docs\CLI.md",
    "docs\FILTER_PREFERENCES.md",
    "docs\WHATS_NEW.md",
    "scripts\launch-audiophile-player.ps1",
    "scripts\install-shortcuts.ps1"
)
if (-not $WithoutDriver) {
    $allowedManifest += @(
        "driver\ap_exclusive_audio.sys",
        "driver\ap_exclusive_audio.inf",
        "driver\ap_exclusive_audio.cat",
        "driver\SHA256SUMS",
        "driver\NOTICE",
        "driver\README.md",
        "driver\install-driver.ps1",
        "driver\update-driver.ps1",
        "driver\uninstall-driver.ps1"
    )
}
$stagedFiles = @(Get-ChildItem -Recurse -File $stageDir | ForEach-Object {
    $_.FullName.Substring($stageDir.Length + 1)
})
$unexpected = @($stagedFiles | Where-Object { $allowedManifest -notcontains $_ })
if ($unexpected.Count -gt 0) {
    throw "Unexpected file(s) outside strict release manifest allowlist: $($unexpected -join ', ')"
}
$missing = @($allowedManifest | Where-Object { $stagedFiles -notcontains $_ })
if ($missing.Count -gt 0) {
    throw "Expected release manifest file(s) missing from staging dir: $($missing -join ', ')"
}

if (-not $WithoutDriver) {
    foreach ($bin in @("driver\ap_exclusive_audio.sys", "driver\ap_exclusive_audio.cat")) {
        $sig = Get-AuthenticodeSignature (Join-Path $stageDir $bin)
        if ($sig.Status -ne "NotSigned") {
            throw "Release driver file '$bin' must be UNSIGNED (got Status=$($sig.Status)). Never ship a personal test signature."
        }
    }
}

Write-Host "`n=== 5. Creating $zipPath ===" -ForegroundColor Cyan
if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
Compress-Archive -Path $stageDir -DestinationPath $zipPath -CompressionLevel Optimal
$hash = (Get-FileHash -Algorithm SHA256 -Path $zipPath).Hash.ToLowerInvariant()
"$hash  $bundleName.zip" | Set-Content -Encoding ASCII -NoNewline $shaPath

Write-Host "`n=== Release archive ready ===" -ForegroundColor Green
Write-Host "  Archive : $zipPath ($((Get-Item $zipPath).Length) bytes)"
Write-Host "  SHA-256 : $hash"
