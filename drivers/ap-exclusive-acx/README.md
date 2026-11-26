# Audiophile Player Exclusive ACX Virtual Render Driver (`ap_exclusive_audio.sys`)

Opt-in pure-Rust (`#![no_std]`) **KMDF 1.31 + ACX 1.1** virtual render audio endpoint (`ROOT\ApExclusiveAudio`) for **Windows 11 22H2+ (Build 22621+) x64**.

It exposes a stereo integer-PCM virtual endpoint (**`Line (Audiophile Player)`**) that accepts **Tidal Desktop** (or any WASAPI Exclusive player) with **zero Windows mixer anywhere in the path**, forwarding raw sample-exact PCM over `\\.\ApExclusiveAudio` into `audiophile-player`'s pure-`f64` 8fS upsampler, REW room convolver, noise-shaped quantizer, and WDM-KS DAC output.

## Zero-Mixer Kernel Guarantee & Access Control

- **32 integer PCM formats only** (`44.1 / 48 / 88.2 / 96 / 176.4 / 192 / 352.8 / 384 kHz` × `16 / 24 / 24-in-32 / 32-bit` stereo). Zero `IEEE_FLOAT` formats advertised.
- **Zero APOs, zero volume/mute elements** in the ACX circuit (`AUDIO_SIGNALPROCESSINGMODE_RAW` only).
- **Permanent kernel gate against `audiodg.exe` & system callers:** on Windows 11, WASAPI Exclusive uses `audiodg.exe` briefly to broker `CreateStream` before handing the KS pin directly to the exclusive application (`TIDALPlayer.exe`), whereas Shared Mode runs `Run` and `SetRenderPacket` inside `audiodg.exe`. The driver unconditionally returns `STATUS_ACCESS_DENIED` whenever `audiodg.exe`, `System`, `Idle`, or PID ≤ 4 attempts `Run` or `SetRenderPacket` (cannot be disabled via IOCTL), and `audiophile-player` double-checks caller names and `is_float == 0` in user mode.
- **Per-session isolation:** `\Device\ApExclusiveAudio` enforces Medium-IL+ (`S:(ML;;NWNRNX;;;ME)`) and matches the control reader's Windows session ID against the session owning the active render stream so another logged-in user session cannot eavesdrop on your PCM.

## Security Implications (Read Before Installing)

- **Windows Test Mode & Secure Boot:** Because this is a community open-source kernel driver without a paid EV/WHQL certificate, installation is **strictly opt-in** and requires **UEFI Secure Boot OFF** and **Windows Test Mode ON** (`bcdedit /set testsigning on`). While Test Mode is enabled, Windows allows **any** test-signed kernel driver to load, shows a desktop watermark, and some kernel anti-cheat systems (Vanguard, FACEIT, EA AC) or DRM paths will refuse to run.
- **BitLocker / Device Encryption:** Turning off Secure Boot (PCR7) and editing `testsigning` in BCD can trigger a BitLocker recovery-key prompt on boot. Back up your 48-digit recovery key ([aka.ms/myrecoverykey](https://aka.ms/myrecoverykey)) before changing UEFI settings. `install-driver.ps1` detects BitLocker and automatically suspends it for 2 reboots before modifying BCD.
- **Certificate trust & ephemeral private key:** `install-driver.ps1` verifies the SHA-256 hashes of the unsigned release `.sys`/`.inf`/`.cat` files, copies them to an admin-only staging directory (`%ProgramData%\AudiophilePlayer\driver`), generates an ephemeral self-signed code-signing certificate, adds the **public** certificate to `LocalMachine\Root` and `LocalMachine\TrustedPublisher`, signs the staged binaries, and **immediately deletes the private key** (`Remove-Item -DeleteKey`). No third-party private keys or pre-signed certificates are ever distributed.

## Installing (From a Release Zip — Zero WDK Required)

Extract the verified release archive into an admin-protected directory (e.g. `C:\Program Files\AudiophilePlayer`), unblock extracted files (`Get-ChildItem -Recurse | Unblock-File`), and open an **Administrator PowerShell** in `driver\` (or `drivers\ap-exclusive-acx\scripts\` in a source checkout):

```powershell
powershell -ExecutionPolicy RemoteSigned -File .\install-driver.ps1
```

1. Preflight checks verify **Windows 11 22H2+ x64**, `Acx01000.sys`, and **Secure Boot OFF** before making any system changes.
2. If Windows Test Mode is off, the script prints the security notice, asks for explicit `YES` confirmation (or `-AcceptTestModeRisk`), suspends BitLocker if active, enables Test Mode, records an install-state marker in `HKLM:\SOFTWARE\AudiophilePlayer\Driver`, and exits `3010` asking you to **reboot once**, then re-run the same command.
3. On the second run (or immediately if Test Mode is already on), the script verifies SHA-256 integrity, stages into `%ProgramData%\AudiophilePlayer\driver`, signs with an ephemeral local certificate (deleting the private key immediately), creates `ROOT\ApExclusiveAudio`, binds the driver via `pnputil`, and verifies the bound `DriverVersion`.
4. Restart Tidal Desktop (if it was already open), choose **`Line (Audiophile Player)`**, turn **Use Exclusive Mode ON**, and leave **Force volume OFF**.

## Updating & Uninstalling

In an **Administrator PowerShell**:

```powershell
# Hot-update an already-installed driver without rebooting (when Tidal & Audiophile Player are closed):
# From a release zip (inside driver\):
powershell -ExecutionPolicy RemoteSigned -File .\update-driver.ps1
# From a source checkout:
powershell -ExecutionPolicy RemoteSigned -File drivers\ap-exclusive-acx\scripts\update-driver.ps1

# Uninstall the virtual device, Driver Store package, staging dir, and local test certificate.
# Automatically turns Test Mode OFF if install-driver.ps1 enabled it (pass -KeepTestSigning to keep it on):
powershell -ExecutionPolicy RemoteSigned -File .\uninstall-driver.ps1

# Force disabling Windows Test Mode even if it was enabled manually beforehand:
powershell -ExecutionPolicy RemoteSigned -File .\uninstall-driver.ps1 -DisableTestSigning
```

**What uninstall reverts:** removes `ROOT\ApExclusiveAudio`, deletes its `oem*.inf` Driver Store package, deletes `%ProgramData%\AudiophilePlayer\driver`, purges recorded test certificates (and any private keys via `-DeleteKey`), and turns `testsigning` off if installed by `install-driver.ps1`. **What uninstall cannot revert automatically:** UEFI Secure Boot must be re-enabled manually in your BIOS/UEFI firmware after rebooting (suspend BitLocker / keep your recovery key handy before toggling Secure Boot back on).

## Building From Source

Requires:
- Rust `stable-x86_64-pc-windows-msvc`
- Visual Studio C++ x64 Build Tools (`link.exe`, `editbin.exe`)
- `libclang.dll` (Visual Studio Clang tools or `winget install LLVM.LLVM`)
- Windows Driver Kit `10.0.26100` (either system-installed or auto-fetched locally into `.wdk\` via `-FetchWdk` with pinned SHA-512 verification and Microsoft Authenticode checks)

```powershell
# Developer build + local test-signing (auto-fetches SHA-512-verified WDK NuGet packages if not installed):
powershell -ExecutionPolicy RemoteSigned -File drivers\ap-exclusive-acx\scripts\build-and-sign.ps1 -FetchWdk

# Unsigned build for release packaging (fatal InfVerif check; never embeds a personal cert):
powershell -ExecutionPolicy RemoteSigned -File drivers\ap-exclusive-acx\scripts\build-and-sign.ps1 -Unsigned
```

## Licensing & Attribution

Licensed under [GPL-3.0-or-later](../../LICENSE) with a GPLv3 Section 7 additional permission for linking with Microsoft WDK/KMDF/ACX static libraries. See [NOTICE](NOTICE) for the full Section 7 permission and complete MIT license texts for referenced ACX ABI projects.
