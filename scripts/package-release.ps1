# Assembles a self-contained Windows x64 release archive under dist\audiophile-player-v<ver>-win-x64.zip.
#
# The release bundle contains:
#   - audiophile-player.exe + audiophile-player.example.toml
#   - README.md, LICENSE, docs\, assets\, scripts\ (Desktop/Start Menu shortcuts)
#   - driver\ (opt-in pure-Rust ACX virtual render driver for Tidal Desktop Exclusive Mode):
#       ap_exclusive_audio.sys (UNSIGNED), ap_exclusive_audio.inf, ap_exclusive_audio.cat (UNSIGNED),
#       NOTICE, README.md, install-driver.ps1 (signs locally per machine, zero WDK needed), uninstall-driver.ps1
#
# Safety invariant: fails loudly if any certificate/key file or pre-signed driver binary would enter the archive.
param(
    [switch]$SkipAppBuild,
    [switch]$SkipDriverBuild,
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

Write-Host "`n=== 3. Staging release directory $stageDir ===" -ForegroundColor Cyan
if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir }
New-Item -ItemType Directory -Force -Path `
    $stageDir, `
    (Join-Path $stageDir "docs"), `
    (Join-Path $stageDir "assets"), `
    (Join-Path $stageDir "scripts"), `
    (Join-Path $stageDir "driver") | Out-Null

Copy-Item -Force $exeSrc (Join-Path $stageDir "audiophile-player.exe")
Copy-Item -Force (Join-Path $repoRoot "audiophile-player.example.toml") $stageDir
Copy-Item -Force (Join-Path $repoRoot "README.md") $stageDir
Copy-Item -Force (Join-Path $repoRoot "LICENSE") $stageDir

foreach ($doc in @("GETTING_STARTED.md", "SOUND_ENGINE.md", "ULTIMATE_MODE.md")) {
    Copy-Item -Force (Join-Path $repoRoot "docs\$doc") (Join-Path $stageDir "docs\$doc")
}
Copy-Item -Force (Join-Path $repoRoot "assets\audiophile-player.ico") (Join-Path $stageDir "assets\audiophile-player.ico")
foreach ($sc in @("launch-audiophile-player.ps1", "install-shortcuts.ps1")) {
    Copy-Item -Force (Join-Path $repoRoot "scripts\$sc") (Join-Path $stageDir "scripts\$sc")
}

# Opt-in Exclusive ACX virtual render driver (unsigned + self-contained per-machine installer)
Copy-Item -Force (Join-Path $driverOut "ap_exclusive_audio.sys") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $driverOut "ap_exclusive_audio.inf") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $driverOut "ap_exclusive_audio.cat") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\NOTICE") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\README.md") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\install-driver.ps1") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\uninstall-driver.ps1") (Join-Path $stageDir "driver\")
Copy-Item -Force (Join-Path $repoRoot "drivers\ap-exclusive-acx\scripts\update-driver.ps1") (Join-Path $stageDir "driver\")

Write-Host "`n=== 4. Verifying release hygiene (zero certs/keys, unsigned driver binaries) ===" -ForegroundColor Cyan
$forbidden = Get-ChildItem -Recurse -File $stageDir | Where-Object { $_.Extension -in @(".cer", ".pfx", ".pvk", ".key", ".pdb") }
if ($forbidden) {
    throw "Forbidden files found in release staging dir: $(($forbidden | ForEach-Object FullName) -join ', ')"
}
foreach ($bin in @("driver\ap_exclusive_audio.sys", "driver\ap_exclusive_audio.cat")) {
    $sig = Get-AuthenticodeSignature (Join-Path $stageDir $bin)
    if ($sig.Status -ne "NotSigned") {
        throw "Release driver file '$bin' must be UNSIGNED (got Status=$($sig.Status)). Never ship a personal test signature."
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
