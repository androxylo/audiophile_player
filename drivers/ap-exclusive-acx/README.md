# Audiophile Player Exclusive ACX Virtual Render Driver (`ap_exclusive_audio.sys`)

Opt-in pure-Rust (`#![no_std]`) **KMDF 1.31 + ACX 1.1** virtual render audio endpoint (`ROOT\ApExclusiveAudio`) for Windows 11 x64.

It exposes a stereo integer-PCM virtual endpoint (**`Line (Audiophile Player)`**) that accepts **Tidal Desktop** (or any WASAPI Exclusive player) with **zero Windows mixer anywhere in the path**, forwarding raw sample-exact PCM over `\\.\ApExclusiveAudio` into `audiophile-player`'s pure-`f64` 8fS upsampler, REW room convolver, noise-shaped quantizer, and WDM-KS DAC output.

## Zero-Mixer Kernel Guarantee

- **32 integer PCM formats only** (`44.1 / 48 / 88.2 / 96 / 176.4 / 192 / 352.8 / 384 kHz` × `16 / 24 / 24-in-32 / 32-bit` stereo). Zero `IEEE_FLOAT` formats advertised.
- **Zero APOs, zero volume/mute elements** in the ACX circuit (`AUDIO_SIGNALPROCESSINGMODE_RAW` only).
- **Hard kernel gate against `audiodg.exe`**: on Windows 11, WASAPI Exclusive uses `audiodg.exe` briefly to broker `CreateStream` before handing the KS pin directly to the exclusive application (`TIDALPlayer.exe`), whereas Shared Mode runs `Run` and `SetRenderPacket` inside `audiodg.exe`. The driver returns `STATUS_ACCESS_DENIED` whenever `audiodg.exe` attempts `Run` or `SetRenderPacket`, and `audiophile-player` double-checks caller names and `is_float == 0` in user mode.

## Installing (From a Release Zip — Zero WDK Required)

> **Prerequisites & trade-offs:** Because this is a community open-source kernel driver without a paid EV/WHQL certificate, installation is **strictly opt-in** and requires **Windows Test Mode** (UEFI Secure Boot must be disabled). `install-driver.ps1` generates a **per-machine self-signed certificate locally** on your PC — no private keys or developer certificates are ever distributed.

Open an **Administrator PowerShell** in the `driver\` folder (or `drivers\ap-exclusive-acx\scripts\` in a source checkout):

```powershell
powershell -ExecutionPolicy Bypass -File .\install-driver.ps1
```

1. If Windows Test Mode is currently off, the script enables it (`bcdedit /set testsigning on`) and asks you to **reboot once**, then re-run the same command.
2. On the second run (or immediately if Test Mode is already on), the script generates/trusts your local machine's test-signing certificate, signs `ap_exclusive_audio.sys` and `ap_exclusive_audio.cat` locally with SHA-256, creates `ROOT\ApExclusiveAudio`, and installs the driver via `pnputil`.
3. Restart Tidal Desktop (if it was already running), choose **`Line (Audiophile Player)`**, turn **Use Exclusive Mode ON**, and leave **Force volume OFF**.

## Uninstalling

In an **Administrator PowerShell**:

```powershell
# Remove the virtual audio device, Driver Store package, and local test certificate:
powershell -ExecutionPolicy Bypass -File .\uninstall-driver.ps1

# Same, and also disable Windows Test Mode (reboot once afterward):
powershell -ExecutionPolicy Bypass -File .\uninstall-driver.ps1 -DisableTestSigning
```

## Building From Source

Requires:
- Rust `stable-x86_64-pc-windows-msvc`
- Visual Studio C++ x64 Build Tools (`link.exe`, `editbin.exe`)
- `libclang.dll` (Visual Studio Clang tools or `winget install LLVM.LLVM`)
- Windows Driver Kit `10.0.26100` (either system-installed or auto-fetched locally into `.wdk\` via `-FetchWdk`)

```powershell
# Developer build + local test-signing (auto-fetches WDK NuGet packages if not installed):
powershell -ExecutionPolicy Bypass -File drivers\ap-exclusive-acx\scripts\build-and-sign.ps1 -FetchWdk

# Unsigned build for release packaging (never embeds a personal cert):
powershell -ExecutionPolicy Bypass -File drivers\ap-exclusive-acx\scripts\build-and-sign.ps1 -Unsigned

# Hot-update an already-installed driver without rebooting (Admin PowerShell):
powershell -ExecutionPolicy Bypass -File drivers\ap-exclusive-acx\scripts\update-driver.ps1
```

## Licensing & Attribution

Licensed under [GPL-3.0-or-later](../../LICENSE). See [NOTICE](NOTICE) for MIT attribution to Microsoft's ACX samples and `nezucloud/acxmic` for WDK/ACX ABI conventions.
