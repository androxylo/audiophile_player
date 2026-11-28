# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo
#
# Audiophile Player Windows Bootstrap Installer
# Builds from source by default (or installs a SHA-256 verified release zip with -Binary).
# Exit codes: 0=ok, 10=prereqs/disk, 20=git, 30=build, 40=binary-verify, 50=post-check

param(
    [string]$Dir = (Join-Path $env:LOCALAPPDATA 'AudiophilePlayer'),
    [string]$Ref = 'main',
    [switch]$Binary,
    [switch]$SkipAttestation,
    [switch]$NonInteractive,
    [switch]$WhatIf,
    [switch]$SkipShortcuts,
    [switch]$RegisterMcp,
    [switch]$Uninstall,
    [switch]$Prune
)

$ErrorActionPreference = 'Stop'
$RepoUrl = 'https://github.com/androxylo/audiophile_player.git'
$RepoSlug = 'androxylo/audiophile_player'

function Write-Step([string]$Msg) { Write-Host "==> $Msg" -ForegroundColor Cyan }
function Write-Ok([string]$Msg)   { Write-Host "  [ok] $Msg" -ForegroundColor Green }
function Write-Warn([string]$Msg) { Write-Host "  [warn] $Msg" -ForegroundColor Yellow }
function Fail([string]$Msg, [int]$Code) {
    Write-Host "error: $Msg" -ForegroundColor Red
    exit $Code
}

function Refresh-ProcessPath {
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    $parts = @($cargoBin, $userPath, $machinePath) | Where-Object { $_ }
    $env:PATH = ($parts -join ';')
}

function Test-ToolchainLinker {
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        if (Get-Command rustup -ErrorAction SilentlyContinue) {
            $active = (& rustup show active-toolchain 2>$null | Out-String)
            if ($LASTEXITCODE -eq 0 -and $active -match '-gnu\b') {
                return $true
            }
        }
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (Test-Path -LiteralPath $vswhere) {
            $found = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
            if ($LASTEXITCODE -eq 0 -and $found) { return $true }
        }
    } finally {
        $ErrorActionPreference = $prevEap
    }
    return [bool](Get-Command link.exe -ErrorAction SilentlyContinue)
}

function Stop-RunningPlayer([string]$Root) {
    # Review #10 OD16: cleanly stop any running audiophile-player.exe before overwriting it in $Dir.
    $installedExe = Join-Path $Root 'audiophile-player.exe'
    $running = @(Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) { return }
    Write-Step 'Stopping running audiophile-player.exe before updating binary'
    if (Test-Path -LiteralPath $installedExe) {
        try { & $installedExe stop --when now 2>$null | Out-Null } catch {}
        Start-Sleep -Milliseconds 500
    }
    $still = @(Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue)
    if ($still.Count -gt 0) {
        Write-Warn 'Graceful stop did not exit; terminating remaining audiophile-player process.'
        $still | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 300
    }
}

function Write-InstallReceipt([string]$Root, [hashtable]$Data) {
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add('# Managed by audiophile-player bootstrap.ps1')
    foreach ($k in ($Data.Keys | Sort-Object)) {
        $v = [string]$Data[$k]
        $v = $v.Replace("'", "''")
        $lines.Add("${k}: '${v}'")
    }
    Set-Content -LiteralPath (Join-Path $Root 'install-receipt.yaml') -Value ($lines -join "`r`n") -Encoding ASCII
}

if ($Uninstall) {
    Write-Step "Uninstalling Audiophile Player from $Dir (music library and %APPDATA%\audiophile-player\config.toml are preserved)"
    if ($WhatIf) {
        Write-Host "  [WhatIf] Would stop audiophile-player, remove Start Menu/Desktop shortcuts, and remove $Dir"
        exit 0
    }
    $installedExe = Join-Path $Dir 'audiophile-player.exe'
    if (Test-Path -LiteralPath $installedExe) {
        try { & $installedExe stop --when now 2>$null | Out-Null } catch {}
    }
    $still = Get-Process -Name 'audiophile-player' -ErrorAction SilentlyContinue
    if ($still) {
        Write-Warn 'Graceful stop did not exit; terminating remaining audiophile-player process.'
        $still | Stop-Process -Force -ErrorAction SilentlyContinue
    }
    foreach ($lnk in @(
        (Join-Path ([Environment]::GetFolderPath('Desktop')) 'Audiophile Player.lnk'),
        (Join-Path ([Environment]::GetFolderPath('Programs')) 'Audiophile Player.lnk')
    )) {
        if (Test-Path -LiteralPath $lnk) { Remove-Item -Force -LiteralPath $lnk }
    }
    if (Test-Path -LiteralPath $Dir) { Remove-Item -Recurse -Force -LiteralPath $Dir }
    Write-Ok 'Uninstall complete.'
    exit 0
}

if ($Prune) {
    $binRoot = Join-Path $Dir 'bin'
    Write-Step "Pruning old versions under $binRoot (keeping newest 2)"
    if (Test-Path -LiteralPath $binRoot) {
        $versions = @(Get-ChildItem -LiteralPath $binRoot -Directory | Sort-Object LastWriteTimeUtc -Descending)
        $toDrop = $versions | Select-Object -Skip 2
        foreach ($d in $toDrop) {
            if ($WhatIf) { Write-Host "  [WhatIf] Would remove $($d.FullName)" }
            else { Remove-Item -Recurse -Force -LiteralPath $d.FullName; Write-Ok "Removed $($d.Name)" }
        }
    }
    exit 0
}

# Step 0: OS and disk space check
if (-not [Environment]::Is64BitOperatingSystem) {
    Fail 'Audiophile Player requires 64-bit Windows 10/11.' 10
}
$driveRoot = Split-Path -Qualifier $Dir
if (-not $driveRoot) { $driveRoot = $env:SystemDrive }
$driveLetter = $driveRoot.TrimEnd(':')
$minGb = if ($Binary) { 0.5 } else { 5.0 }
$psDrive = Get-PSDrive -Name $driveLetter -ErrorAction SilentlyContinue
if ($psDrive -and ($psDrive.Free / 1GB) -lt $minGb) {
    Fail ("Insufficient free disk space on {0}: need at least {1:N1} GB." -f $driveRoot, $minGb) 10
}

$activeExe = ''
$installVer = '0.0.0'
$commitSha = ''
$zipSha = ''

if (-not $Binary) {
    # Source-build path (default)
    Write-Step 'Checking Windows source-build prerequisites (Git, Rustup, Visual Studio C++ Build Tools)'
    Refresh-ProcessPath
    $hasGit = [bool](Get-Command git -ErrorAction SilentlyContinue)
    $hasCargo = [bool](Get-Command cargo -ErrorAction SilentlyContinue)
    $hasLinker = Test-ToolchainLinker

    if (-not ($hasGit -and $hasCargo -and $hasLinker)) {
        $winget = Get-Command winget -ErrorAction SilentlyContinue
        if (-not $winget) {
            Fail 'WinGet is required to install missing build tools. Install App Installer from Microsoft Store or install Git, Rustup, and VS 2022 Build Tools manually.' 10
        }
        $missing = @()
        if (-not $hasGit)    { $missing += 'Git.Git' }
        if (-not $hasCargo)  { $missing += 'Rustlang.Rustup' }
        if (-not $hasLinker) { $missing += 'Microsoft.VisualStudio.2022.BuildTools' }
        Write-Warn "Missing build tools: $($missing -join ', ')"
        if ($WhatIf) {
            Write-Host "  [WhatIf] Would install via WinGet: $($missing -join ', ')"
        } else {
            # Review #10 OD18: warn explicitly before launching WinGet installers that trigger UAC.
            Write-Warn "Windows may display a User Account Control (UAC) elevation prompt to install: $($missing -join ', ')."
            Write-Warn "If installation pauses, check the taskbar for a blinking UAC shield and click Yes."
            foreach ($pkg in $missing) {
                $wgArgs = @('install', '--exact', '--id', $pkg, '--source', 'winget', '--accept-package-agreements', '--accept-source-agreements')
                if ($NonInteractive) {
                    $wgArgs += @('--silent', '--disable-interactivity')
                }
                if ($pkg -eq 'Microsoft.VisualStudio.2022.BuildTools') {
                    $wgArgs += @('--override', '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended')
                }
                & winget @wgArgs
                if ($LASTEXITCODE -ne 0) {
                    Fail "WinGet install failed for $pkg (exit $LASTEXITCODE)." 10
                }
            }
            Refresh-ProcessPath
        }
    } else {
        Write-Ok 'Git, Cargo, and MSVC C++ Build Tools are present.'
    }

    $srcDir = Join-Path $Dir 'src'
    Write-Step "Fetching source ($RepoUrl @ $Ref) into $srcDir"
    if ($WhatIf) {
        Write-Host "  [WhatIf] Would clone/update $RepoUrl ($Ref) into $srcDir and run cargo build --release --bin audiophile-player"
        exit 0
    }
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    if (-not (Test-Path -LiteralPath (Join-Path $srcDir '.git'))) {
        & git clone --branch $Ref --depth 1 $RepoUrl $srcDir
        if ($LASTEXITCODE -ne 0) { exit 20 }
    } else {
        Push-Location $srcDir
        try {
            & git fetch --depth 1 origin $Ref
            if ($LASTEXITCODE -ne 0) { exit 20 }
            & git checkout FETCH_HEAD
            if ($LASTEXITCODE -ne 0) { exit 20 }
        } finally { Pop-Location }
    }

    Push-Location $srcDir
    try {
        $commitSha = (& git rev-parse --short=12 HEAD).Trim()
        Write-Step 'Building audiophile-player.exe (release, --locked)'
        & cargo build --release --locked --bin audiophile-player
        if ($LASTEXITCODE -ne 0) { exit 30 }
        $cargoToml = Get-Content -LiteralPath (Join-Path $srcDir 'Cargo.toml') -Raw
        if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') { $installVer = $Matches[1] }
    } finally { Pop-Location }

    Stop-RunningPlayer $Dir
    $verDir = Join-Path $Dir "bin\v$installVer"
    New-Item -ItemType Directory -Force -Path $verDir, (Join-Path $Dir 'scripts'), (Join-Path $Dir 'assets'), (Join-Path $Dir 'docs') | Out-Null
    $activeExe = Join-Path $verDir 'audiophile-player.exe'
    Copy-Item -Force (Join-Path $srcDir 'target\release\audiophile-player.exe') $activeExe
    Copy-Item -Force $activeExe (Join-Path $Dir 'audiophile-player.exe')
    Copy-Item -Force (Join-Path $srcDir 'audiophile-player.example.toml') $Dir
    Copy-Item -Force (Join-Path $srcDir 'assets\audiophile-player.ico') (Join-Path $Dir 'assets\')
    foreach ($sc in @('launch-audiophile-player.ps1', 'install-shortcuts.ps1')) {
        if (Test-Path -LiteralPath (Join-Path $srcDir "scripts\$sc")) {
            Copy-Item -Force (Join-Path $srcDir "scripts\$sc") (Join-Path $Dir 'scripts\')
        }
    }
    foreach ($doc in @('QUICK_START.md', 'GETTING_STARTED.md', 'SOUND_ENGINE.md', 'ULTIMATE_MODE.md', 'CLI.md', 'FILTER_PREFERENCES.md', 'WHATS_NEW.md')) {
        if (Test-Path -LiteralPath (Join-Path $srcDir "docs\$doc")) {
            Copy-Item -Force (Join-Path $srcDir "docs\$doc") (Join-Path $Dir 'docs\')
        }
    }
    Write-InstallReceipt $Dir @{
        install_kind = 'source'
        version      = $installVer
        ref          = $Ref
        commit       = $commitSha
        exe_path     = $activeExe
        installed_at = (Get-Date).ToUniversalTime().ToString('o')
    }
} else {
    # Opt-in verified pre-built binary path
    $apiUrl = if ($Ref -eq 'main') {
        "https://api.github.com/repos/$RepoSlug/releases/latest"
    } else {
        "https://api.github.com/repos/$RepoSlug/releases/tags/$Ref"
    }
    Write-Step "Fetching release metadata from $apiUrl"
    if ($WhatIf) {
        Write-Host "  [WhatIf] Would download release zip + SHA256SUMS from $apiUrl, verify SHA-256 (and gh attestation if installed), and extract to $Dir"
        exit 0
    }
    try {
        $rel = Invoke-RestMethod -Uri $apiUrl -Headers @{ 'User-Agent' = 'audiophile-player-bootstrap' }
    } catch {
        Fail "No published GitHub Release found at $apiUrl. Run without -Binary to build from source instead." 40
    }
    $zipAsset = $rel.assets | Where-Object { $_.name -match '^audiophile-player-v[0-9].*-win-x64\.zip$' } | Select-Object -First 1
    $shaAsset = $rel.assets | Where-Object { $_.name -eq 'SHA256SUMS' -or $_.name -eq ($zipAsset.name + '.sha256') } | Select-Object -First 1
    if (-not $zipAsset -or -not $shaAsset) {
        Fail 'Release is missing win-x64 zip or SHA-256 checksum asset. Run without -Binary to build from source.' 40
    }
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("ap-bootstrap-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
    try {
        $zipFile = Join-Path $tmpDir $zipAsset.name
        $shaFile = Join-Path $tmpDir $shaAsset.name
        Invoke-WebRequest -Uri $zipAsset.browser_download_url -OutFile $zipFile
        Invoke-WebRequest -Uri $shaAsset.browser_download_url -OutFile $shaFile

        $actualHash = (Get-FileHash -Algorithm SHA256 -Path $zipFile).Hash.ToLowerInvariant()
        $shaText = Get-Content -LiteralPath $shaFile -Raw
        $matchedLine = ($shaText -split "`r?`n") | Where-Object { $_ -match [regex]::Escape($zipAsset.name) } | Select-Object -First 1
        if (-not $matchedLine -or ($matchedLine.Trim().Split()[0].ToLowerInvariant() -ne $actualHash)) {
            Fail "SHA-256 verification FAILED for $($zipAsset.name)!" 40
        }
        $zipSha = $actualHash
        Write-Ok "SHA-256 verified: $zipSha"

        # Review #10 OD7: fail closed on missing GitHub provenance attestation in -Binary mode
        # unless the caller explicitly passes -SkipAttestation (relying on SHA-256 only).
        if (-not $SkipAttestation) {
            if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
                Fail "GitHub CLI ('gh') is required to verify build provenance attestation in -Binary mode. Install 'gh' and run 'gh auth login', omit -Binary to build from source, or pass -SkipAttestation to rely on SHA-256 only." 40
            }
            $ghAuthed = [bool]($env:GH_TOKEN -or $env:GITHUB_TOKEN)
            if (-not $ghAuthed) {
                $prevEap = $ErrorActionPreference
                $ErrorActionPreference = 'Continue'
                try {
                    & gh auth status 2>$null | Out-Null
                    $ghAuthed = ($LASTEXITCODE -eq 0)
                } finally {
                    $ErrorActionPreference = $prevEap
                }
            }
            if (-not $ghAuthed) {
                Fail "GitHub CLI ('gh') is not authenticated (run 'gh auth login') so build provenance attestation cannot be verified. Authenticate 'gh', omit -Binary to build from source, or pass -SkipAttestation to rely on SHA-256 only." 40
            }
            & gh attestation verify $zipFile --repo $RepoSlug
            if ($LASTEXITCODE -ne 0) {
                Fail 'GitHub build provenance attestation verification FAILED.' 40
            }
            Write-Ok 'GitHub build provenance attestation verified.'
        } else {
            Write-Warn 'Skipping GitHub build provenance attestation (-SkipAttestation explicitly set; SHA-256 verified).'
        }

        Stop-RunningPlayer $Dir
        $installVer = ($rel.tag_name -replace '^v', '')
        $verDir = Join-Path $Dir "bin\v$installVer"
        if (Test-Path -LiteralPath $verDir) { Remove-Item -Recurse -Force -LiteralPath $verDir }
        New-Item -ItemType Directory -Force -Path $verDir | Out-Null
        Expand-Archive -Path $zipFile -DestinationPath $tmpDir\extract -Force
        $inner = Get-ChildItem -LiteralPath "$tmpDir\extract" -Directory | Select-Object -First 1
        $copyRoot = if ($inner) { $inner.FullName } else { "$tmpDir\extract" }
        Copy-Item -Recurse -Force "$copyRoot\*" $verDir
        Copy-Item -Recurse -Force "$copyRoot\*" $Dir
        $activeExe = Join-Path $verDir 'audiophile-player.exe'
        Write-InstallReceipt $Dir @{
            install_kind = 'binary'
            version      = $installVer
            ref          = $rel.tag_name
            sha256       = $zipSha
            exe_path     = $activeExe
            installed_at = (Get-Date).ToUniversalTime().ToString('o')
        }
    } finally {
        if (Test-Path -LiteralPath $tmpDir) { Remove-Item -Recurse -Force -LiteralPath $tmpDir }
    }
}

if (-not $SkipShortcuts) {
    $shortcutScript = Join-Path $Dir 'scripts\install-shortcuts.ps1'
    if (Test-Path -LiteralPath $shortcutScript) {
        Write-Step 'Installing Start Menu and Desktop shortcuts'
        & powershell -ExecutionPolicy Bypass -File $shortcutScript
    }
}

if ($RegisterMcp) {
    $mcpSnippet = Join-Path $Dir 'mcp-config.json'
    $stableExe = Join-Path $Dir 'audiophile-player.exe'
    $escapedExe = $stableExe.Replace('\', '\\')
    $json = "{`r`n  `"mcpServers`": {`r`n    `"audiophile-player`": {`r`n      `"command`": `"$escapedExe`",`r`n      `"args`": [`"mcp`"]`r`n    }`r`n  }`r`n}"
    Set-Content -LiteralPath $mcpSnippet -Value $json -Encoding ASCII
    Write-Ok "Wrote MCP server snippet to $mcpSnippet (merge into your AI client MCP config)."
}

Write-Step 'Running post-install verification (audiophile-player doctor)'
if (-not (Test-Path -LiteralPath $activeExe)) {
    Fail "Installed binary missing: $activeExe" 50
}
& $activeExe --version
if ($LASTEXITCODE -ne 0) { exit 50 }
& $activeExe doctor
Write-Ok "Audiophile Player v$installVer installed in $Dir"
exit 0
