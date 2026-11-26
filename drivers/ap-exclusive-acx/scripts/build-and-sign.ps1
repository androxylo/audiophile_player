# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

# Builds ap_exclusive_audio.sys + ap_exclusive_audio.inf + ap_exclusive_audio.cat
# using either the local .wdk\kit cache or a system WDK 10.0.26100 install (no Admin needed).
#
# Switches:
#   -Unsigned     Produce unsigned .sys and .cat (for GitHub Release bundles; no cert/signature emitted).
#   -FetchWdk     Auto-download official Microsoft WDK/SDK 10.0.26100 NuGet packages into .wdk\ if missing
#                 (SHA-512 pinned + atomic extraction + Microsoft Authenticode check on build tools).
#   -OutDir       Custom output directory (default: drivers\ap-exclusive-acx\out).
#   -BuildNumber  Optional 4th DriverVer component (default: git commit count % 65535 for reproducible upgrades).
param(
    [switch]$Unsigned,
    [switch]$FetchWdk,
    [string]$OutDir = "",
    [int]$BuildNumber = -1
)
$ErrorActionPreference = "Stop"

$root     = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent (Split-Path -Parent $root)
$localWdk = Join-Path $root ".wdk"
$localKit = Join-Path $localWdk "kit"
$localExt = Join-Path $localWdk "extracted"
$out      = if ($OutDir) { $OutDir } else { Join-Path $root "out" }
$kver     = "10.0.26100.0"
$kmdf     = "1.31"

function Assert-MicrosoftSignedTool([string]$ToolPath) {
    if (-not (Test-Path -LiteralPath $ToolPath)) {
        throw "Required WDK/SDK build tool missing: $ToolPath"
    }
    $sig = Get-AuthenticodeSignature -FilePath $ToolPath
    $subj = if ($sig.SignerCertificate) { $sig.SignerCertificate.Subject } else { "" }
    $thumb = if ($sig.SignerCertificate) { $sig.SignerCertificate.Thumbprint } else { "" }
    $isMs = $subj -match "O=Microsoft Corporation"
    if ($sig.Status -eq "Valid" -and $isMs) {
        return
    }
    # Microsoft ships WDK 10.0.26100 Inf2Cat.exe signed with its internal Build Tools CodeSign CA
    # (hash intact, root not in default Windows Root store). Allow ONLY that exact pinned thumbprint.
    if ($sig.Status -eq "UnknownError" -and $isMs `
        -and $thumb -eq "0061B7902128989780FAB7D913EA8D60FF9BB766" `
        -and $sig.StatusMessage -match "root certificate") {
        return
    }
    throw "Refusing to execute untrusted build tool '$ToolPath' (Status=$($sig.Status), Thumb=$thumb, Signer=$subj)"
}

function Bootstrap-LocalWdk {
    Write-Host "=== Bootstrapping local Microsoft WDK/SDK NuGet cache in $localWdk ===" -ForegroundColor Cyan
    $pkgsDir = Join-Path $localWdk "downloads"
    New-Item -ItemType Directory -Force -Path $pkgsDir, $localExt, $localKit | Out-Null

    # Review #8 D9: Pinned SHA-512 hashes for official Microsoft WDK/SDK NuGet packages.
    $packages = @(
        @{
            Id     = "microsoft.windows.wdk.x64"
            Ver    = "10.0.26100.6584"
            Sha512 = "8e175d6819e1303aaddc656bdf64554ed691d0a1e66438d8d09093327d74390a64e3e01285708af50034f6f05ace9f278b5323bbce2a3394865df21f9f2389fa"
        },
        @{
            Id     = "microsoft.windows.sdk.cpp"
            Ver    = "10.0.26100.9457"
            Sha512 = "fdae45feeed2b07871c54ac484b1bef454e74ff082ec9a640801960df8b62d3a692a4b5f071756f39728567c28c58c8b6751f4c6af9161684cb0e0410baff85b"
        },
        @{
            Id     = "microsoft.windows.sdk.cpp.x64"
            Ver    = "10.0.26100.9457"
            Sha512 = "fe318b5604abb4dc331604f9e7914fdd8b8849892bfac3288f92a43b0c1393e56549cf53968128a2c6270b13c2de3ec671f4b2545dd60ce193449737573e901a"
        },
        @{
            Id     = "microsoft.windows.sdk.buildtools"
            Ver    = "10.0.26100.9457"
            Sha512 = "6d5c339c34fc0044d295ee1e3e130b5cc16b8ef2e31e37ba0281f35f4719809ee78417e2c59203d6de4626fa6234034d6e774ee7c05b2eff557cc1d46d0f664c"
        }
    )
    foreach ($p in $packages) {
        $nupkg = Join-Path $pkgsDir "$($p.Id).$($p.Ver).nupkg"
        $dest  = Join-Path $localExt $p.Id
        if (-not (Test-Path -LiteralPath $nupkg)) {
            $url = "https://api.nuget.org/v3-flatcontainer/$($p.Id)/$($p.Ver)/$($p.Id).$($p.Ver).nupkg"
            $tmpPkg = "$nupkg.tmp"
            Remove-Item -Force $tmpPkg -ErrorAction SilentlyContinue
            Write-Host "Downloading $($p.Id) $($p.Ver)..."
            & curl.exe -sSL --fail -o $tmpPkg $url
            if ($LASTEXITCODE -ne 0) {
                Remove-Item -Force $tmpPkg -ErrorAction SilentlyContinue
                throw "Failed downloading $url"
            }
            Move-Item -Force $tmpPkg $nupkg
        }

        # Verify pinned SHA-512 BEFORE extracting (Review #8 D9).
        $actualSha = (Get-FileHash -Algorithm SHA512 -Path $nupkg).Hash.ToLowerInvariant()
        if ($actualSha -ne $p.Sha512) {
            Remove-Item -Force $nupkg -ErrorAction SilentlyContinue
            throw "SHA-512 integrity check FAILED for $($p.Id).$($p.Ver).nupkg!`n  Expected: $($p.Sha512)`n  Actual  : $actualSha"
        }

        if (-not (Test-Path -LiteralPath $dest)) {
            Write-Host "Extracting $($p.Id) (SHA-512 verified)..."
            $destTmp = "$dest.tmp"
            $zipCopy = "$nupkg.zip"
            if (Test-Path -LiteralPath $destTmp) { Remove-Item -Recurse -Force $destTmp }
            Copy-Item -Force $nupkg $zipCopy
            try {
                Expand-Archive -Path $zipCopy -DestinationPath $destTmp -Force
                Move-Item -Force $destTmp $dest
            } catch {
                if (Test-Path -LiteralPath $destTmp) { Remove-Item -Recurse -Force $destTmp -ErrorAction SilentlyContinue }
                throw
            } finally {
                Remove-Item -Force $zipCopy -ErrorAction SilentlyContinue
            }
        }
    }

    # Assemble unified WDK/SDK kit layout under .wdk\kit
    $wdkC = Join-Path $localExt "microsoft.windows.wdk.x64\c"
    $sdkC = Join-Path $localExt "microsoft.windows.sdk.cpp\c"
    $sdkX = Join-Path $localExt "microsoft.windows.sdk.cpp.x64\c"
    $bt   = Join-Path $localExt "microsoft.windows.sdk.buildtools"

    $copies = @(
        @{ Src = "$wdkC\Include";          Dst = "$localKit\Include" },
        @{ Src = "$sdkC\Include";          Dst = "$localKit\Include" },
        @{ Src = "$wdkC\Lib";              Dst = "$localKit\Lib" },
        @{ Src = "$sdkX\um\x64";           Dst = "$localKit\Lib\$kver\um\x64" },
        @{ Src = "$sdkX\ucrt\x64";         Dst = "$localKit\Lib\$kver\ucrt\x64" },
        @{ Src = "$wdkC\bin";              Dst = "$localKit\bin" },
        @{ Src = "$bt\bin";                Dst = "$localKit\bin" },
        @{ Src = "$wdkC\tools";            Dst = "$localKit\tools" }
    )
    foreach ($c in $copies) {
        if (Test-Path $c.Src) {
            New-Item -ItemType Directory -Force -Path $c.Dst | Out-Null
            Copy-Item -Recurse -Force -Path "$($c.Src)\*" -Destination $c.Dst
        }
    }
    if (-not (Test-Path "$localKit\Include\$kver\km\wdm.h")) {
        throw "Local WDK kit assembly failed: wdm.h missing under $localKit\Include\$kver\km"
    }
}

function Find-VsWhere {
    $candidates = @(
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\Installer\vswhere.exe"
    )
    return $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}

function Find-MsvcRoot {
    $vswhere = Find-VsWhere
    $installs = @()
    if ($vswhere) {
        $installs = @(& $vswhere -products * -prerelease -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -sort -property installationPath 2>$null)
    }
    foreach ($inst in $installs) {
        $msvcBase = Join-Path $inst "VC\Tools\MSVC"
        if (Test-Path $msvcBase) {
            $verDir = Get-ChildItem -Directory $msvcBase | Sort-Object Name -Descending |
                Where-Object { Test-Path (Join-Path $_.FullName "bin\Hostx64\x64\link.exe") } |
                Select-Object -First 1
            if ($verDir) { return $verDir.FullName }
        }
    }
    # Fallback: check VCINSTALLDIR from an active Developer PowerShell
    if ($env:VCToolsInstallDir -and (Test-Path "$($env:VCToolsInstallDir)\bin\Hostx64\x64\link.exe")) {
        return $env:VCToolsInstallDir.TrimEnd('\')
    }
    throw "MSVC x64 toolset (link.exe) not found. Install Visual Studio C++ Build Tools (x64)."
}

function Find-LibClangDir {
    $candidates = New-Object System.Collections.Generic.List[string]
    if ($env:LIBCLANG_PATH) { $candidates.Add($env:LIBCLANG_PATH) }
    $vswhere = Find-VsWhere
    if ($vswhere) {
        $installs = @(& $vswhere -products * -prerelease -sort -property installationPath 2>$null)
        foreach ($inst in $installs) {
            $candidates.Add((Join-Path $inst "VC\Tools\Llvm\x64\bin"))
            $candidates.Add((Join-Path $inst "VC\Tools\Llvm\bin"))
        }
    }
    $candidates.Add("$env:ProgramFiles\LLVM\bin")
    $candidates.Add("${env:ProgramFiles(x86)}\LLVM\bin")
    foreach ($dir in $candidates) {
        if ($dir -and (Test-Path (Join-Path $dir "libclang.dll"))) {
            return $dir
        }
    }
    throw "libclang.dll not found. Install LLVM (winget install LLVM.LLVM) or Visual Studio Clang tools, or set LIBCLANG_PATH."
}

# 0. Locate WDK 10.0.26100 (or bootstrap locally if -FetchWdk)
if (Test-Path "$localKit\Include\$kver\km\wdm.h") {
    $kit = $localKit
} elseif (Test-Path "C:\Program Files (x86)\Windows Kits\10\Include\$kver\km\wdm.h") {
    $kit = "C:\Program Files (x86)\Windows Kits\10"
} elseif ($FetchWdk) {
    Bootstrap-LocalWdk
    $kit = $localKit
} else {
    throw "WDK $kver not found at $localKit or C:\Program Files (x86)\Windows Kits\10. Re-run with -FetchWdk to download locally via NuGet."
}

$msvc     = Find-MsvcRoot
$libclang = Find-LibClangDir

$k32 = @(
    "$localExt\microsoft.windows.sdk.cpp.x64\c\um\x64",
    "$kit\Lib\$kver\um\x64",
    "C:\Program Files (x86)\Windows Kits\10\Lib\$kver\um\x64"
) | Where-Object { Test-Path "$_\kernel32.lib" } | Select-Object -First 1
if (-not $k32) { throw "Windows SDK um\x64 (kernel32.lib) not found" }

$ucrt = @(
    "$localExt\microsoft.windows.sdk.cpp.x64\c\ucrt\x64",
    "$kit\Lib\$kver\ucrt\x64",
    "C:\Program Files (x86)\Windows Kits\10\Lib\$kver\ucrt\x64"
) | Where-Object { Test-Path "$_\ucrt.lib" } | Select-Object -First 1
if (-not $ucrt) { throw "Windows SDK ucrt\x64 (ucrt.lib) not found" }

$env:PATH = "$env:USERPROFILE\.cargo\bin;$msvc\bin\Hostx64\x64;$env:PATH"
$env:LIB  = "$msvc\lib\x64;$k32;$ucrt"
$env:LIBCLANG_PATH   = $libclang
$env:AP_WDK_KIT_ROOT = $kit

# Review #8 D8: Deterministic DriverVer derived from workspace version + build/commit counter + commit date.
$wsCargoPath = Join-Path $repoRoot "Cargo.toml"
$verSource = if (Test-Path -LiteralPath $wsCargoPath) { Get-Content -LiteralPath $wsCargoPath -Raw } else { Get-Content -LiteralPath "$root\Cargo.toml" -Raw }
$major = "0"; $minor = "1"; $patch = "0"
if ($verSource -match '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"') {
    $major = $Matches[1]; $minor = $Matches[2]; $patch = $Matches[3]
}
if ($BuildNumber -lt 0) {
    $commitCount = $null
    try {
        $rawCnt = (& git -C $repoRoot rev-list --count HEAD 2>$null)
        if ($LASTEXITCODE -eq 0 -and $rawCnt -match '^\d+$') {
            $commitCount = ([int]$rawCnt) % 65535
        }
    } catch {}
    $BuildNumber = if ($null -ne $commitCount) { $commitCount } else { 0 }
}
$infVer = "$major.$minor.$patch.$BuildNumber"

$driverDate = "01/01/2026"
try {
    $gitDate = (& git -C $repoRoot log -1 --format=%cd --date=format:%m/%d/%Y 2>$null)
    if ($LASTEXITCODE -eq 0 -and $gitDate -match '^\d{2}/\d{2}/\d{4}$') {
        $driverDate = $gitDate.Trim()
    }
} catch {}

Write-Host "=== 1. Building Pure-Rust ACX 1.1 Kernel Driver (ap_exclusive_audio.sys v$infVer, date $driverDate) ===" -ForegroundColor Cyan
Write-Host "  WDK      : $kit"
Write-Host "  MSVC     : $msvc"
Write-Host "  libclang : $libclang"
cargo +stable-x86_64-pc-windows-msvc build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw "Kernel driver build failed" }

Write-Host "`n=== 2. Packaging driver into $out ===" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $out | Out-Null
$sysPath = Join-Path $out "ap_exclusive_audio.sys"
$infPath = Join-Path $out "ap_exclusive_audio.inf"
$catPath = Join-Path $out "ap_exclusive_audio.cat"
Copy-Item -Force "$root\target\release\ap_exclusive_audio.dll" $sysPath
Copy-Item -Force "$root\driver\ap_exclusive_audio.inx" $infPath

# Clear cdylib IMAGE_FILE_DLL bit (0x2000) and recompute PE checksum for /INTEGRITYCHECK
$bytes = [System.IO.File]::ReadAllBytes($sysPath)
$peOff = [BitConverter]::ToInt32($bytes, 0x3C)
$charsOff = $peOff + 4 + 18
$chars = [BitConverter]::ToUInt16($bytes, $charsOff)
if (($chars -band 0x2000) -ne 0) {
    $newChars = [uint16]($chars -band (-bnot 0x2000))
    $cb = [BitConverter]::GetBytes($newChars)
    $bytes[$charsOff]     = $cb[0]
    $bytes[$charsOff + 1] = $cb[1]
    [System.IO.File]::WriteAllBytes($sysPath, $bytes)
    & "$msvc\bin\Hostx64\x64\editbin.exe" /NOLOGO /RELEASE $sysPath
    if ($LASTEXITCODE -ne 0) { throw "editbin /RELEASE failed" }
}

Write-Host "`n=== 3. Stamping INF, generating CAT & verifying INF ===" -ForegroundColor Cyan
$stampinf = "$kit\bin\$kver\x64\stampinf.exe"
$inf2cat  = "$kit\bin\$kver\x86\Inf2Cat.exe"
$infverif = "$kit\tools\$kver\x64\infverif.exe"
$signtool = @(
    "$kit\bin\$kver\x64\signtool.exe",
    "$localExt\microsoft.windows.sdk.buildtools\bin\$kver\x64\signtool.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

# Review #8 D9: Verify Microsoft Authenticode signatures on WDK/SDK executables before running.
Assert-MicrosoftSignedTool $stampinf
Assert-MicrosoftSignedTool $inf2cat
if ($signtool) { Assert-MicrosoftSignedTool $signtool }

& $stampinf -f $infPath -d $driverDate -a amd64 -k $kmdf -v $infVer
if ($LASTEXITCODE -ne 0) { throw "stampinf failed (exit $LASTEXITCODE)" }

& $inf2cat /driver:"$out" /os:10_NI_X64 /uselocaltime
if ($LASTEXITCODE -ne 0) { throw "Inf2Cat failed (exit $LASTEXITCODE)" }

# Review #8 D10: InfVerif is strictly fatal for -Unsigned release builds (and missing InfVerif fails too).
if (Test-Path -LiteralPath $infverif) {
    Assert-MicrosoftSignedTool $infverif
    & $infverif /v /w $infPath
    if ($LASTEXITCODE -ne 0) {
        if ($Unsigned) {
            throw "InfVerif failed with exit code $LASTEXITCODE (fatal for release builds)"
        }
        Write-Host "InfVerif returned $LASTEXITCODE" -ForegroundColor Yellow
    }
} elseif ($Unsigned) {
    throw "infverif.exe not found at $infverif (required for release builds)"
}

if ($Unsigned) {
    Remove-Item -Force (Join-Path $out "ApExclusiveTestCert.cer") -ErrorAction SilentlyContinue
    Write-Host "`n=== Built UNSIGNED release driver artifacts in $out ===" -ForegroundColor Green
    Write-Host "  (End-user install-driver.ps1 verifies SHA-256, signs in admin-only staging, and deletes the signing key.)"
} else {
    Write-Host "`n=== 4. Test-Signing with Local Developer Certificate ===" -ForegroundColor Cyan
    $cert = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -eq "CN=Audiophile Player Local Test Signing" -and $_.NotAfter -gt (Get-Date) } |
        Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate `
            -Type CodeSigningCert `
            -Subject "CN=Audiophile Player Local Test Signing" `
            -CertStoreLocation Cert:\CurrentUser\My `
            -KeyExportPolicy NonExportable `
            -NotAfter (Get-Date).AddYears(1)
    }
    $cerPath = Join-Path $out "ApExclusiveTestCert.cer"
    Export-Certificate -Cert $cert -FilePath $cerPath | Out-Null

    if ($signtool) {
        & $signtool sign /v /sha1 $cert.Thumbprint /fd sha256 $sysPath
        if ($LASTEXITCODE -ne 0) { throw "signtool (.sys) failed" }
        & $signtool sign /v /sha1 $cert.Thumbprint /fd sha256 $catPath
        if ($LASTEXITCODE -ne 0) { throw "signtool (.cat) failed" }
    } else {
        Set-AuthenticodeSignature -FilePath $sysPath -Certificate $cert -HashAlgorithm SHA256 | Out-Null
        Set-AuthenticodeSignature -FilePath $catPath -Certificate $cert -HashAlgorithm SHA256 | Out-Null
    }
    Write-Host "`n=== Built & Test-Signed in $out ===" -ForegroundColor Green
}

Get-ChildItem $out | Select-Object Name, Length
