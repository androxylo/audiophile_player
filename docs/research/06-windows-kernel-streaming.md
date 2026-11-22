# INV-06: Windows Audio Transport — WASAPI Exclusive, Kernel Streaming (WDM-KS) & Minimal-Effort Zero-Mixer POC

**Status:** Completed (Empirical Gungnir 2 hardware probe pending first POC run)  
**Resolves:** `OQ-18`, `OQ-19`, `OQ-20`, `OQ-21`, `OQ-22`  
**Hard Constraints:** Zero Windows mixer anywhere in the pipeline (even in the earliest POC); non-exclusive / Shared Mode strictly forbidden; Audirvana-style kernel-mode streaming target; Windows 10/11 + Schiit Gungnir 2 (Unison 384 USB / `usbaudio2.sys`).

---

## 1. Windows Audio Stack: Where the Mixer Lives & How Each Mode Bypasses It

Understanding the exact Windows 10/11 signal path removes common audiophile myths and clarifies why **both** WASAPI Exclusive Mode and Direct Kernel Streaming (WDM-KS) achieve 100% mixer-free, bit-perfect delivery—and exactly where they differ.

### Signal Path Comparison (Windows 10/11 + `usbaudio2.sys`)

| Mode | Exact Signal Path | Touches Windows Mixer (`audiodg.exe`)? | Allowed? |
| --- | --- | --- | --- |
| **WASAPI Shared Mode** (`AUDCLNT_SHAREMODE_SHARED`) | `App → AudioSes.dll → Cross-process buffer → audiodg.exe (audioeng.dll: float32 mixer, forced resampling, CAudioLimiter, APOs/sAPOs, Windows volume) → WaveRT pin (usbaudio2.sys) → USB ISOC → DAC` | **YES — Full Windows Mixer & Resampler** | **STRICTLY FORBIDDEN** (not even in POC) |
| **WASAPI Exclusive Mode** (`AUDCLNT_SHAREMODE_EXCLUSIVE`, Event-Driven) | `App → AudioSes.dll (in-process!) → WaveRT cyclic buffer mapped directly into App memory (usbaudio2.sys) → USB ISOC → DAC` | **NO — 100% Bypassed.** `AudioSrv` grants exclusive lock & evicts `audiodg.exe`. Zero mixing, zero APOs, zero resampling, zero `CAudioLimiter`. | **YES (Compliant)** |
| **Direct Kernel Streaming (WDM-KS)** *(Audirvana "Kernel Streaming" Mode)* | `App → SetupDi* / CreateFile(KS filter) / KsCreatePin (ksuser.dll) → WaveRT cyclic buffer mapped directly into App memory (usbaudio2.sys) → USB ISOC → DAC` | **NO — 100% Bypassed.** Also bypasses `AudioSes.dll` and WASAPI COM/session management completely. | **YES (Ultimate Purist Target)** |

### Key Architectural Truth (Clearing Up Myths)

1. **Both WASAPI Exclusive and WDM-KS hit the exact same kernel driver (`usbaudio2.sys`) and the exact same WaveRT cyclic hardware buffer.**
   - Schiit's **Unison 384™ USB** is a class-compliant USB Audio Class 2.0 (UAC2) interface driven natively on Windows 10/11 by Microsoft's inbox **`usbaudio2.sys`**, which is a **WaveRT (Real-Time Wave Port) miniport driver**.
   - In **WASAPI Exclusive Mode**, `AudioSes.dll` runs **inside the player's own process**, negotiates an exclusive lock via `AudioSrv` (forcing `audiodg.exe` off the endpoint), opens the `usbaudio2.sys` WaveRT pin, and memory-maps the cyclic DMA/kernel buffer directly into the player's address space. **`audiodg.exe` does not touch a single audio sample.**
2. **How Audirvana's "Kernel Streaming" Mode Works Under the Hood (`OQ-18`):**
   - Audirvana (Studio / Origin on Windows) implements **user-mode WDM-KS** directly against the device's Kernel Streaming filter/pin (`ks.h`, `ksmedia.h`, `ksuser.dll`), skipping `AudioSes.dll`, `IMMDevice` audio sessions, and the WASAPI COM wrapper altogether:
     1. Enumerate audio filter device interfaces via `SetupDiGetClassDevs` (`KSCATEGORY_AUDIO` + `KSCATEGORY_RENDER`).
     2. Open the kernel filter handle with `CreateFileW`.
     3. Query pin count & supported formats (`KSPROPERTY_PIN_CINSTANCES`, `KSPROPERTY_PIN_DATARANGES`, `KSPROPERTY_PIN_DATAFLOW = KSPIN_DATAFLOW_IN`).
     4. Instantiate the render pin directly with **`KsCreatePin`** passing a `KSDATAFORMAT_WAVEFORMATEXTENSIBLE` structure matching exact integer PCM rate/bit-depth.
     5. Allocate and map the **WaveRT cyclic buffer** directly into user-space memory via `DeviceIoControl` (`IOCTL_KS_PROPERTY`) using **`KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION`** (or fallback `KSPROPERTY_RTAUDIO_BUFFER` / legacy `IOCTL_KS_WRITE_STREAM` for non-WaveRT drivers).
     6. Register a Win32 event handle via **`KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT`** so hardware buffer-period completion wakes the player's MMCSS `"Pro Audio"` thread directly.
     7. Transition pin state: `KSSTATE_ACQUIRE` → `KSSTATE_PAUSE` → pre-fill buffer → `KSSTATE_RUN`.
   - **Audirvana's Kernel Streaming Optimization Levels (`Normal` / `High` / `Extreme`):** Selectable under `Settings > Audio > Kernel Streaming` when playback is stopped. These levels progressively raise OS thread/process scheduling priority (MMCSS `"Pro Audio"` / high priority class), tighten buffer/transfer scheduling, and isolate the audio pump thread from background OS tasks (`Normal` = baseline KS; `High` = elevated thread priority & buffer tuning, commonly cited as stability/quality sweet spot; `Extreme` = maximum priority & resource locking for dedicated audio PCs, with higher risk of dropouts/clicks on busy machines).
   - **PCM Focus & Schiit Unison USB Caveat:** Audirvana specifically targets Kernel Streaming for **non-DSD PCM playback** (recommending ASIO mainly for native DSD — irrelevant to our pure-PCM Gungnir 2 engine). However, Audirvana Forum users with **Schiit Bifrost 2** (which shares the Schiit Unison USB / `usbaudio2.sys` lineage with Gungnir 2) reported **mixed results at high sample rates in KS mode** (some hitting sample-rate limits or playback quirks, others running cleanly). This reinforces why we must empirically probe Gungnir 2 across **both WDM-KS and WASAPI Exclusive** up to `352.8 / 384 kHz` in POC #1!
   - **Myth debunked:** User-mode WDM-KS does **not** run application code in Ring 0 (kernel space). It makes standard user-mode `DeviceIoControl` / `KsCreatePin` calls into `ks.sys` / `usbaudio2.sys`. (Historically, malformed KS IOCTLs against buggy third-party kernel drivers could trigger BSODs, which is why Microsoft recommends WASAPI Exclusive over WDM-KS.)
3. **Practical Comparison on Windows 10/11 + `usbaudio2.sys` (`OQ-19`):**

| Aspect | WASAPI Exclusive (Event-Driven) | Direct WDM-KS (Audirvana Kernel Mode) |
| --- | --- | --- |
| **Bit-exactness** | 100% bit-perfect | 100% bit-perfect (identical bits to `usbaudio2.sys`) |
| **Windows mixer (`audiodg.exe`)** | Completely bypassed | Completely bypassed |
| **`AudioSes.dll` / `AudioSrv` layer** | Used for session lock & WaveRT setup only | Completely bypassed |
| **Evicting idle browser / OS sounds** | **Clean & automatic** (`AudioSrv` releases shared clients when exclusive opens) | **Strict/brittle** — fails with `ERROR_BUSY` / `STATUS_INSUFFICIENT_RESOURCES` if `audiodg.exe` still holds the render pin open |
| **Rate switching (`352.8 ↔ 384 kHz`)** | Very reliable via `Stop → Release → Activate → Initialize` | Reliable **if** full teardown is done (`KSSTATE_PAUSE → KSSTATE_STOP → UnregisterEvent → CloseHandle(pin) → KsCreatePin`) |
| **Implementation complexity** | Low (~150 lines direct Win32 / ~20 lines via library) | Medium-High (~600–1,200 lines direct Win32 for topology, WaveRT + IRP fallback, device-name matching) |

**Architectural Decision:** Our engine will support **both** zero-mixer backends behind one small `AudioOutput` trait/interface:
- **WASAPI Exclusive (Event-Driven)** — guaranteed zero mixer, reliable eviction of `audiodg.exe`, ideal for **POC #1** and daily stability.
- **Direct WDM-KS (WaveRT Kernel Streaming)** — Audirvana-style ultimate minimal-layer mode, testable immediately via PortAudio WDM-KS in POC #1 and implemented directly via Win32 `ksuser.dll` / `DeviceIoControl` in POC #2 / Rust production.
- **WASAPI Shared Mode is permanently forbidden** — if exclusive / KS acquisition fails, the player logs a clear error and stops; it **never** falls back to Shared Mode.

---

## 2. Critical Low-Level Gotchas for `usbaudio2.sys` & Schiit Unison 384

Even experienced Windows audio developers hit these exact pitfalls with UAC2 DACs (`usbaudio2.sys`):

1. **Integer PCM Only — Do Not Request `IEEE_FLOAT` in Exclusive/KS Mode:**
   - Unlike Shared Mode (which always expects `float32`), `usbaudio2.sys` hardware endpoints accept **integer PCM (`KSDATAFORMAT_SUBTYPE_PCM`)** only. Requesting `IEEE_FLOAT` will fail with `AUDCLNT_E_UNSUPPORTED_FORMAT`.
2. **Container Width vs. Valid Bits (`WAVEFORMATEXTENSIBLE`):**
   - For **24-bit** and **32-bit** audio, Windows UAC2 drivers typically prefer **32-bit containers** (`wBitsPerSample = 32`, `nBlockAlign = 8` for stereo), either with `wValidBitsPerSample = 32` or `wValidBitsPerSample = 24` (samples left-aligned / MSB-aligned in `int32`: `sample24 << 8`).
   - Some endpoints accept **24-bit packed** (`wBitsPerSample = 24`, `wValidBitsPerSample = 24`, 3 bytes/sample), while others reject it due to 32-bit DMA alignment.
   - For **16-bit** FLAC in POC #1: either open native `16-bit` (`wBitsPerSample = 16`, `wValidBitsPerSample = 16`), **or** losslessly zero-pad into `32-bit` (`sample16 << 16`, which is mathematically 100% bit-identical to 16-bit PCM).
   - Always probe with `IAudioClient::IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &wfext, NULL)` (or KS `KSPROPERTY_PIN_PROPOSEDATAFORMAT`) in priority order: `32-in-32` → `24-in-32` → `24-in-24` → `16-in-16`.
   - Set `dwChannelMask = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT` (`0x00000003`).
3. **Mandatory WASAPI Event-Driven Alignment Dance (`AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`):**
   - In `AUDCLNT_SHAREMODE_EXCLUSIVE | AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, `hnsBufferDuration` and `hnsPeriodicity` **must be equal** and aligned to the USB hardware's exact packet frame count.
   - If the first `IAudioClient::Initialize()` returns `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` (`0x88890001`), this is normal:
     1. Call `GetBufferSize(&nFrames)`.
     2. Recompute `hnsDuration = (REFERENCE_TIME)((10_000_000.0 * nFrames / sampleRate) + 0.5)`.
     3. `Release()` the `IAudioClient`, re-`Activate()` it from `IMMDevice`, and call `Initialize()` again with `hnsBufferDuration = hnsPeriodicity = hnsDuration`.
   - **Buffer Size Guidance:** HQPlayer's official manual (v5.7.3, p. 54) specifically warns that Windows' default USB buffer (`10 ms`) is small and recommends **`100 ms`** as a rock-solid starting point. Since our player has **no real-time / low-latency requirement**, using a generous hardware buffer (`50–100 ms`, aligned to the driver period) maximizes stability and minimizes CPU wakeups.
4. **Pre-Fill Before `Start()`:**
   - In both WASAPI Exclusive and WaveRT WDM-KS, fill the initial buffer with the first audio frames (or silence) **before** calling `IAudioClient::Start()` / transitioning to `KSSTATE_RUN`.
5. **Full-Memory RAM Playback & Thread Hygiene (`OQ-21`):**
   - **Decode the entire FLAC track into RAM before starting output** (Audirvana / Album Player "Full Memory" approach). This removes all filesystem I/O from the playback loop and costs only ~100–250 MB for a 5-minute 24/96 stereo track (or ~500 MB–1 GB once upsampled to 32/384 in later milestones).
   - Register the audio output thread with Windows **MMCSS**: `AvSetMmThreadCharacteristicsW(L"Pro Audio", &taskIndex)` (elevates scheduling priority cleanly without breaking OS responsiveness).
   - Optional `VirtualLock()` on the pre-loaded PCM buffer so Windows never pages it out mid-track.
   - Verify Windows Sound Settings for Gungnir 2 has **"Allow applications to take exclusive control of this device"** enabled (on by default in Windows 10/11).

---

## 3. Minimal-Effort Path for Very First Prototype (POC #1: Raw `~/Music` Playback, Zero Mixer)

### Local Machine Toolchain Check
Already installed on this Windows machine:
- **Python 3.14** (`C:\Python314\python.exe`) + `pip`
- **Visual Studio 2022 Preview** (`clang.exe`, `cmake.exe`)
- **Rust (`cargo`/`rustc`)**: Not installed yet (no need to install until prototypes are proven, per project rules).

### Comparing the 3 Options for POC #1

| Option | Effort / Code Size | Mixer-Free? | Supports WDM-KS + WASAPI Excl? | Pros & Cons |
| --- | --- | --- | --- | --- |
| **Option A (Recommended for POC #1): Python (`sounddevice` + `soundfile`)** | **~80–120 lines**, zero compilation, runs in minutes | **YES** (strictly `WDM-KS` or `WASAPI Exclusive`, hard-fail if unavailable) | **YES — BOTH!** Official `sounddevice` Windows wheel bundles PortAudio compiled with **both** `Windows WASAPI` (`WasapiSettings(exclusive=True)`) and `Windows WDM-KS`! | **Fastest path to sound on Gungnir 2.** Lets us immediately test BOTH WASAPI Exclusive and WDM-KS on your real DAC, probe 44.1→384 kHz rates, and add the tiny stdlib `http.server` UI effortlessly. *Caveat:* PortAudio sits between Python and Win32, so we must pass `dtype='int32'`, `clip_off=True`, `dither_off=True` and verify bit-exactness. |
| **Option B (Recommended for POC #2): Direct Win32 C/C++ (no PortAudio)** | ~250–350 lines (WASAPI Excl) + ~600 lines (direct WDM-KS) | **YES** (100% direct Win32 calls) | **YES** (full control over `WAVEFORMATEXTENSIBLE` & `KSPROPERTY_RTAUDIO_*`) | Zero middleware; exact blueprint for the final Rust `windows-rs` production backend. Slightly more initial boilerplate than Python for Day 1. |
| **Option C (Final Production): Rust (`windows-rs` / `wasapi` + `symphonia`/`claxon`)** | ~200+ lines + `rustup` setup | **YES** | WASAPI via `wasapi`/`windows-rs`; WDM-KS via custom `windows-rs` FFI | Target production stack once DSP & transport behavior are proven. |

### Exact Minimal-Effort Recipe for POC #1 (Python, Strictly Zero-Mixer)

1. **Dependencies (2 tiny packages, open-source MIT/BSD/LGPL):**
   ```powershell
   pip install sounddevice soundfile
   ```
   - `soundfile` wraps `libsndfile` (native lossless FLAC decoding, reads straight to MSB-aligned `int32` via `sf.read(path, dtype='int32', always_2d=True)` with zero float conversion!).
   - `sounddevice` wraps PortAudio with **both** `Windows WASAPI` (Exclusive) and `Windows WDM-KS` (Kernel Streaming) built in.
2. **Strict Zero-Mixer Guardrails Built Into POC #1:**
   - Enumerate host APIs via `sd.query_hostapis()`; **only ever allow** `Windows WDM-KS` or `Windows WASAPI` (with `sd.WasapiSettings(exclusive=True)`).
   - **Never allow** `MME`, `Windows DirectSound`, or `WASAPI` without `exclusive=True`. If opening the exclusive/KS stream fails, exit with an error — **no Shared Mode fallback under any circumstance**.
   - Always pass **`dtype='int32'`**, **`clip_off=True`**, **`dither_off=True`** (maps to PortAudio `paClipOff | paDitherOff` so PortAudio applies zero dithering, zero clipping, and zero float conversion).
   - Load each FLAC completely into RAM before opening the stream for that file's exact native `samplerate` (44.1 / 48 / 88.2 / 96 / 176.4 / 192 kHz), then close and cleanly reopen the stream when the next random track has a different sample rate.
3. **Included Hardware Probe (Answers `OQ-02`, `OQ-04`, `OQ-19` Empirically in 10 Seconds):**
   - Before/alongside playing `~/Music`, a tiny `--probe` flag tests Gungnir 2 across **both** `Windows WASAPI (Exclusive)` and `Windows WDM-KS` at `44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000 Hz` and `int16 / int32`, printing an exact hardware capability matrix for your real Gungnir 2!

*(Note: After POC #1 plays cleanly and proves Gungnir 2's exact behavior under WASAPI Exclusive and WDM-KS, POC #2 will replace PortAudio with direct Win32 `IAudioClient` Exclusive + direct `KsCreatePin` / WaveRT WDM-KS so we have 100% zero-middleware control prior to porting to Rust.)*

---

## 4. Objective Verification Protocol: Proving Zero Windows Mixer & Bit-Perfection (`OQ-22`)

Every prototype (starting with POC #1) must pass these four concrete checks:

1. **Exclusive Hardware Lock Check:** While playing a FLAC to Gungnir 2, attempt to play audio to Gungnir 2 from a browser (YouTube) or Windows Sound Control Panel ("Test"). Output from the other app must fail or remain completely silent — proving `audiodg.exe` has been evicted from the endpoint.
2. **Windows Volume & APO Immunity Check:** While playing, drag the Windows volume mixer slider for Gungnir 2 (or enable a Windows APO/enhancement) — playback volume and sound must remain **100% unchanged** (note: if the DAC itself exposes a USB hardware volume descriptor, leave it untouched at 100%; Schiit Unison USB has no software attenuation in exclusive mode).
3. **Dynamic Native Sample-Rate Lock Check:** Set Windows Shared Mode default format to `24-bit / 48 kHz` in Sound Control Panel, then play a `44.1 kHz`, `96 kHz`, and `192 kHz` (and synthetic `352.8 / 384 kHz`) FLAC. Verify via exclusive open success (and Gungnir 2 relay/status / `audiodg.exe` 0% CPU activity in Task Manager/Process Explorer) that the hardware locks to each file's exact native rate with zero OS resampling.
4. **Bit-Exact Pipeline Unit Test (TDD):** Verify with a deterministic synthetic 16-bit and 24-bit FLAC test fixture that the exact `int32` byte buffer handed to the output stream matches the reference decoded integer samples bit-for-bit (`0` bit differences across all samples, zero scaling/rounding/dither).

---

## 5. Primary References & Optional Inaccessible Sources

### Consulted Sources
- Microsoft Learn — *Exclusive-Mode Streams (Core Audio / WASAPI)*: https://learn.microsoft.com/en-us/windows/win32/coreaudio/exclusive-mode-streams
- Microsoft Learn — *IAudioClient::Initialize & `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`*: https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-initialize
- Microsoft Learn — *User-Mode Audio Components (`audiodg.exe`, `AudioSes.dll`)*: https://learn.microsoft.com/en-us/windows/win32/coreaudio/user-mode-audio-components
- Microsoft Learn — *WaveRT Port Driver & `KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION`*: https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/wavert-port-driver
- Microsoft Learn — *USB Audio 2.0 Class Driver (`usbaudio2.sys`)*: https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/usb-2-0-audio-drivers
- Signalyst — *HQPlayer 5 Desktop Manual (v5.7.3)*, pp. 13–14, 54 (local reference; Windows WASAPI exclusive & 100 ms USB buffer guidance).
- PortAudio — *WDM-KS (`pa_win_wdmks.c`) & WASAPI (`pa_win_wasapi.c`)*: https://github.com/PortAudio/portaudio
- Audirvana — *Exclusive Playback Technology* & Support Guide (*How to get the best sound possible with Audirvāna*): https://audirvana.com/exclusive-playback-technology/ , https://help.audirvana.com/en/support/solutions/articles/202000050813-how-to-get-the-best-sound-possible-with-audirv%C4%81na-
- Audirvana Community Forum — Kernel Streaming optimization levels (`Normal`/`High`/`Extreme`), WASAPI comparison & Schiit Bifrost 2 high-res KS reports: threads `35671`, `28935`, `29191`, `28492` (`community.audirvana.com`), plus JRiver Interact topic `134799`.
- `python-sounddevice` Documentation — *`WasapiSettings`, `RawOutputStream`, `clip_off`, `dither_off`*: https://python-sounddevice.readthedocs.io/
- Rust `wasapi` crate (`wasapi-rs`, MIT): https://docs.rs/wasapi/

### Optional Inaccessible Sources (If Deeper Audirvana Internals Are Needed Later)
- **Audirvana Community Forum (`community.audirvana.com`)** — Damien Plisson's technical posts detailing Audirvana Studio/Origin's WDM-KS WaveRT implementation details and debug logs with UAC2 DACs (login/Cloudflare-gated for full thread inspection). Not blocking POC #1; if you ever want us to match a specific Audirvana KS quirk, paste a snippet or drop a browser PDF into `docs/research/sources/`.
