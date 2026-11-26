# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

# Run as Administrator to install Audiophile Player's opt-in Exclusive ACX Virtual Render Driver
# (ROOT\ApExclusiveAudio -> "Line (Audiophile Player)" in Tidal Desktop Exclusive Mode).
#
# Self-contained: works both inside an unpacked GitHub Release zip (driver\install-driver.ps1)
# and inside a source checkout (drivers\ap-exclusive-acx\scripts\install-driver.ps1).
# Requires ZERO WDK, ZERO Visual Studio, and ZERO external tools:
#   - Verifies Windows 11 22H2+ x64, ACX 1.1 runtime, and UEFI Secure Boot BEFORE any system changes
#   - Requires explicit user consent before enabling Windows Test Mode & suspends BitLocker for reboot
#   - Verifies SHA-256 of unsigned release artifacts, stages into an admin-only directory (%ProgramData%)
#   - Generates an ephemeral local test-signing cert, signs in staging, and IMMEDIATELY deletes the private key
#   - Creates ROOT\ApExclusiveAudio via built-in Windows setupapi.dll, installs via pnputil, and verifies DriverVersion
param(
    [switch]$AcceptTestModeRisk,
    [switch]$NoEnableTestSigning,
    [switch]$ForceResign
)
$ErrorActionPreference = "Stop"

$certSubject = "CN=Audiophile Player Local Test Signing"
$regStateKey = "HKLM:\SOFTWARE\AudiophilePlayer\Driver"
$stageDir    = Join-Path $env:ProgramData "AudiophilePlayer\driver"

# Embedded SHA-256 hashes of pristine unsigned release artifacts (populated by package-release.ps1).
# @EMBEDDED_SHA256SUMS_START@
$EmbeddedSha256Sums = @{}
# @EMBEDDED_SHA256SUMS_END@

function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Arguments,
        [int[]]$OkExitCodes = @(0)
    )
    & $Exe @Arguments
    $rc = $LASTEXITCODE
    if ($OkExitCodes -notcontains $rc) {
        throw "'$Exe $($Arguments -join ' ')' failed with exit code $rc"
    }
    return $rc
}

function Set-DriverStateValue([string]$Name, $Value, [string]$Type = "String") {
    if (-not (Test-Path -LiteralPath $regStateKey)) {
        New-Item -Path $regStateKey -Force | Out-Null
    }
    New-ItemProperty -Path $regStateKey -Name $Name -Value $Value -PropertyType $Type -Force | Out-Null
}

function Record-InstalledThumbprint([string]$Thumbprint) {
    if (-not $Thumbprint) { return }
    $existing = @()
    if (Test-Path -LiteralPath $regStateKey) {
        $props = Get-ItemProperty -Path $regStateKey -ErrorAction SilentlyContinue
        if ($props -and $props.CertThumbprints) {
            $existing = @($props.CertThumbprints)
        }
    }
    if ($existing -notcontains $Thumbprint) {
        $existing += $Thumbprint
        Set-DriverStateValue -Name "CertThumbprints" -Value ([string[]]$existing) -Type "MultiString"
    }
}

function Ensure-AdminOnlyDirectory([string]$Dir) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetAccessRuleProtection($true, $false) # disable inheritance, purge inherited ACEs
    $adminSid  = New-Object System.Security.Principal.SecurityIdentifier([System.Security.Principal.WellKnownSidType]::BuiltinAdministratorsSid, $null)
    $systemSid = New-Object System.Security.Principal.SecurityIdentifier([System.Security.Principal.WellKnownSidType]::LocalSystemSid, $null)
    $flags     = [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [System.Security.AccessControl.InheritanceFlags]::ObjectInherit
    $prop      = [System.Security.AccessControl.PropagationFlags]::None
    $full      = [System.Security.AccessControl.FileSystemRights]::FullControl
    $allow     = [System.Security.AccessControl.AccessControlType]::Allow
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($adminSid,  $full, $flags, $prop, $allow)))
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule($systemSid, $full, $flags, $prop, $allow)))
    Set-Acl -LiteralPath $Dir -AclObject $acl
}

function Verify-ArtifactHashes([string]$Dir, [hashtable]$Expected) {
    if ($Expected.Count -eq 0) { return }
    foreach ($rel in $Expected.Keys) {
        $fullPath = Join-Path $Dir $rel
        if (-not (Test-Path -LiteralPath $fullPath)) {
            throw "Integrity check failed: required file '$rel' missing in $Dir"
        }
        $actual = (Get-FileHash -Algorithm SHA256 -Path $fullPath).Hash.ToLowerInvariant()
        $want   = $Expected[$rel].ToLowerInvariant()
        if ($actual -ne $want) {
            throw "Integrity check FAILED for '$rel' in $Dir!`n  Expected: $want`n  Actual  : $actual`nRefusing to sign or install modified driver files."
        }
    }
}

function Import-ValidatedPublicCert([string]$CerPath) {
    $x509 = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($CerPath)
    if ($x509.Subject -ne $certSubject) {
        throw "Refusing to trust '$CerPath': unexpected Subject '$($x509.Subject)' (expected '$certSubject')"
    }
    if ($x509.Issuer -ne $x509.Subject) {
        throw "Refusing to trust '$CerPath': certificate is not self-issued"
    }
    $ekuExt = $x509.Extensions | Where-Object { $_.Oid.Value -eq "2.5.29.37" } | Select-Object -First 1
    $ekus = @()
    if ($ekuExt) {
        $ekuObj = New-Object System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension($ekuExt, $false)
        $ekus = @($ekuObj.EnhancedKeyUsages | ForEach-Object { $_.Value })
    }
    if ($ekus.Count -ne 1 -or $ekus[0] -ne "1.3.6.1.5.5.7.3.3") {
        throw "Refusing to trust '$CerPath': EKU must be strictly Code Signing (1.3.6.1.5.5.7.3.3), found: $($ekus -join ', ')"
    }
    Import-Certificate -FilePath $CerPath -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
    Import-Certificate -FilePath $CerPath -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
    Record-InstalledThumbprint $x509.Thumbprint
    return $x509.Thumbprint
}

# === 0. Preflight checks (BEFORE any system modification - Review #8 D1, D5) ===
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
    exit 1
}

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "Unsupported architecture '$env:PROCESSOR_ARCHITECTURE': ap_exclusive_audio.sys supports Windows 11 x64 (AMD64) only."
}
$osBuild = [Environment]::OSVersion.Version.Build
if ($osBuild -lt 22621) {
    throw "Unsupported Windows build ${osBuild}: Windows 11 22H2+ (Build 22621+) is required for ACX 1.1."
}
if (-not (Test-Path -LiteralPath "$env:SystemRoot\System32\drivers\Acx01000.sys")) {
    throw "Windows ACX 1.1 runtime ($env:SystemRoot\System32\drivers\Acx01000.sys) is not present on this system."
}

# Check Secure Boot BEFORE changing BCD, BitLocker, or certificate stores (Review #8 D1).
$secureBootOn = $false
try {
    if (Get-Command Confirm-SecureBootUEFI -ErrorAction SilentlyContinue) {
        $secureBootOn = [bool](Confirm-SecureBootUEFI)
    }
} catch {
    # Legacy BIOS or unsupported platform: Confirm-SecureBootUEFI throws; treat as off.
    $secureBootOn = $false
}
if ($secureBootOn) {
    Write-Host "`nERROR: UEFI Secure Boot is currently ENABLED." -ForegroundColor Red
    Write-Host "Windows will not allow enabling Test Mode or loading locally test-signed kernel drivers while Secure Boot is ON." -ForegroundColor Yellow
    Write-Host "Before disabling Secure Boot in BIOS/UEFI:" -ForegroundColor Yellow
    Write-Host "  1. If BitLocker / Windows Device Encryption is enabled, back up your 48-digit recovery key first (https://aka.ms/myrecoverykey) and suspend BitLocker."
    Write-Host "  2. Reboot into BIOS/UEFI firmware settings and disable Secure Boot."
    Write-Host "  3. Boot back into Windows and re-run this script as Administrator."
    exit 1
}

# Auto-detect driver bundle directory (release zip: $PSScriptRoot; repo checkout: ..\out)
if ((Test-Path (Join-Path $PSScriptRoot "ap_exclusive_audio.sys")) -and (Test-Path (Join-Path $PSScriptRoot "ap_exclusive_audio.inf"))) {
    $pkgDir = $PSScriptRoot
} elseif (Test-Path (Join-Path (Split-Path -Parent $PSScriptRoot) "out\ap_exclusive_audio.sys")) {
    $pkgDir = Join-Path (Split-Path -Parent $PSScriptRoot) "out"
} else {
    throw "Driver files (ap_exclusive_audio.sys / .inf / .cat) not found next to this script or in ..\out. Run build-and-sign.ps1 first if building from source."
}

$srcSys = Join-Path $pkgDir "ap_exclusive_audio.sys"
$srcInf = Join-Path $pkgDir "ap_exclusive_audio.inf"
$srcCat = Join-Path $pkgDir "ap_exclusive_audio.cat"
if (-not (Test-Path -LiteralPath $srcCat)) {
    throw "Catalog file missing: $srcCat"
}

# Load expected hashes from embedded release manifest or SHA256SUMS sidecar (Review #8 D2).
$expectedHashes = @{}
foreach ($k in $EmbeddedSha256Sums.Keys) { $expectedHashes[$k] = $EmbeddedSha256Sums[$k] }
$shaFile = Join-Path $pkgDir "SHA256SUMS"
if ($expectedHashes.Count -eq 0 -and (Test-Path -LiteralPath $shaFile)) {
    foreach ($line in Get-Content -LiteralPath $shaFile) {
        $trimmed = $line.Trim()
        if ($trimmed -match '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
            $expectedHashes[$Matches[2].Trim()] = $Matches[1].ToLowerInvariant()
        }
    }
}

# Check Windows Test Mode with explicit user consent & BitLocker suspension (Review #8 D1).
$sso = (Get-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control" -Name "SystemStartOptions").SystemStartOptions
if ($sso -notmatch "TESTSIGNING") {
    if ($NoEnableTestSigning) {
        throw "Windows Test Mode is not enabled. Enable it with 'bcdedit /set testsigning on' (Secure Boot must be OFF), reboot once, and re-run."
    }
    Write-Host "`n=== Security Notice: Windows Test Mode Required ===" -ForegroundColor Yellow
    Write-Host "Installing a locally test-signed kernel driver requires enabling Windows Test Mode ('bcdedit /set testsigning on'):"
    Write-Host "  * Kernel code integrity is weakened machine-wide: ANY test-signed driver can load while Test Mode is active."
    Write-Host "  * A self-signed code-signing certificate is added to LocalMachine\Root & TrustedPublisher (its private key is deleted immediately after signing, and uninstall removes the certificate)."
    Write-Host "  * Kernel anti-cheat software (e.g. Vanguard, FACEIT, EA AC) and some DRM paths may refuse to run in Test Mode."
    Write-Host "  * Windows displays a 'Test Mode' watermark in the bottom-right corner."
    Write-Host "  * You can revert completely at any time by running 'uninstall-driver.ps1' as Administrator."

    if (-not $AcceptTestModeRisk) {
        $reply = Read-Host "`nType YES (all caps) to enable Windows Test Mode and continue"
        if ($reply -cne "YES") {
            Write-Host "Aborted by user. No system changes were made." -ForegroundColor Yellow
            exit 1
        }
    }

    # Detect BitLocker and suspend for 2 reboots so BCD edit does not trigger a recovery-key lockout!
    $bl = Get-BitLockerVolume -MountPoint $env:SystemDrive -ErrorAction SilentlyContinue
    if ($bl -and $bl.ProtectionStatus -eq "On") {
        Write-Warning "BitLocker / Device Encryption is ON for $env:SystemDrive. Ensure your 48-digit recovery key is backed up at https://aka.ms/myrecoverykey."
        Write-Host "Suspending BitLocker for 2 reboots so the Test Mode BCD change does not prompt for recovery..." -ForegroundColor Cyan
        Suspend-BitLocker -MountPoint $env:SystemDrive -RebootCount 2 | Out-Null
    }

    Write-Host "Enabling Test Mode: bcdedit /set testsigning on" -ForegroundColor Cyan
    Invoke-Native -Exe "bcdedit.exe" -Arguments @("/set", "testsigning", "on") -OkExitCodes @(0) | Out-Null
    Set-DriverStateValue -Name "EnabledTestSigning" -Value 1 -Type "DWord"
    Write-Host "`nTest Mode enabled! Please REBOOT WINDOWS once, then re-run this script as Administrator." -ForegroundColor Green
    exit 3010
}

# Clean up any leftover prototype ROOT\ApTidalTest device so Tidal only sees "Audiophile Player"
$oldDevs = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApTidalTest" }
foreach ($d in $oldDevs) {
    Write-Host "Removing previous prototype device $($d.InstanceId)..." -ForegroundColor Yellow
    Invoke-Native -Exe "pnputil.exe" -Arguments @("/remove-device", $d.InstanceId) -OkExitCodes @(0, 259, 3010) | Out-Null
}
$oldInfs = pnputil /enum-drivers | Select-String -Pattern "ap_tidal_test\.inf" -Context 3,0
foreach ($m in $oldInfs) {
    $block = ($m.Context.PreContext -join "`n")
    if ($block -match "(oem\d+\.inf)") {
        Invoke-Native -Exe "pnputil.exe" -Arguments @("/delete-driver", $Matches[1], "/uninstall", "/force") -OkExitCodes @(0, 259, 3010) | Out-Null
    }
}

Write-Host "=== 1. Verifying integrity & staging into admin-only directory ($stageDir) ===" -ForegroundColor Cyan
$srcSigSys = Get-AuthenticodeSignature -FilePath $srcSys
$srcSigCat = Get-AuthenticodeSignature -FilePath $srcCat
$srcIsUnsigned = ($srcSigSys.Status -eq "NotSigned" -and $srcSigCat.Status -eq "NotSigned")

if ($expectedHashes.Count -gt 0 -and $srcIsUnsigned) {
    Verify-ArtifactHashes -Dir $pkgDir -Expected $expectedHashes
    Write-Host "SHA-256 integrity verified for unsigned release artifacts." -ForegroundColor Green
} elseif ($expectedHashes.Count -gt 0 -and -not $srcIsUnsigned) {
    Write-Host "Source artifacts already signed; verifying INF hash..." -ForegroundColor Yellow
    if ($expectedHashes.ContainsKey("ap_exclusive_audio.inf")) {
        Verify-ArtifactHashes -Dir $pkgDir -Expected @{ "ap_exclusive_audio.inf" = $expectedHashes["ap_exclusive_audio.inf"] }
    }
}

Ensure-AdminOnlyDirectory $stageDir
$sysPath = Join-Path $stageDir "ap_exclusive_audio.sys"
$infPath = Join-Path $stageDir "ap_exclusive_audio.inf"
$catPath = Join-Path $stageDir "ap_exclusive_audio.cat"
Copy-Item -Force -LiteralPath $srcSys -Destination $sysPath
Copy-Item -Force -LiteralPath $srcInf -Destination $infPath
Copy-Item -Force -LiteralPath $srcCat -Destination $catPath
if ($expectedHashes.Count -gt 0 -and $srcIsUnsigned) {
    # Re-verify inside the admin-only directory to close any TOCTOU window (Review #8 D2).
    Verify-ArtifactHashes -Dir $stageDir -Expected $expectedHashes
}

# Extract expected DriverVer from INF for post-install binding verification (Review #8 D8).
$infContent = Get-Content -LiteralPath $infPath -Raw
$expectedInfVer = $null
if ($infContent -match '(?m)^DriverVer\s*=\s*[^,\r\n]+,\s*([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)') {
    $expectedInfVer = $Matches[1]
}

Write-Host "`n=== 2. Local test-signing (ephemeral key - private key deleted immediately) ===" -ForegroundColor Cyan
$cerFile = Join-Path $pkgDir "ApExclusiveTestCert.cer"
if ((Test-Path -LiteralPath $cerFile) -and -not $ForceResign) {
    $devThumb = Import-ValidatedPublicCert $cerFile
    Write-Host "Validated & trusted local developer certificate (Thumbprint: $devThumb)." -ForegroundColor Green
}

$sigSys = Get-AuthenticodeSignature -FilePath $sysPath
$sigCat = Get-AuthenticodeSignature -FilePath $catPath

if ($ForceResign -or $sigSys.Status -ne "Valid" -or $sigCat.Status -ne "Valid") {
    Write-Host "Creating ephemeral per-install self-signed code-signing certificate..."
    $cert = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject $certSubject `
        -CertStoreLocation Cert:\LocalMachine\My `
        -KeyExportPolicy NonExportable `
        -NotAfter (Get-Date).AddYears(1)

    $tmpCer = Join-Path $stageDir ("ApExclusiveTestCert-" + [guid]::NewGuid().ToString("N") + ".cer")
    try {
        Export-Certificate -Cert $cert -FilePath $tmpCer | Out-Null
        Import-ValidatedPublicCert $tmpCer | Out-Null

        $rSys = Set-AuthenticodeSignature -FilePath $sysPath -Certificate $cert -HashAlgorithm SHA256
        $rCat = Set-AuthenticodeSignature -FilePath $catPath -Certificate $cert -HashAlgorithm SHA256
        if ($rSys.Status -ne "Valid") {
            throw "Failed to sign ap_exclusive_audio.sys: $($rSys.Status) ($($rSys.StatusMessage))"
        }
        if ($rCat.Status -ne "Valid") {
            throw "Failed to sign ap_exclusive_audio.cat: $($rCat.Status) ($($rCat.StatusMessage))"
        }
        Write-Host "Signed in admin staging (Thumbprint: $($cert.Thumbprint))." -ForegroundColor Green
    } finally {
        Remove-Item -Force -LiteralPath $tmpCer -ErrorAction SilentlyContinue
        # Review #8 D3: Delete the private key IMMEDIATELY after signing so zero signing key stays on disk!
        Remove-Item -LiteralPath "Cert:\LocalMachine\My\$($cert.Thumbprint)" -DeleteKey -Force -ErrorAction SilentlyContinue
    }
    Write-Host "Deleted ephemeral private key from LocalMachine\My (only public cert remains in Root/TrustedPublisher)." -ForegroundColor Green
} else {
    Write-Host "Staged driver artifacts carry a valid trusted test signature." -ForegroundColor Green
}

Write-Host "`n=== 3. Creating ROOT\ApExclusiveAudio virtual audio endpoint ===" -ForegroundColor Cyan
$existing = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" }
if (-not $existing) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class ApRootDeviceHelper {
    private const uint DICD_GENERATE_ID   = 0x00000001;
    private const uint SPDRP_HARDWAREID   = 0x00000001;
    private const uint DIF_REGISTERDEVICE = 0x00000019;

    [StructLayout(LayoutKind.Sequential)]
    private struct SP_DEVINFO_DATA {
        public uint   cbSize;
        public Guid   ClassGuid;
        public uint   DevInst;
        public IntPtr Reserved;
    }

    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid classGuid, IntPtr hwndParent);

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool SetupDiCreateDeviceInfoW(
        IntPtr deviceInfoSet,
        string deviceName,
        ref Guid classGuid,
        string deviceDescription,
        IntPtr hwndParent,
        uint creationFlags,
        ref SP_DEVINFO_DATA deviceInfoData);

    [DllImport("setupapi.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool SetupDiSetDeviceRegistryPropertyW(
        IntPtr deviceInfoSet,
        ref SP_DEVINFO_DATA deviceInfoData,
        uint property,
        byte[] propertyBuffer,
        uint propertyBufferSize);

    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiCallClassInstaller(
        uint installFunction,
        IntPtr deviceInfoSet,
        ref SP_DEVINFO_DATA deviceInfoData);

    [DllImport("setupapi.dll", SetLastError = true)]
    private static extern bool SetupDiDestroyDeviceInfoList(IntPtr deviceInfoSet);

    public static void CreateRootDevice(string hardwareId, Guid classGuid, string className) {
        IntPtr hDevInfo = SetupDiCreateDeviceInfoList(ref classGuid, IntPtr.Zero);
        if (hDevInfo == IntPtr.Zero || hDevInfo == new IntPtr(-1)) {
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "SetupDiCreateDeviceInfoList failed");
        }
        try {
            var devData = new SP_DEVINFO_DATA { cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVINFO_DATA)) };
            if (!SetupDiCreateDeviceInfoW(hDevInfo, className, ref classGuid, null, IntPtr.Zero, DICD_GENERATE_ID, ref devData)) {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "SetupDiCreateDeviceInfoW failed");
            }
            byte[] multiSz = Encoding.Unicode.GetBytes(hardwareId + "\0\0");
            if (!SetupDiSetDeviceRegistryPropertyW(hDevInfo, ref devData, SPDRP_HARDWAREID, multiSz, (uint)multiSz.Length)) {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "SetupDiSetDeviceRegistryPropertyW(SPDRP_HARDWAREID) failed");
            }
            if (!SetupDiCallClassInstaller(DIF_REGISTERDEVICE, hDevInfo, ref devData)) {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "SetupDiCallClassInstaller(DIF_REGISTERDEVICE) failed");
            }
        } finally {
            SetupDiDestroyDeviceInfoList(hDevInfo);
        }
    }
}
"@
    $mediaClassGuid = [Guid]"4d36e96c-e325-11ce-bfc1-08002be10318"
    [ApRootDeviceHelper]::CreateRootDevice("ROOT\ApExclusiveAudio", $mediaClassGuid, "MEDIA")
    Write-Host "Created root-enumerated device ROOT\ApExclusiveAudio."
    Start-Sleep -Seconds 1
} else {
    Write-Host "ROOT\ApExclusiveAudio already exists; updating driver binding."
}

Write-Host "`n=== 4. Installing ap_exclusive_audio.inf & binding to ROOT\ApExclusiveAudio ===" -ForegroundColor Cyan
$rebootRequired = $false
$rcAdd = Invoke-Native -Exe "pnputil.exe" -Arguments @("/add-driver", $infPath, "/install") -OkExitCodes @(0, 259, 3010)
if ($rcAdd -eq 3010) { $rebootRequired = $true }
Start-Sleep -Seconds 1

$existing = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" }
foreach ($d in $existing) {
    $rcRes = Invoke-Native -Exe "pnputil.exe" -Arguments @("/restart-device", $d.InstanceId) -OkExitCodes @(0, 259, 3010)
    if ($rcRes -eq 3010) { $rebootRequired = $true }
}
Start-Sleep -Seconds 1

Write-Host "`n=== 5. Verifying device & bound DriverVersion ===" -ForegroundColor Cyan
$mediaDevs = @(Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" })
$mediaDevs | Format-Table -AutoSize Status, Class, FriendlyName, InstanceId

if ($rebootRequired) {
    Write-Host "`nDriver staged - REBOOT REQUIRED (exit 3010) because a process held the audio device open." -ForegroundColor Yellow
    exit 3010
}

$okDev = $mediaDevs | Where-Object { $_.Status -eq "OK" } | Select-Object -First 1
if (-not $okDev) {
    throw "ROOT\ApExclusiveAudio did not reach Status=OK. Check C:\Windows\INF\setupapi.dev.log."
}

# Review #8 D8: Verify the actually bound DriverVersion matches our INF DriverVer!
if ($expectedInfVer) {
    $boundProp = Get-PnpDeviceProperty -InstanceId $okDev.InstanceId -KeyName "DEVPKEY_Device_DriverVersion" -ErrorAction SilentlyContinue
    $boundVer = if ($boundProp) { [string]$boundProp.Data } else { "" }
    Write-Host "Bound DriverVersion: $boundVer (expected: $expectedInfVer)"
    if ($boundVer -and $boundVer -ne $expectedInfVer) {
        throw "Bound driver version ($boundVer) does NOT match expected package version ($expectedInfVer). Run uninstall-driver.ps1 first, then re-run install-driver.ps1."
    }
}

Write-Host "`nInstalled successfully!" -ForegroundColor Green
Write-Host "Next steps:"
Write-Host "  1. If Tidal Desktop is already open, restart it once so it discovers the new endpoint."
Write-Host "  2. In Tidal Desktop -> Sound output, select 'Line (Audiophile Player)'."
Write-Host "  3. Enable 'Use Exclusive Mode' (and leave 'Force volume' OFF)."
Write-Host "  4. Start Audiophile Player and press Play in Tidal Desktop."
exit 0
