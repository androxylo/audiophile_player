# Run as Administrator to install Audiophile Player's opt-in Exclusive ACX Virtual Render Driver
# (ROOT\ApExclusiveAudio -> "Line (Audiophile Player)" in Tidal Desktop Exclusive Mode).
#
# Self-contained: works both inside an unpacked GitHub Release zip (driver\install-driver.ps1)
# and inside a source checkout (drivers\ap-exclusive-acx\scripts\install-driver.ps1).
# Requires ZERO WDK, ZERO Visual Studio, and ZERO external tools:
#   - Generates a per-machine self-signed code-signing certificate locally (if needed)
#   - Signs ap_exclusive_audio.sys and ap_exclusive_audio.cat locally with SHA-256
#   - Creates ROOT\ApExclusiveAudio via built-in Windows setupapi.dll
#   - Installs and binds the driver via built-in pnputil.exe
param(
    [switch]$NoEnableTestSigning
)
$ErrorActionPreference = "Stop"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "ERROR: Please run this script from an Administrator PowerShell window." -ForegroundColor Red
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

$sysPath = Join-Path $pkgDir "ap_exclusive_audio.sys"
$infPath = Join-Path $pkgDir "ap_exclusive_audio.inf"
$catPath = Join-Path $pkgDir "ap_exclusive_audio.cat"
if (-not (Test-Path $catPath)) {
    throw "Catalog file missing: $catPath"
}

# 0. Check Windows Test Mode (required for locally test-signed kernel drivers)
$sso = (Get-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control" -Name "SystemStartOptions").SystemStartOptions
if ($sso -notmatch "TESTSIGNING") {
    if ($NoEnableTestSigning) {
        throw "Windows Test Mode is not enabled. Enable it with 'bcdedit /set testsigning on' (Secure Boot must be OFF), reboot once, and re-run."
    }
    Write-Host "Windows Test Mode is currently OFF (required for locally test-signed drivers)." -ForegroundColor Yellow
    Write-Host "Enabling Test Mode: bcdedit /set testsigning on" -ForegroundColor Cyan
    bcdedit /set testsigning on
    if ($LASTEXITCODE -ne 0) {
        throw "bcdedit failed. Ensure UEFI Secure Boot is disabled in BIOS/UEFI, then re-run this script as Administrator."
    }
    Write-Host "`nTest Mode enabled! Please REBOOT WINDOWS once, then re-run this script as Administrator." -ForegroundColor Green
    exit 0
}

# Clean up any leftover prototype ROOT\ApTidalTest device so Tidal only sees "Audiophile Player"
$oldDevs = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApTidalTest" }
foreach ($d in $oldDevs) {
    Write-Host "Removing previous prototype device $($d.InstanceId)..." -ForegroundColor Yellow
    pnputil /remove-device $d.InstanceId | Out-Null
}
$oldInfs = pnputil /enum-drivers | Select-String -Pattern "ap_tidal_test\.inf" -Context 3,0
foreach ($m in $oldInfs) {
    $block = ($m.Context.PreContext -join "`n")
    if ($block -match "(oem\d+\.inf)") {
        pnputil /delete-driver $Matches[1] /uninstall /force | Out-Null
    }
}

Write-Host "=== 1. Ensuring per-machine local test-signing certificate & SHA-256 signatures ===" -ForegroundColor Cyan
$certSubject = "CN=Audiophile Player Local Test Signing"
$cerFile     = Join-Path $pkgDir "ApExclusiveTestCert.cer"

function Ensure-TrustedCertFile([string]$Path) {
    Import-Certificate -FilePath $Path -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
    Import-Certificate -FilePath $Path -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
}

# If developer build-and-sign.ps1 already produced a matching .cer, trust it first.
if (Test-Path $cerFile) {
    Ensure-TrustedCertFile $cerFile
}

$sigSys = Get-AuthenticodeSignature -FilePath $sysPath
$sigCat = Get-AuthenticodeSignature -FilePath $catPath

if ($sigSys.Status -ne "Valid" -or $sigCat.Status -ne "Valid") {
    Write-Host "Signing driver artifacts locally with a per-machine self-signed certificate..."
    $cert = Get-ChildItem Cert:\LocalMachine\My |
        Where-Object { $_.Subject -eq $certSubject -and $_.HasPrivateKey -and $_.NotAfter -gt (Get-Date).AddDays(7) } |
        Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate `
            -Type CodeSigningCert `
            -Subject $certSubject `
            -CertStoreLocation Cert:\LocalMachine\My `
            -KeyExportPolicy NonExportable `
            -NotAfter (Get-Date).AddYears(3)
    }

    $tmpCer = Join-Path $env:TEMP ("ApExclusiveTestCert-" + [guid]::NewGuid().ToString("N") + ".cer")
    try {
        Export-Certificate -Cert $cert -FilePath $tmpCer | Out-Null
        Ensure-TrustedCertFile $tmpCer
    } finally {
        Remove-Item -Force $tmpCer -ErrorAction SilentlyContinue
    }

    $rSys = Set-AuthenticodeSignature -FilePath $sysPath -Certificate $cert -HashAlgorithm SHA256
    $rCat = Set-AuthenticodeSignature -FilePath $catPath -Certificate $cert -HashAlgorithm SHA256
    if ($rSys.Status -ne "Valid") {
        throw "Failed to sign ap_exclusive_audio.sys: $($rSys.Status) ($($rSys.StatusMessage))"
    }
    if ($rCat.Status -ne "Valid") {
        throw "Failed to sign ap_exclusive_audio.cat: $($rCat.Status) ($($rCat.StatusMessage))"
    }
    Write-Host "Signed locally (Thumbprint: $($cert.Thumbprint))." -ForegroundColor Green
} else {
    Write-Host "Driver artifacts already carry a valid trusted test signature." -ForegroundColor Green
}

Write-Host "`n=== 2. Creating ROOT\ApExclusiveAudio virtual audio endpoint ===" -ForegroundColor Cyan
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

Write-Host "`n=== 3. Installing ap_exclusive_audio.inf & binding to ROOT\ApExclusiveAudio ===" -ForegroundColor Cyan
pnputil /add-driver $infPath /install
if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 259) {
    throw "pnputil /add-driver /install failed with exit code $LASTEXITCODE"
}
Start-Sleep -Seconds 1
$existing = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" }
foreach ($d in $existing) {
    pnputil /restart-device $d.InstanceId | Out-Null
}
Start-Sleep -Seconds 1

Write-Host "`n=== 4. Verifying device ===" -ForegroundColor Cyan
$mediaDevs = @(Get-PnpDevice -Class MEDIA -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains "ROOT\ApExclusiveAudio" })
$mediaDevs | Format-Table -AutoSize Status, Class, FriendlyName, InstanceId
if ($mediaDevs.Count -eq 0 -or ($mediaDevs | Where-Object { $_.Status -eq "OK" }).Count -eq 0) {
    throw "ROOT\ApExclusiveAudio did not reach Status=OK. Check C:\Windows\INF\setupapi.dev.log."
}

Write-Host "`nInstalled successfully!" -ForegroundColor Green
Write-Host "Next steps:"
Write-Host "  1. If Tidal Desktop is already open, restart it once so it discovers the new endpoint."
Write-Host "  2. In Tidal Desktop -> Sound output, select 'Line (Audiophile Player)'."
Write-Host "  3. Enable 'Use Exclusive Mode' (and leave 'Force volume' OFF)."
Write-Host "  4. Start Audiophile Player and press Play in Tidal Desktop."
