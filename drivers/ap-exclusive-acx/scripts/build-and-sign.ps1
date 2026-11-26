# Builds and test-signs ap_exclusive_audio.sys + ap_exclusive_audio.inf + ap_exclusive_audio.cat
# using either the local .wdk\kit cache or a system WDK 10.0.26100 install (no Admin needed).
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$localKit = Join-Path $root ".wdk\kit"
$localExt = Join-Path $root ".wdk\extracted"
$out  = Join-Path $root "out"
$kver = "10.0.26100.0"
$kmdf = "1.31"

if (Test-Path "$localKit\Include\$kver\km\wdm.h") {
    $kit = $localKit
} elseif (Test-Path "C:\Program Files (x86)\Windows Kits\10\Include\$kver\km\wdm.h") {
    $kit = "C:\Program Files (x86)\Windows Kits\10"
} else {
    throw "WDK $kver not found at $localKit or C:\Program Files (x86)\Windows Kits\10"
}

$msvc = @(
    "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Tools\MSVC\14.50.35717",
    "C:\Program Files\Microsoft Visual Studio\2022\Preview\VC\Tools\MSVC\14.38.32919"
) | Where-Object { Test-Path "$_\bin\Hostx64\x64\link.exe" } | Select-Object -First 1
if (-not $msvc) { throw "MSVC x64 link.exe not found" }

$libclang = "C:\Program Files\Microsoft Visual Studio\2022\Preview\VC\Tools\Llvm\x64\bin"
if (-not (Test-Path "$libclang\libclang.dll")) { throw "libclang.dll not found at $libclang" }

$k32 = if (Test-Path "$localExt\microsoft.windows.sdk.cpp.x64\c\um\x64\kernel32.lib") {
    "$localExt\microsoft.windows.sdk.cpp.x64\c\um\x64"
} else { "$kit\Lib\$kver\um\x64" }
$ucrt = if (Test-Path "$localExt\microsoft.windows.sdk.cpp.x64\c\ucrt\x64\ucrt.lib") {
    "$localExt\microsoft.windows.sdk.cpp.x64\c\ucrt\x64"
} else { "$kit\Lib\$kver\ucrt\x64" }

$env:PATH = "$env:USERPROFILE\.cargo\bin;$msvc\bin\Hostx64\x64;$env:PATH"
$env:LIB = "$msvc\lib\x64;$k32;$ucrt"
$env:LIBCLANG_PATH = $libclang
$env:AP_WDK_KIT_ROOT = $kit

Write-Host "=== 1. Building Pure-Rust ACX 1.1 Kernel Driver (ap_exclusive_audio.sys) ===" -ForegroundColor Cyan
cargo +stable-x86_64-pc-windows-msvc build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw "Kernel driver build failed" }

Write-Host "`n=== 2. Packaging driver into $out ===" -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $out | Out-Null
$sysPath = "$out\ap_exclusive_audio.sys"
$infPath = "$out\ap_exclusive_audio.inf"
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
    $bytes[$charsOff] = $cb[0]
    $bytes[$charsOff + 1] = $cb[1]
    [System.IO.File]::WriteAllBytes($sysPath, $bytes)
    & "$msvc\bin\Hostx64\x64\editbin.exe" /NOLOGO /RELEASE $sysPath
    if ($LASTEXITCODE -ne 0) { throw "editbin /RELEASE failed" }
}

Write-Host "`n=== 3. Stamping INF, generating CAT & verifying INF ===" -ForegroundColor Cyan
$stampinf = "$kit\bin\$kver\x64\stampinf.exe"
$inf2cat  = "$kit\bin\$kver\x86\Inf2Cat.exe"
$infverif = "$kit\tools\$kver\x64\infverif.exe"
$signtool = "$kit\bin\$kver\x64\signtool.exe"
if (-not (Test-Path $signtool)) {
    $signtool = "$localExt\microsoft.windows.sdk.buildtools\bin\$kver\x64\signtool.exe"
}

& $stampinf -f $infPath -d * -a amd64 -k $kmdf -v *
if ($LASTEXITCODE -ne 0) { throw "stampinf failed" }

& $inf2cat /driver:"$out" /os:10_VB_X64 /uselocaltime
if ($LASTEXITCODE -ne 0) { throw "Inf2Cat failed" }

& $infverif /v /w $infPath
if ($LASTEXITCODE -ne 0) { Write-Host "InfVerif returned $LASTEXITCODE" -ForegroundColor Yellow }

Write-Host "`n=== 4. Test-Signing with Per-Machine Certificate ===" -ForegroundColor Cyan
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq "CN=Audiophile Player Local Test Signing" -and $_.NotAfter -gt (Get-Date) } |
    Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject "CN=Audiophile Player Local Test Signing" `
        -CertStoreLocation Cert:\CurrentUser\My `
        -NotAfter (Get-Date).AddYears(3)
}
$cerPath = "$out\ApExclusiveTestCert.cer"
Export-Certificate -Cert $cert -FilePath $cerPath | Out-Null

& $signtool sign /v /sha1 $cert.Thumbprint /fd sha256 $sysPath
if ($LASTEXITCODE -ne 0) { throw "signtool (.sys) failed" }
& $signtool sign /v /sha1 $cert.Thumbprint /fd sha256 "$out\ap_exclusive_audio.cat"
if ($LASTEXITCODE -ne 0) { throw "signtool (.cat) failed" }

Write-Host "`n=== Built & Test-Signed in $out ===" -ForegroundColor Green
Get-ChildItem $out | Select-Object Name, Length
