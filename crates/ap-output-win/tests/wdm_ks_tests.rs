//! Test-First (TDD) verification suite for native Rust Windows Kernel Streaming (`WDM-KS`).
//!
//! Structure:
//! - Offline unit tests: verify exact Win32 `KSPIN_CONNECT` + `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`
//!   binary layouts, WaveRT cyclic half-buffer frame packing & wrap-around, MMCSS guard, and
//!   busy/Audirvana error classification (zero hardware required).
//! - Live hardware integration tests: enumerate real Windows WDM-KS audio filters, probe
//!   `Gungnir 2` across `44.1 -> 384 kHz` via direct `KsCreatePin`, and stream a gentle,
//!   click-free `-24 dBFS` stereo chord at `384,000 Hz / Int32` via native Rust WaveRT KS.
//!
//! Note: Live tests serialize via `HARDWARE_LOCK` so `cargo test` parallelism never self-locks
//! the DAC. If Audirvana or another exclusive player holds the DAC, `OutputError::DeviceBusy`
//! is reported clearly without falling back to the Windows mixer.

use std::sync::Mutex;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;
use ap_output_win::hygiene::{MemoryLockGuard, MmcssGuard, OptimizationLevel};
use ap_output_win::sink::ZeroMixerAudioSink;
use ap_output_win::wdm_ks::{
    KsPinConnectBlob, KsRenderDevice, StreamingWaveRtPump, WaveRtFramePump, WaveRtPumpSource,
    WdmKsSink, classify_ks_pin_error, enumerate_wdm_ks_render_devices, select_wdm_ks_device,
};

static HARDWARE_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// 1. Offline Pure-Logic Unit Tests (Zero Hardware Required)
// ---------------------------------------------------------------------------

#[test]
fn ks_pin_connect_blob_matches_exact_win32_abi_for_int32_384khz() {
    let blob = KsPinConnectBlob::new_pcm(0, 384_000, 2, DacWireFormat::Int32, true);
    let bytes = blob.as_bytes();

    // KSPIN_CONNECT (72 B on x64) + KSDATAFORMAT (64 B) + WAVEFORMATEXTENSIBLE (40 B) = 176 B
    assert_eq!(bytes.len(), 176);
    assert_eq!(blob.format_size(), 104);
    assert_eq!(blob.block_align(), 8); // 2 channels * 4 bytes
    assert_eq!(blob.bits_per_sample(), 32);
    assert_eq!(blob.valid_bits_per_sample(), 32);
    assert_eq!(blob.avg_bytes_per_sec(), 384_000 * 8);
    assert_eq!(blob.channel_mask(), 0x3); // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
}

#[test]
fn ks_pin_connect_blob_supports_int24_in_32_int24_packed_and_int16() {
    let b24_32 = KsPinConnectBlob::new_pcm(1, 352_800, 2, DacWireFormat::Int24In32, true);
    assert_eq!(b24_32.bits_per_sample(), 32);
    assert_eq!(b24_32.valid_bits_per_sample(), 24);
    assert_eq!(b24_32.block_align(), 8);

    let b24 = KsPinConnectBlob::new_pcm(1, 96_000, 2, DacWireFormat::Int24Packed, true);
    assert_eq!(b24.bits_per_sample(), 24);
    assert_eq!(b24.valid_bits_per_sample(), 24);
    assert_eq!(b24.block_align(), 6);

    let b16 = KsPinConnectBlob::new_pcm(1, 44_100, 2, DacWireFormat::Int16, false);
    assert_eq!(b16.bits_per_sample(), 16);
    assert_eq!(b16.valid_bits_per_sample(), 16);
    assert_eq!(b16.block_align(), 4);
}

#[test]
fn wavert_frame_pump_writes_exact_int32_bytes_and_zero_pads_tail() {
    let frames = vec![
        [0x1122_3344_i32, -0x0102_0304_i32],
        [0x7FFF_FF00_i32, i32::MIN],
        [12345_i32, -67890_i32],
    ];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    // Cyclic buffer holding 2 frames per half (16 bytes per half, 32 bytes total).
    let mut ring = vec![0xAA_u8; 32];

    let n0 = pump.fill_slice(&mut ring[0..16]);
    assert_eq!(n0, 2);
    assert!(!pump.is_exhausted());

    // Verify exact little-endian bit layout of frame 0 and frame 1
    assert_eq!(&ring[0..4], &0x1122_3344_i32.to_le_bytes());
    assert_eq!(&ring[4..8], &(-0x0102_0304_i32).to_le_bytes());
    assert_eq!(&ring[8..12], &0x7FFF_FF00_i32.to_le_bytes());
    assert_eq!(&ring[12..16], &i32::MIN.to_le_bytes());

    // Second half holds 1 remaining frame + 1 zero-padded silent frame
    let n1 = pump.fill_slice(&mut ring[16..32]);
    assert_eq!(n1, 1);
    assert!(pump.is_exhausted());
    assert_eq!(&ring[16..20], &12345_i32.to_le_bytes());
    assert_eq!(&ring[20..24], &(-67890_i32).to_le_bytes());
    assert_eq!(&ring[24..32], &[0u8; 8]);

    // Subsequent trailing flush fills pure silence
    let n2 = pump.fill_slice(&mut ring[0..16]);
    assert_eq!(n2, 0);
    assert_eq!(&ring[0..16], &[0u8; 16]);
}

#[test]
fn wavert_frame_pump_encodes_int24_packed_and_int16_from_msb_aligned_int32() {
    // MSB-aligned int32: top 24 bits = 0x123456, top 16 bits = 0x1234
    let frames = vec![[0x1234_5600_i32, -0x0001_0000_i32]];

    let mut p24 = WaveRtFramePump::new(&frames, DacWireFormat::Int24Packed);
    let mut out24 = vec![0u8; 6];
    assert_eq!(p24.fill_slice(&mut out24), 1);
    // 0x123456 in little-endian 3 bytes: [0x56, 0x34, 0x12]
    assert_eq!(&out24[0..3], &[0x56, 0x34, 0x12]);

    let mut p16 = WaveRtFramePump::new(&frames, DacWireFormat::Int16);
    let mut out16 = vec![0u8; 4];
    assert_eq!(p16.fill_slice(&mut out16), 1);
    assert_eq!(&out16[0..2], &0x1234_i16.to_le_bytes());
    assert_eq!(&out16[2..4], &(-1_i16).to_le_bytes());
}

#[test]
fn wavert_frame_pump_fade_out_ramps_smoothly_to_near_zero() {
    let frames = vec![[1_000_000_000_i32, -1_000_000_000_i32]; 100];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut out = vec![0u8; 100 * 8];
    assert_eq!(pump.fill_fade_out_slice(&mut out), 100);

    let first_l = i32::from_le_bytes(out[0..4].try_into().unwrap());
    let mid_l = i32::from_le_bytes(out[50 * 8..50 * 8 + 4].try_into().unwrap());
    let last_l = i32::from_le_bytes(out[99 * 8..99 * 8 + 4].try_into().unwrap());
    assert_eq!(first_l, 1_000_000_000);
    assert!((mid_l - 500_000_000).abs() <= 1);
    assert!(last_l.abs() <= 15_000_000);
}

#[test]
fn streaming_wavert_pump_bridges_uneven_blocks_seamlessly_without_gaps() {
    let blocks = vec![
        InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[1, -1], [2, -2]],
        },
        InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[3, -3]],
        },
        InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[4, -4], [5, -5]],
        },
    ];
    let mut iter = blocks.into_iter();
    let mut pump = StreamingWaveRtPump::new(DacWireFormat::Int32, move || iter.next());

    // Pull 3 frames at a time across 2 + 1 + 2 uneven blocks -> must bridge seamlessly!
    let mut half0 = vec![0xFF_u8; 3 * 8];
    assert_eq!(pump.fill_slice(&mut half0), 3);
    assert_eq!(&half0[0..4], &1_i32.to_le_bytes());
    assert_eq!(&half0[8..12], &2_i32.to_le_bytes());
    assert_eq!(&half0[16..20], &3_i32.to_le_bytes());

    let mut half1 = vec![0xFF_u8; 3 * 8];
    assert_eq!(pump.fill_slice(&mut half1), 2);
    assert_eq!(&half1[0..4], &4_i32.to_le_bytes());
    assert_eq!(&half1[8..12], &5_i32.to_le_bytes());
    assert_eq!(&half1[16..24], &[0_u8; 8]); // clean zero-pad on EOF
    assert_eq!(pump.frames_written(), 5);
}

#[test]
fn busy_win32_codes_map_to_device_busy_with_audirvana_message() {
    // ERROR_SHARING_VIOLATION (32), ERROR_BUSY (170), ERROR_NO_SYSTEM_RESOURCES (1450)
    for code in [32_u32, 170, 1450] {
        let err = classify_ks_pin_error(code, "Gungnir 2", 384_000, DacWireFormat::Int32);
        assert!(err.is_device_busy(), "expected DeviceBusy for Win32 error {code}, got {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("Audirvana"), "missing Audirvana hint in: {msg}");
        assert!(msg.contains("Shared Mode fallback is strictly disabled"));
    }
}

#[test]
fn auto_device_selector_prefers_usb_uac2_dac_over_hdmi_and_desktop_speakers() {
    let devices = vec![
        KsRenderDevice {
            friendly_name: "AMD HD Audio HDMI out #3".to_string(),
            device_path: r"\\?\hdaudio#func_01\e3hdmiout2wave".to_string(),
            render_pin_id: 0,
            supports_looped_wavert: true,
        },
        KsRenderDevice {
            friendly_name: "Pebble V3".to_string(),
            device_path: r"\\?\usb#vid_041e&pid_3272\global".to_string(),
            render_pin_id: 0,
            supports_looped_wavert: true,
        },
        KsRenderDevice {
            friendly_name: "Gungnir 2 Unison 384".to_string(),
            device_path: r"\\?\usb#vid_30be&pid_1022\msft_wave".to_string(),
            render_pin_id: 1,
            supports_looped_wavert: true,
        },
    ];
    let best = select_wdm_ks_device(&devices, None).unwrap();
    assert_eq!(best.friendly_name, "Gungnir 2 Unison 384");

    let explicit = select_wdm_ks_device(&devices, Some("pebble")).unwrap();
    assert_eq!(explicit.friendly_name, "Pebble V3");
}

#[test]
fn mmcss_pro_audio_guard_enters_and_drops_cleanly() {
    let guard = MmcssGuard::enter_pro_audio(OptimizationLevel::High);
    // On Windows desktop sessions, MMCSS "Pro Audio" should activate cleanly.
    #[cfg(windows)]
    assert!(guard.is_active(), "expected MMCSS 'Pro Audio' to activate on Windows");
    drop(guard);
}

#[test]
fn memory_lock_guard_pins_and_releases_pcm_slice_cleanly() {
    let buf = vec![[0_i32, 0_i32]; 4096];
    let guard = MemoryLockGuard::lock_slice(&buf);
    #[cfg(windows)]
    assert!(guard.is_locked(), "expected VirtualLock to succeed for 32 KiB PCM slice");
    drop(guard);
}

// ---------------------------------------------------------------------------
// 2. Live Windows Kernel Streaming (WDM-KS) Hardware Tests (Schiit Gungnir 2)
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[test]
fn live_wdm_ks_enumerates_gungnir2_render_pin() {
    let _lock = HARDWARE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let devices = enumerate_wdm_ks_render_devices().expect("WDM-KS enumeration failed");
    println!("=== Native Rust WDM-KS Render Devices ({}) ===", devices.len());
    for d in &devices {
        println!(
            "  pin={} | name='{}' | path='{}'",
            d.render_pin_id, d.friendly_name, d.device_path
        );
    }
    assert!(
        devices
            .iter()
            .any(|d| d.friendly_name.to_lowercase().contains("gungnir 2")),
        "Schiit Gungnir 2 not found among WDM-KS render devices: {devices:?}"
    );
}

#[cfg(windows)]
#[test]
fn live_wdm_ks_probes_gungnir2_up_to_384khz() {
    let _lock = HARDWARE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sink = WdmKsSink::open("Gungnir 2").expect("failed to locate Gungnir 2 WDM-KS device");

    let rates = [44_100_u32, 48_000, 96_000, 192_000, 352_800, 384_000];
    for &rate in &rates {
        match sink.probe_format(rate, DacWireFormat::Int32) {
            Ok(()) => println!("  [WDM-KS KsCreatePin] Gungnir 2 Int32 @ {rate:>6} Hz : OK"),
            Err(err) if err.is_device_busy() => {
                panic!("DAC LOCKED BY ANOTHER PLAYER (e.g. Audirvana): {err}");
            }
            Err(err) => panic!("unexpected WDM-KS probe error at {rate} Hz: {err}"),
        }
    }
}

#[cfg(windows)]
#[test]
fn live_wdm_ks_plays_gentle_384khz_stereo_chord_on_gungnir2() {
    let _lock = HARDWARE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sink = WdmKsSink::open("Gungnir 2").expect("failed to locate Gungnir 2 WDM-KS device");

    // Generate a gentle, pleasant -24 dBFS Dm9 warm stereo chord @ 384,000 Hz in pure f64 -> i32.
    let sample_rate = 384_000_u32;
    let duration_s = 1.5_f64;
    let total_frames = (sample_rate as f64 * duration_s).round() as usize;
    let fade_frames = (sample_rate as f64 * 0.08).round() as usize;
    let peak_lin = 10.0_f64.powf(-24.0 / 20.0); // safe -24 dBFS for external power amp
    let scale = (i32::MAX as f64) * peak_lin / 4.0;

    // D4 (293.66), F4 (349.23), A4 (440.0), E5 (659.25) with subtle stereo spread
    let freqs_l = [293.6648_f64, 349.2282, 440.0, 659.2551];
    let freqs_r = [293.6648_f64, 349.2282, 440.4, 659.6551];

    let mut frames = Vec::with_capacity(total_frames);
    for i in 0..total_frames {
        let t = i as f64 / sample_rate as f64;
        let env = if i < fade_frames {
            0.5 * (1.0 - (std::f64::consts::PI * i as f64 / fade_frames as f64).cos())
        } else if i >= total_frames - fade_frames {
            let rem = total_frames - 1 - i;
            0.5 * (1.0 - (std::f64::consts::PI * rem as f64 / fade_frames as f64).cos())
        } else {
            1.0
        };
        let decay = (-t / 1.1).exp();
        let l: f64 = freqs_l.iter().map(|&f| (2.0 * std::f64::consts::PI * f * t).sin()).sum();
        let r: f64 = freqs_r.iter().map(|&f| (2.0 * std::f64::consts::PI * f * t).sin()).sum();
        let s_l = (l * env * decay * scale).round() as i32;
        let s_r = (r * env * decay * scale).round() as i32;
        frames.push([s_l, s_r]);
    }

    let buffer = InterleavedInt32Buffer {
        sample_rate_hz: sample_rate,
        frames,
    };

    match sink.play_full_memory(&buffer, DacWireFormat::Int32) {
        Ok(stats) => {
            println!("=== Native Rust WDM-KS WaveRT Playback Complete ===");
            println!("  {stats:?}");
            assert_eq!(stats.sample_rate_hz, 384_000);
            assert_eq!(stats.frames_played, total_frames);
            assert!(stats.wavert_buffer_bytes > 0);
            assert!(stats.notifications > 0);
        }
        Err(err) if err.is_device_busy() => {
            panic!("DAC LOCKED BY ANOTHER PLAYER (e.g. Audirvana): {err}");
        }
        Err(err) => panic!("Native Rust WDM-KS playback failed: {err}"),
    }
}

#[cfg(windows)]
#[test]
fn live_wdm_ks_plays_generated_nocturne_24_96_flac_on_gungnir2() {
    let _lock = HARDWARE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join("original_nocturne_24_96.flac");

    let decoded = ap_source::flac::decode_flac_full_memory_int32(&fixture)
        .expect("failed to decode 24/96 CC0 nocturne FLAC into RAM");
    assert_eq!(decoded.spec.sample_rate_hz, 96_000);
    assert_eq!(decoded.spec.bits_per_sample, 24);

    let mut sink = WdmKsSink::open("Gungnir 2").expect("failed to locate Gungnir 2 WDM-KS device");
    println!(
        "=== Playing '{}' (24-bit / {} Hz, {} frames) via Native Rust WDM-KS WaveRT ===",
        fixture.file_name().unwrap().to_string_lossy(),
        decoded.spec.sample_rate_hz,
        decoded.pcm.frames.len()
    );

    match sink.play_full_memory(&decoded.pcm, DacWireFormat::Int32) {
        Ok(stats) => {
            println!("  Playback complete: {stats:?}");
            assert_eq!(stats.sample_rate_hz, 96_000);
            assert_eq!(stats.frames_played, decoded.pcm.frames.len());
            assert!(stats.mmcss_pro_audio);
        }
        Err(err) if err.is_device_busy() => {
            panic!("DAC LOCKED BY ANOTHER PLAYER (e.g. Audirvana): {err}");
        }
        Err(err) => panic!("Native Rust WDM-KS FLAC playback failed: {err}"),
    }
}
