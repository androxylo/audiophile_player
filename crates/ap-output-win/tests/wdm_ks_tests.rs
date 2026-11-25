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
use std::time::Duration;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;
use ap_output_win::hygiene::{MemoryLockGuard, MmcssGuard, OptimizationLevel};
use ap_output_win::sink::ZeroMixerAudioSink;
use ap_output_win::wdm_ks::{
    AudioPositionCursorSelector, KsPinConnectBlob, KsRenderDevice, PositionWrapMode, StreamBlock,
    StreamingWaveRtPump, WaveRtFramePump, WaveRtLoopController, WaveRtPumpSource, WaveRtRingLayout,
    WaveRtStepOutcome, WdmKsSink, classify_ks_pin_error, enumerate_wdm_ks_render_devices,
    select_wdm_ks_device,
};

static HARDWARE_LOCK: Mutex<()> = Mutex::new(());
static MEMORY_LOCK_TEST_LOCK: Mutex<()> = Mutex::new(());

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
    // Exact MSB-aligned 24-bit int32 (low byte == 0) passes bit-identically to Int24Packed.
    let frames24 = vec![[0x1234_5600_i32, -0x0001_0000_i32]];
    let mut p24 = WaveRtFramePump::new(&frames24, DacWireFormat::Int24Packed);
    let mut out24 = vec![0u8; 6];
    assert_eq!(p24.fill_slice(&mut out24), 1);
    // 0x123456 in little-endian 3 bytes: [0x56, 0x34, 0x12]
    assert_eq!(&out24[0..3], &[0x56, 0x34, 0x12]);

    // Exact MSB-aligned 16-bit int32 (low 16 bits == 0) passes bit-identically to Int16.
    let frames16 = vec![[0x1234_0000_i32, -0x0001_0000_i32]];
    let mut p16 = WaveRtFramePump::new(&frames16, DacWireFormat::Int16);
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
    let mut pump = StreamingWaveRtPump::new(384_000, DacWireFormat::Int32, move || iter.next());

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
    let _mem_lock = MEMORY_LOCK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let buf = vec![[0_i32, 0_i32]; 4096];
    let guard = MemoryLockGuard::lock_slice(&buf);
    #[cfg(windows)]
    assert!(guard.is_locked(), "expected VirtualLock to succeed for 32 KiB PCM slice");
    drop(guard);
}

#[test]
fn ks_pin_connect_blob_padding_is_zeroed_and_channel_mask_handles_high_channels() {
    let blob = KsPinConnectBlob::new_pcm(0, 384_000, 2, DacWireFormat::Int32, true);
    let bytes = blob.as_bytes();
    // Bytes 52..56 are the x64 alignment padding after KSPIN_CONNECT.PinId.
    assert_eq!(&bytes[52..56], &[0, 0, 0, 0]);

    // 32+ channels must not overflow shift.
    let b32 = KsPinConnectBlob::new_pcm(0, 48_000, 32, DacWireFormat::Int32, true);
    assert_eq!(b32.channel_mask(), u32::MAX);
    let b0 = KsPinConnectBlob::new_pcm(0, 48_000, 0, DacWireFormat::Int32, true);
    assert_eq!(b0.channel_mask(), 0);
}

#[test]
fn wavert_frame_pump_dithers_reduced_depths_and_zeros_int24_in_32_low_byte() {
    assert_eq!(DacWireFormat::Int32.bytes_per_frame(), 8);
    assert_eq!(DacWireFormat::Int24In32.bytes_per_frame(), 8);
    assert_eq!(DacWireFormat::Int24Packed.bytes_per_frame(), 6);
    assert_eq!(DacWireFormat::Int16.bytes_per_frame(), 4);

    // 1. Already-quantized 24-bit and 16-bit buffers must pass through BIT-IDENTICALLY (no double-dither).
    let exact24 = vec![[0x1234_5600_i32, -0x0045_6700_i32]; 16];
    let mut p_exact24 = WaveRtFramePump::new(&exact24, DacWireFormat::Int24In32);
    let mut out_exact24 = vec![0xAA_u8; 16 * 8];
    assert_eq!(p_exact24.fill_slice(&mut out_exact24), 16);
    for i in 0..16 {
        assert_eq!(
            i32::from_le_bytes(out_exact24[i * 8..i * 8 + 4].try_into().unwrap()),
            0x1234_5600
        );
        assert_eq!(
            i32::from_le_bytes(out_exact24[i * 8 + 4..i * 8 + 8].try_into().unwrap()),
            -0x0045_6700
        );
    }

    // 2. Sub-LSB 32-bit input into Int24In32: unbiased TPDF dither (mean ≈ 0x123456.75), low byte always 0, clean saturation.
    let n = 8192_usize;
    let frames24 = vec![[0x1234_56C0_i32, 0x7FFF_FFFF_i32]; n];
    let mut p24_32 = WaveRtFramePump::new(&frames24, DacWireFormat::Int24In32);
    let mut out24_32 = vec![0xAA_u8; n * 8];
    assert_eq!(p24_32.fill_slice(&mut out24_32), n);
    let mut sum_l = 0.0_f64;
    let mut distinct_l = std::collections::HashSet::new();
    for i in 0..n {
        let l = i32::from_le_bytes(out24_32[i * 8..i * 8 + 4].try_into().unwrap());
        let r = i32::from_le_bytes(out24_32[i * 8 + 4..i * 8 + 8].try_into().unwrap());
        assert_eq!(l & 0xFF, 0, "Int24In32 low byte must be zeroed");
        assert_eq!(r & 0xFF, 0, "Int24In32 low byte must be zeroed");
        let l_q = l >> 8;
        assert!(
            (l_q - 0x123456).abs() <= 2,
            "TPDF dither must stay within ±2 LSB of input, got {l_q:#x}"
        );
        // Full-scale positive never wraps negative.
        assert!(r >= 0x7FFF_FD00 && r <= 0x7FFF_FF00);
        distinct_l.insert(l_q);
        sum_l += (l_q - 0x123456) as f64;
    }
    let mean_frac_l = sum_l / (n as f64);
    assert!(
        (mean_frac_l - 0.75).abs() < 0.05,
        "TPDF 24-bit mean fractional LSB should be ~0.75, got {mean_frac_l:.4}"
    );
    assert!(
        distinct_l.len() >= 2,
        "TPDF dither must actively dither sub-LSB input across adjacent codes"
    );

    // Fade-out on Int24In32 must also keep low byte zero on every frame.
    let fade_frames = vec![[0x4000_0000_i32, -0x4000_0000_i32]; 16];
    let mut pf = WaveRtFramePump::new(&fade_frames, DacWireFormat::Int24In32);
    let mut out_f = vec![0u8; 16 * 8];
    assert_eq!(pf.fill_fade_out_slice(&mut out_f), 16);
    for i in 0..16 {
        assert_eq!(out_f[i * 8], 0, "Int24In32 faded L low byte must be 0");
        assert_eq!(out_f[i * 8 + 4], 0, "Int24In32 faded R low byte must be 0");
    }

    // 3. Int16 TPDF requantization: mean of 0x1234_C000 is 0x1234.75, full-scale saturates cleanly.
    let frames16 = vec![[0x1234_C000_i32, i32::MAX]; n];
    let mut p16 = WaveRtFramePump::new(&frames16, DacWireFormat::Int16);
    let mut out16 = vec![0u8; n * 4];
    assert_eq!(p16.fill_slice(&mut out16), n);
    let mut sum16 = 0.0_f64;
    for i in 0..n {
        let l16 = i16::from_le_bytes(out16[i * 4..i * 4 + 2].try_into().unwrap());
        let r16 = i16::from_le_bytes(out16[i * 4 + 2..i * 4 + 4].try_into().unwrap());
        assert!((i32::from(l16) - 0x1234).abs() <= 2);
        assert!(r16 >= i16::MAX - 2);
        sum16 += (i32::from(l16) - 0x1234) as f64;
    }
    let mean_frac_16 = sum16 / (n as f64);
    assert!(
        (mean_frac_16 - 0.75).abs() < 0.05,
        "TPDF 16-bit mean fractional LSB should be ~0.75, got {mean_frac_16:.4}"
    );
}

#[test]
fn streaming_wavert_pump_rejects_sample_rate_mismatch() {
    let blocks = vec![
        InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[10, -10]],
        },
        InterleavedInt32Buffer {
            sample_rate_hz: 352_800, // mismatched!
            frames: vec![[20, -20]],
        },
    ];
    let mut iter = blocks.into_iter();
    let mut pump = StreamingWaveRtPump::new(384_000, DacWireFormat::Int32, move || iter.next());
    let mut out = vec![0xFF_u8; 2 * 8];
    let written = pump.fill_slice(&mut out);
    assert_eq!(written, 1, "must stop before writing mismatched-rate block");
    assert_eq!(&out[8..16], &[0u8; 8], "tail must be zero-filled");
    let err = pump
        .take_error()
        .expect("expected sample rate mismatch error");
    assert!(
        err.to_string().contains("384000") && err.to_string().contains("352800"),
        "unexpected error: {err}"
    );
}

#[test]
fn auto_device_selector_requires_looped_wavert_support() {
    let devices = vec![
        KsRenderDevice {
            friendly_name: "Gungnir 2 Non-WaveRT".to_string(),
            device_path: r"\\?\usb#vid_30be&pid_1022\msft_wave".to_string(),
            render_pin_id: 0,
            supports_looped_wavert: false,
        },
        KsRenderDevice {
            friendly_name: "Real WaveRT DAC".to_string(),
            device_path: r"\\?\usb#vid_1234&pid_5678\wave".to_string(),
            render_pin_id: 1,
            supports_looped_wavert: true,
        },
    ];
    // Even though "Gungnir 2" scores higher, non-WaveRT devices must be rejected.
    let best = select_wdm_ks_device(&devices, None).unwrap();
    assert_eq!(best.friendly_name, "Real WaveRT DAC");
    assert!(select_wdm_ks_device(&devices, Some("Gungnir")).is_err());
}

#[cfg(windows)]
#[test]
fn mmcss_pro_audio_guard_restores_prior_thread_priority_on_drop() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
    // Run on a dedicated thread so priority changes don't affect the test harness thread.
    let handle = std::thread::spawn(|| {
        let before = unsafe { GetThreadPriority(GetCurrentThread()) };
        {
            let guard = MmcssGuard::enter_pro_audio(OptimizationLevel::Extreme);
            assert!(guard.is_active());
            let during = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_ne!(during, before, "Extreme optimization should elevate thread priority");
        }
        let after = unsafe { GetThreadPriority(GetCurrentThread()) };
        assert_eq!(after, before, "MmcssGuard::drop must restore prior thread priority");
    });
    handle.join().unwrap();
}

#[test]
fn wavert_ring_layout_aligns_odd_frame_counts_and_rejects_unaligned_sizes() {
    // Int24Packed (bpf = 6) with page-rounded 28672 bytes (28672 % 6 == 2) must be rejected.
    assert!(WaveRtRingLayout::new(28_672, 6).is_err());
    // Too small (< 2 frames) or bpf == 0 must be rejected.
    assert!(WaveRtRingLayout::new(8, 8).is_err());
    assert!(WaveRtRingLayout::new(64, 0).is_err());

    // Odd frame count (5 frames * 6 bytes = 30 bytes): half 0 = 2 frames (0..12), half 1 = 3 frames (12..30).
    let odd = WaveRtRingLayout::new(30, 6).expect("odd frame count should align cleanly");
    assert_eq!(odd.half_len(), 12);
    assert_eq!(odd.half_range(0), 0..12);
    assert_eq!(odd.half_range(1), 12..30);
    assert_eq!(odd.half_range(0).len() % 6, 0);
    assert_eq!(odd.half_range(1).len() % 6, 0);
    assert_eq!(odd.safe_write_half_for_play_byte(0), 1);
    assert_eq!(odd.safe_write_half_for_play_byte(11), 1);
    assert_eq!(odd.safe_write_half_for_play_byte(12), 0);
    assert_eq!(odd.safe_write_half_for_play_byte(29), 0);
    assert_eq!(odd.safe_write_half_for_play_byte(30), 1); // wraps cleanly
}

#[test]
fn wavert_loop_controller_plays_full_stop_fade_out_before_finishing() {
    // 8 frames per half (64 B per half, 128 B ring), 40 full-scale source frames.
    let layout = WaveRtRingLayout::new(128, 8).unwrap();
    let frames = vec![[1_000_000_000_i32, -1_000_000_000_i32]; 40];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ring = vec![0xAA_u8; 128];
    let mut ctrl = WaveRtLoopController::new(layout);

    // Prefill fills half 0 (8 frames) and half 1 (8 frames); 0 frames have finished DMA yet.
    ctrl.prefill(&mut ring, &mut pump).unwrap();
    assert_eq!(ctrl.frames_played(), 0);

    // Event E_k: DMA finishes half 0 and starts half 1; stop is requested now.
    // Must write fade-out into half 0 and CONTINUE (half 1 is still playing; fade hasn't played).
    let (h, outcome) = ctrl
        .on_notification(&mut ring, &mut pump, true, Some(64))
        .unwrap();
    assert_eq!(h, 0);
    assert_eq!(outcome, WaveRtStepOutcome::Continue);
    assert_eq!(ctrl.frames_played(), 8, "only half 0 finished DMA so far");
    // Verify half 0 actually contains a ramp down (first sample high, last sample near zero).
    let fade_first = i32::from_le_bytes(ring[0..4].try_into().unwrap());
    let fade_last = i32::from_le_bytes(ring[7 * 8..7 * 8 + 4].try_into().unwrap());
    assert_eq!(fade_first, 1_000_000_000);
    assert!(fade_last.abs() < fade_first.abs() / 4);

    // Event E_{k+1}: DMA finishes half 1 and JUST STARTS half 0 (the fade-out!).
    // Must zero half 1 and CONTINUE so the fade-out half actually plays through DMA.
    let (h, outcome) = ctrl
        .on_notification(&mut ring, &mut pump, true, Some(128))
        .unwrap();
    assert_eq!(h, 1);
    assert_eq!(outcome, WaveRtStepOutcome::Continue);
    assert_eq!(&ring[64..128], &[0u8; 64]);
    assert_eq!(ctrl.frames_played(), 16, "half 0 + half 1 completed; fade half now playing");

    // Event E_{k+2}: DMA finishes half 0 (the fade-out!) and enters half 1 (silence).
    // Now it is click-free to finish and transition to KSSTATE_PAUSE / STOP.
    let (h, outcome) = ctrl
        .on_notification(&mut ring, &mut pump, true, Some(192))
        .unwrap();
    assert_eq!(h, 0);
    assert_eq!(outcome, WaveRtStepOutcome::Finished);
    assert_eq!(&ring[0..64], &[0u8; 64]);
    assert_eq!(
        ctrl.frames_played(),
        24,
        "exact frames played = half 0 (8) + half 1 (8) + fade half (8)"
    );
}

#[test]
fn wavert_loop_controller_resyncs_from_hardware_position_after_coalesced_notification() {
    let layout = WaveRtRingLayout::new(64, 8).unwrap(); // 4 frames per half
    let frames = vec![[100_i32, -100_i32]; 20];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ring = vec![0u8; 64];
    let mut ctrl = WaveRtLoopController::new(layout);

    ctrl.prefill(&mut ring, &mut pump).unwrap();

    // Normal wake #1: play cursor in half 1 (pos = 36) -> writes half 0.
    let (h1, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(36))
        .unwrap();
    assert_eq!(h1, 0);
    assert_eq!(ctrl.frames_played(), 4);

    // Coalesced notification! Two half-periods elapsed so play cursor is back in half 1 (pos = 100 % 64 = 36).
    // Blind toggle would wrongly pick half 1 (which DMA is currently reading!); hardware position picks half 0.
    let (h2, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(100))
        .unwrap();
    assert_eq!(h2, 0, "must pick safe half opposite DMA play cursor");
    // Both half 1 (from prefill) and half 0 (from wake #1) played during the coalesced interval.
    assert_eq!(ctrl.frames_played(), 12);
}

#[test]
fn wavert_loop_controller_recovers_from_nonblocking_underrun_without_truncating_track() {
    let layout = WaveRtRingLayout::new(64, 8).unwrap(); // 4 frames per half
    let mut sequence = vec![
        StreamBlock::Ready(InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[1, -1]; 8], // fills both prefill halves
        }),
        StreamBlock::Underrun, // wake 1: silent half, NOT EOF
        StreamBlock::Underrun, // wake 2: silent half, NOT EOF
        StreamBlock::Ready(InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[2, -2]; 4], // wake 3: recovers seamlessly!
        }),
        StreamBlock::EndOfTrack,
    ]
    .into_iter();

    let mut pump = StreamingWaveRtPump::new(384_000, DacWireFormat::Int32, move || {
        sequence.next().unwrap_or(StreamBlock::EndOfTrack)
    });
    let mut ring = vec![0xFF_u8; 64];
    let mut ctrl = WaveRtLoopController::new(layout);

    ctrl.prefill(&mut ring, &mut pump).unwrap();

    // Wake 1 (half 0): underrun -> fills half 0 with silence, MUST Continue!
    let (h1, out1) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(32))
        .unwrap();
    assert_eq!(h1, 0);
    assert_eq!(out1, WaveRtStepOutcome::Continue);
    assert_eq!(&ring[0..32], &[0u8; 32]);
    assert_eq!(ctrl.frames_played(), 4);

    // Wake 2 (half 1): second underrun -> fills half 1 with silence, MUST still Continue!
    let (h2, out2) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(64))
        .unwrap();
    assert_eq!(h2, 1);
    assert_eq!(out2, WaveRtStepOutcome::Continue);
    assert_eq!(&ring[32..64], &[0u8; 32]);
    assert_eq!(ctrl.frames_played(), 8);

    // Wake 3 (half 0): producer recovers with 4 frames of [2, -2]!
    let (h3, out3) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(96))
        .unwrap();
    assert_eq!(h3, 0);
    assert_eq!(out3, WaveRtStepOutcome::Continue);
    assert_eq!(&ring[0..4], &2_i32.to_le_bytes());

    // Wake 4 (half 1): EndOfTrack -> trailing_silent_halves = 1, Continue while recovered half 0 plays.
    let (_, out4) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(128))
        .unwrap();
    assert_eq!(out4, WaveRtStepOutcome::Continue);

    // Wake 5 (half 0): recovered half 0 finished DMA -> trailing_silent_halves = 2 -> Finished!
    let (_, out5) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(160))
        .unwrap();
    assert_eq!(out5, WaveRtStepOutcome::Finished);
    assert_eq!(ctrl.frames_played(), 12, "all 8 + 4 audio frames must play cleanly");
}

#[test]
fn wavert_loop_controller_handles_lagging_play_offset_and_boundary_jitter_without_false_coalescing()
{
    let layout = WaveRtRingLayout::new(64, 8).unwrap(); // 4 frames per half (half_len = 32)
    let frames = vec![[42_i32, -42_i32]; 64];

    // 1. Constant PlayOffset lag (10 bytes behind each 32-byte boundary, like usbaudio2.sys).
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ring = vec![0u8; 64];
    let mut ctrl = WaveRtLoopController::new(layout);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    let (h1, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(22)) // 32 - 10
        .unwrap();
    let (h2, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(54)) // 64 - 10
        .unwrap();
    let (h3, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(86)) // 96 - 10
        .unwrap();
    let (h4, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(118)) // 128 - 10
        .unwrap();
    assert_eq!(
        [h1, h2, h3, h4],
        [0, 1, 0, 1],
        "lagging PlayOffset must not flip into the active DMA half"
    );
    assert_eq!(
        ctrl.frames_played(),
        16,
        "4 normal halves = 16 frames (no false coalescing credit)"
    );

    // Now simulate a REAL coalesced wake (1 notification missed) WHILE still lagging by 10 bytes:
    // next normal wake would be 150 (160 - 10); missed one -> arrives at 182 (192 - 10), delta = 64.
    let (h5, _) = ctrl
        .on_notification(&mut ring, &mut pump, false, Some(182))
        .unwrap();
    assert_eq!(
        h5, 1,
        "real coalesced wake with constant lag must resync to half 1"
    );
    assert_eq!(
        ctrl.frames_played(),
        24,
        "coalesced wake must credit both halves (16 + 8)"
    );

    // 2. Coarse boundary jitter (position 1 byte short of boundary on wake 2).
    let mut pump2 = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl2 = WaveRtLoopController::new(layout);
    ctrl2.prefill(&mut ring, &mut pump2).unwrap();
    let (j1, _) = ctrl2
        .on_notification(&mut ring, &mut pump2, false, Some(32))
        .unwrap();
    let (j2, _) = ctrl2
        .on_notification(&mut ring, &mut pump2, false, Some(63)) // 1 byte short of 64!
        .unwrap();
    let (j3, _) = ctrl2
        .on_notification(&mut ring, &mut pump2, false, Some(95))
        .unwrap();
    assert_eq!([j1, j2, j3], [0, 1, 0]);
    assert_eq!(ctrl2.frames_played(), 12);

    // 3. Modulo-buf_len wrapping POSITIONREGISTER (normal wrap + full-ring coalesced wrap).
    let mut pump3 = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl3 = WaveRtLoopController::new(layout);
    ctrl3.prefill(&mut ring, &mut pump3).unwrap();
    let (r1, _) = ctrl3
        .on_notification(&mut ring, &mut pump3, false, Some(36))
        .unwrap();
    let (r2, _) = ctrl3
        .on_notification(&mut ring, &mut pump3, false, Some(4)) // wrapped normally
        .unwrap();
    assert_eq!([r1, r2], [0, 1]);
    assert_eq!(ctrl3.frames_played(), 8);
    // Coalesced full ring on wrapping register: DMA wrapped all the way back to pos=6 (in half 0).
    let (r3, _) = ctrl3
        .on_notification(&mut ring, &mut pump3, false, Some(6))
        .unwrap();
    assert_eq!(r3, 1, "wrapping register full-ring coalesce must resync to safe half 1");
    assert_eq!(ctrl3.frames_played(), 16);
}

#[test]
fn wavert_loop_controller_does_not_misclassify_late_non_coalesced_wake_as_coalesced() {
    // Review #3 Finding #1: ring 64 B (half_len = 32, 4 frames/half).
    let layout = WaveRtRingLayout::new(64, 8).unwrap();
    let frames = vec![[11_i32, -11_i32]; 64];
    let mut ring = vec![0u8; 64];

    // Scenario A: normal wake 1 at pos=36 (writes half 0), then LATE wake 2 at pos=86
    // (DMA is at 86%64=22, inside half 0 -> only 1 boundary crossed!).
    // Old rounded-delta heuristic saw delta=50>=48, falsely classified 2 halves, overwrote half 0,
    // falsely credited half 0's unplayed frames, and inverted the toggle permanently.
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    let (h1, _) = ctrl.on_notification(&mut ring, &mut pump, false, Some(36)).unwrap();
    assert_eq!(h1, 0);
    assert_eq!(ctrl.frames_played(), 4);

    let (h2, _) = ctrl.on_notification(&mut ring, &mut pump, false, Some(86)).unwrap();
    assert_eq!(h2, 1, "DMA is at byte 22 in half 0 -> MUST write safe half 1");
    assert_eq!(
        ctrl.frames_played(),
        8,
        "only 1 boundary crossed -> must credit 4 frames, NOT 8"
    );

    // Next on-time wake at pos=98 (DMA in half 1) must stay in sync and write half 0.
    let (h3, _) = ctrl.on_notification(&mut ring, &mut pump, false, Some(98)).unwrap();
    assert_eq!(h3, 0);
    assert_eq!(ctrl.frames_played(), 12);

    // Scenario B: first wake late at pos=50 (DMA in half 1 -> boundary 1, NOT 2).
    let mut pump2 = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl2 = WaveRtLoopController::new(layout);
    ctrl2.prefill(&mut ring, &mut pump2).unwrap();

    let (f1, _) = ctrl2.on_notification(&mut ring, &mut pump2, false, Some(50)).unwrap();
    assert_eq!(f1, 0, "DMA is at byte 50 in half 1 -> MUST write safe half 0");
    assert_eq!(ctrl2.frames_played(), 4, "first wake late at 50 crossed 1 boundary, not 2");
    let (f2, _) = ctrl2.on_notification(&mut ring, &mut pump2, false, Some(68)).unwrap();
    assert_eq!(f2, 1);
    assert_eq!(ctrl2.frames_played(), 8);
}

#[test]
fn wavert_loop_controller_resyncs_coalesced_first_wake_on_modular_and_unavailable_sources() {
    let layout = WaveRtRingLayout::new(64, 8).unwrap();
    let frames = vec![[22_i32, -22_i32]; 64];
    let mut ring = vec![0u8; 64];
    let half_dur = Duration::from_millis(50);

    // Review #3 Finding #6A: Modular position source, first wake coalesced (real pos=70 -> mod 64 = 6).
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout)
        .with_half_duration(half_dur)
        .with_wrap_mode(PositionWrapMode::Modular);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    let (h1, _) = ctrl
        .on_notification_at(
            &mut ring,
            &mut pump,
            false,
            Some(6),
            Some(Duration::from_millis(110)), // 2.2 halves elapsed
        )
        .unwrap();
    assert_eq!(h1, 1, "DMA wrapped into half 0 at pos=6 -> must write safe half 1");
    assert_eq!(ctrl.frames_played(), 8, "coalesced first wake completed both halves");

    // Subsequent normal wake at pos=36 (half 1) -> writes half 0; half 1 is still playing so
    // frames_played remains 8 until half 1 completes on wake 3.
    let (h2, _) = ctrl
        .on_notification_at(
            &mut ring,
            &mut pump,
            false,
            Some(36),
            Some(Duration::from_millis(156)),
        )
        .unwrap();
    assert_eq!(h2, 0);
    assert_eq!(ctrl.frames_played(), 8);

    // Wake 3 at pos=4 (half 0): half 1 written on wake 1 has now completed DMA -> 12 frames played.
    let (h3, _) = ctrl
        .on_notification_at(
            &mut ring,
            &mut pump,
            false,
            Some(4),
            Some(Duration::from_millis(206)),
        )
        .unwrap();
    assert_eq!(h3, 1);
    assert_eq!(ctrl.frames_played(), 12);

    // Review #3 Finding #6B: Unavailable position source uses wall-clock boundary stepping so a
    // coalesced wake resyncs instead of permanently inverting, while a late wake does NOT misfire.
    let mut pump2 = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl2 = WaveRtLoopController::new(layout).with_half_duration(half_dur);
    ctrl2.prefill(&mut ring, &mut pump2).unwrap();

    // Wake 1 on time (50 ms) -> half 0.
    let (u1, _) = ctrl2
        .on_notification_at(&mut ring, &mut pump2, false, None, Some(Duration::from_millis(52)))
        .unwrap();
    assert_eq!(u1, 0);
    assert_eq!(ctrl2.frames_played(), 4);

    // Wake 2 late by 35 ms (135 ms = 2.7 halves -> 1.7 halves since boundary 1 -> NOT coalesced!).
    let (u2, _) = ctrl2
        .on_notification_at(&mut ring, &mut pump2, false, None, Some(Duration::from_millis(135)))
        .unwrap();
    assert_eq!(u2, 1, "late-but-not-coalesced wake on Unavailable source must NOT invert");
    assert_eq!(ctrl2.frames_played(), 8);

    // Wake 3 coalesced (missed boundary 3 at 150 ms, arrives at 205 ms = boundary 4 -> 2 halves since boundary 2!).
    let (u3, _) = ctrl2
        .on_notification_at(&mut ring, &mut pump2, false, None, Some(Duration::from_millis(205)))
        .unwrap();
    assert_eq!(u3, 1, "coalesced wake on Unavailable source must skip to boundary 4 -> half 1");
    assert_eq!(ctrl2.frames_played(), 16);
}

#[test]
fn wavert_loop_controller_lagging_modular_play_offset_does_not_false_trigger_wrap_override() {
    // Review #3 Finding #25: modular PlayOffset-only driver with constant 12 B lag on 64 B ring.
    let layout = WaveRtRingLayout::new(64, 8).unwrap();
    let frames = vec![[33_i32, -33_i32]; 64];
    let mut ring = vec![0u8; 64];
    let half_dur = Duration::from_millis(50);

    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout).with_half_duration(half_dur);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    // Wake 1 (real 32, lag 12 -> reports 20) -> writes half 0.
    let (h1, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(20), Some(Duration::from_millis(50)))
        .unwrap();
    // Wake 2 (real 64, lag 12 -> reports 52) -> writes half 1.
    let (h2, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(52), Some(Duration::from_millis(100)))
        .unwrap();
    // Wake 3 late (real 126 = 64 + 62, lag 12 -> 114 % 64 = 50) -> wrapped, writes half 0.
    let (h3, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(50), Some(Duration::from_millis(197)))
        .unwrap();
    // Wake 4 on time (real 129, lag 12 -> 117 % 64 = 53, raw_delta = 3 < 16) -> MUST write half 1, NOT coalesce!
    let (h4, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(53), Some(Duration::from_millis(202)))
        .unwrap();

    assert_eq!([h1, h2, h3, h4], [0, 1, 0, 1]);
    assert_eq!(ctrl.frames_played(), 16, "no false coalescing credit on lagging wrap sequence");
}

#[test]
fn audio_position_cursor_selector_never_mixes_play_and_write_offset_on_zero_wrap() {
    // Review #3 Finding #26: once WriteOffset is observed non-zero, legitimate modulo wrap to 0
    // MUST return 0 (WriteOffset), never fall back to lagging PlayOffset.
    let mut sel = AudioPositionCursorSelector::new();
    assert_eq!(sel.select(0, 0), 0);
    assert_eq!(sel.select(140_000, 153_600), 153_600);
    assert!(sel.is_write_offset());
    // Wrap boundary: WriteOffset wraps to 0 while PlayOffset still lags at 290_000!
    assert_eq!(
        sel.select(290_000, 0),
        0,
        "must NOT fall back to PlayOffset when WriteOffset wraps to 0"
    );
    assert_eq!(sel.select(140_000, 153_600), 153_600);

    // Driver where WriteOffset is always 0: locks onto PlayOffset permanently.
    let mut sel_play = AudioPositionCursorSelector::new();
    assert_eq!(sel_play.select(22, 0), 22);
    assert!(!sel_play.is_write_offset());
    assert_eq!(sel_play.select(54, 0), 54);
}

#[test]
fn wavert_fade_out_applies_pre_encode_tpdf_dither_on_reduced_depths() {
    // Review #3 Finding #9: fade-out gain must be applied BEFORE reduced-depth encoding so
    // Int16 / Int24* paths run TPDF dither once after attenuation rather than re-rounding
    // already-quantized wire bytes without dither.
    let quiet_sample = 0x0001_0000_i32; // +1 LSB in Int16 (exact 16-bit input)
    let frames = vec![[quiet_sample, quiet_sample]; 256];

    // 1. WaveRtFramePump on Int16: fade ramp crosses 0.5 LSB across many frames; undithered
    // rounding would produce a deterministic flat step (0 or 1), while pre-encode TPDF dither
    // produces stochastic {-1, 0, +1, +2} distribution across the tail and decorrelated L vs R.
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int16);
    let mut out16 = vec![0u8; frames.len() * 4];
    let written = pump.fill_fade_out_slice(&mut out16);
    assert_eq!(written, frames.len());
    let mut lr_differ_count = 0usize;
    let mut tail_nonzero = 0usize;
    for i in 0..frames.len() {
        let l = i16::from_le_bytes(out16[i * 4..i * 4 + 2].try_into().unwrap());
        let r = i16::from_le_bytes(out16[i * 4 + 2..i * 4 + 4].try_into().unwrap());
        if l != r {
            lr_differ_count += 1;
        }
        // In the final 25% of the ramp (gain < 0.25), undithered round(1 * gain) is ALWAYS 0,
        // whereas TPDF dither occasionally produces ±1 LSB.
        if i >= (frames.len() * 3) / 4 && (l != 0 || r != 0) {
            tail_nonzero += 1;
        }
    }
    assert!(
        lr_differ_count > 30,
        "expected stereo-decorrelated TPDF dither during Int16 fade-out (lr_differ={lr_differ_count})"
    );
    assert!(
        tail_nonzero > 5,
        "expected TPDF dither activity in quiet Int16 fade tail (tail_nonzero={tail_nonzero})"
    );

    // 2. StreamingWaveRtPump on Int24In32: low byte must remain 0 and L/R must be decorrelated.
    let mut once = Some(InterleavedInt32Buffer {
        sample_rate_hz: 384_000,
        frames: vec![[0x0000_0100_i32, 0x0000_0100_i32]; 128], // +1 LSB in 24-bit
    });
    let mut spump = StreamingWaveRtPump::new(384_000, DacWireFormat::Int24In32, move || {
        once.take()
            .map(StreamBlock::Ready)
            .unwrap_or(StreamBlock::EndOfTrack)
    });
    let mut out24 = vec![0u8; 128 * 8];
    let sw = spump.fill_fade_out_slice(&mut out24);
    assert_eq!(sw, 128);
    let mut lr24_differ = 0usize;
    for i in 0..128 {
        let l = i32::from_le_bytes(out24[i * 8..i * 8 + 4].try_into().unwrap());
        let r = i32::from_le_bytes(out24[i * 8 + 4..i * 8 + 8].try_into().unwrap());
        assert_eq!(l & 0xFF, 0, "Int24In32 low byte must be zero after dithered fade");
        assert_eq!(r & 0xFF, 0, "Int24In32 low byte must be zero after dithered fade");
        if l != r {
            lr24_differ += 1;
        }
    }
    assert!(
        lr24_differ > 15,
        "expected stereo-decorrelated TPDF dither in StreamingWaveRtPump Int24In32 fade-out (lr24_differ={lr24_differ})"
    );
}

#[cfg(windows)]
#[test]
fn memory_lock_guard_refcounts_shared_pages_across_overlapping_guards() {
    // Serialize against both other MemoryLockGuard unit tests and live play_full_memory tests
    // so process-global locked-page count assertions never race (Review #3 Finding #10).
    let _hw_lock = HARDWARE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _mem_lock = MEMORY_LOCK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let cap = MemoryLockGuard::working_set_growth_cap_bytes();
    let max_cap = (4u64 << 30).min(usize::MAX as u64) as usize;
    assert!(
        (512 * 1024 * 1024..=max_cap).contains(&cap),
        "expected dynamic VirtualLock cap in [512 MiB, 4 GiB], got {cap}"
    );

    let baseline = MemoryLockGuard::active_locked_page_count();
    let buf = vec![[0_i32, 0_i32]; 2048];
    let g1 = MemoryLockGuard::lock_slice(&buf[0..1024]);
    let g2 = MemoryLockGuard::lock_slice(&buf[512..1536]);
    assert!(g1.is_locked());
    assert!(g2.is_locked());
    let locked_both = MemoryLockGuard::active_locked_page_count();
    assert!(locked_both > baseline);
    drop(g1);
    assert!(
        MemoryLockGuard::active_locked_page_count() > baseline,
        "dropping first overlapping guard must keep second guard's pages locked"
    );
    drop(g2);
    assert_eq!(
        MemoryLockGuard::active_locked_page_count(),
        baseline,
        "dropping last guard must release all refcounted pages back to baseline"
    );
}

#[test]
fn wavert_loop_controller_reestimates_lag_after_late_or_stale_first_wake() {
    // Review #4 Finding #1 (HIGH):
    // Ring 6400 B (half 3200 B = 50 ms, 400 frames/half), PlayOffset-only driver with 960 B
    // (15 ms = 0.3 half) constant lag. First wake arrives 14 ms late (real 4096 -> reports 3136),
    // underestimating initial lag as 64 B instead of 960 B. All subsequent on-time wakes at
    // real k*3200 + 64 (reporting k*3200 - 896) must STILL write the safe opposite half!
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let frames = vec![[77_i32, -77_i32]; 8000];
    let half_dur = Duration::from_millis(50);

    for wrap_mode in [PositionWrapMode::Monotonic, PositionWrapMode::Modular] {
        let mut ring = vec![0u8; 6400];
        let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
        let mut ctrl = WaveRtLoopController::new(layout)
            .with_half_duration(half_dur)
            .with_wrap_mode(wrap_mode);
        ctrl.prefill(&mut ring, &mut pump).unwrap();

        // Wake 1: 14 ms late (t = 64 ms), real DMA = 4096 (half 1), reports 3136.
        let pos1 = if wrap_mode == PositionWrapMode::Modular {
            3136 % 6400
        } else {
            3136
        };
        let (h1, out1) = ctrl
            .on_notification_at(&mut ring, &mut pump, false, Some(pos1), Some(Duration::from_millis(64)))
            .unwrap();
        assert_eq!(out1, WaveRtStepOutcome::Continue);
        assert_eq!(h1, 0, "wake 1 DMA in half 1 -> must write safe half 0");

        // Wakes 2..=9: on-time at real k*3200 + 64 (1 ms after boundary), reporting k*3200 - 896.
        for k in 2u64..=9 {
            let real = k * 3200 + 64;
            let reported = real - 960;
            let pos = if wrap_mode == PositionWrapMode::Modular {
                reported % 6400
            } else {
                reported
            };
            let t = Duration::from_millis(k * 50 + 1);
            let (h, out) = ctrl
                .on_notification_at(&mut ring, &mut pump, false, Some(pos), Some(t))
                .unwrap();
            let dma_half = ((real / 3200) & 1) as usize;
            let safe_half = dma_half ^ 1;
            assert_eq!(out, WaveRtStepOutcome::Continue);
            assert_eq!(
                h, safe_half,
                "wrap_mode={wrap_mode:?} wake {k}: must write safe half {safe_half}, not active DMA half {dma_half}"
            );
        }
        assert_eq!(ctrl.frames_played(), 9 * 400);
    }

    // Converse: stale/coarse first wake overestimates lag (reports 2200 -> 1000 B instead of
    // true 100 B lag). After two on-time wakes the estimator drops the stale outlier so a late
    // wake at boundary 4 + 2500 B is NOT falsely treated as crossing boundary 5.
    let mut ring = vec![0u8; 6400];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout)
        .with_half_duration(half_dur)
        .with_wrap_mode(PositionWrapMode::Monotonic);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    let (h1, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(2200), Some(Duration::from_millis(50)))
        .unwrap();
    let (h2, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(6364), Some(Duration::from_millis(101)))
        .unwrap();
    let (h3, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(9564), Some(Duration::from_millis(151)))
        .unwrap();
    // Wake 4 late (real = 12800 + 2500 = 15300 in half 0, true lag 100 -> reports 15200, t = 239 ms).
    let (h4, _) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(15200), Some(Duration::from_millis(239)))
        .unwrap();
    assert_eq!([h1, h2, h3, h4], [0, 1, 0, 1]);
    assert_eq!(ctrl.frames_played(), 4 * 400);
}

#[test]
fn wavert_loop_controller_recovers_multi_half_stalls_on_modular_register() {
    // Review #4 Finding #6: Register (modular) source, ring 6400 B (half 3200 B = 50 ms).
    // Stall of ~2.8 halves (from t=251 ms pos=3263 bidx=5 to t=390 ms real=24960 pos=5760 bidx=7)
    // and ~4.8 halves must infer exact full-ring wraps, mark coalesced, and write the safe half.
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let frames = vec![[55_i32, -55_i32]; 12000];
    let mut ring = vec![0u8; 6400];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout)
        .with_half_duration(Duration::from_millis(50))
        .with_wrap_mode(PositionWrapMode::Modular);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    // Wakes 1..=5 on time: t = 51, 101, 151, 201, 251 ms.
    for k in 1u64..=5 {
        let real = k * 3200 + 63;
        let (h, out) = ctrl
            .on_notification_at(
                &mut ring,
                &mut pump,
                false,
                Some(real % 6400),
                Some(Duration::from_millis(k * 50 + 1)),
            )
            .unwrap();
        assert_eq!(out, WaveRtStepOutcome::Continue);
        assert_eq!(h, ((k as usize) & 1) ^ 1);
    }
    assert_eq!(ctrl.frames_played(), 5 * 400);

    // Stall of 2.8 halves -> wake at t=390 ms, real=24960 (bidx 7, half 1), pos=5760 >= prev_raw=3263.
    let (h_stall, out_stall) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(5760), Some(Duration::from_millis(390)))
        .unwrap();
    assert_eq!(out_stall, WaveRtStepOutcome::Continue);
    assert_eq!(h_stall, 0, "DMA is in half 1 (bidx 7) -> must write safe half 0");
    assert_eq!(ctrl.frames_played(), 7 * 400, "coalesced 2-half stall credits both halves");

    // Next on-time wake at t=401 ms, real=25664 (bidx 8, half 0), pos=64 -> writes safe half 1.
    let (h_after, out_after) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(64), Some(Duration::from_millis(401)))
        .unwrap();
    assert_eq!(out_after, WaveRtStepOutcome::Continue);
    assert_eq!(h_after, 1);

    // Stall of 4.8 halves -> wake at t=641 ms, real=41024 (bidx 12, half 0), pos=2624 >= 64.
    let (h_big, out_big) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(2624), Some(Duration::from_millis(641)))
        .unwrap();
    assert_eq!(out_big, WaveRtStepOutcome::Continue);
    assert_eq!(h_big, 1, "DMA is in half 0 (bidx 12) -> must write safe half 1");
}

#[test]
fn wavert_loop_controller_unavailable_clock_anchor_resists_steady_latency_and_stale_events() {
    // Review #4 Finding #7: Unavailable position source with 50 ms halves and consistent 10 ms
    // wake latency (t = 60, 110, ..., 460). Boundary 500 is missed; merged wake arrives at
    // t = 550.2 ms (DMA in half 1, boundary 11). Must advance 2 halves and write safe half 0,
    // and ignore a stale duplicate wake arriving at t = 555 ms.
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let frames = vec![[88_i32, -88_i32]; 12000];
    let mut ring = vec![0u8; 6400];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout).with_half_duration(Duration::from_millis(50));
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    for k in 1u64..=9 {
        let (h, out) = ctrl
            .on_notification_at(
                &mut ring,
                &mut pump,
                false,
                None,
                Some(Duration::from_millis(k * 50 + 10)),
            )
            .unwrap();
        assert_eq!(out, WaveRtStepOutcome::Continue);
        assert_eq!(h, ((k as usize) & 1) ^ 1);
    }
    assert_eq!(ctrl.frames_played(), 9 * 400);

    // Missed boundary 10 (500 ms); coalesced wake at 550.2 ms -> boundary 11 (DMA in half 1).
    let (h_coal, out_coal) = ctrl
        .on_notification_at(
            &mut ring,
            &mut pump,
            false,
            None,
            Some(Duration::from_micros(550_200)),
        )
        .unwrap();
    assert_eq!(out_coal, WaveRtStepOutcome::Continue);
    assert_eq!(h_coal, 0, "DMA at 550.2 ms is in half 1 -> must write safe half 0");
    assert_eq!(ctrl.frames_played(), 11 * 400);

    // Stale duplicate event right after catch-up at 555 ms -> must be ignored as Spurious!
    let ring_before = ring.clone();
    let (_, out_stale) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, None, Some(Duration::from_millis(555)))
        .unwrap();
    assert_eq!(out_stale, WaveRtStepOutcome::Spurious);
    assert_eq!(ctrl.spurious_wakes(), 1);
    assert_eq!(ctrl.frames_played(), 11 * 400, "spurious wake must not credit frames");
    assert_eq!(ring, ring_before, "spurious wake must not overwrite either half");

    // Subsequent on-time wakes at 600.5, 650.5 ms remain strictly in phase.
    let (h12, out12) = ctrl
        .on_notification_at(
            &mut ring,
            &mut pump,
            false,
            None,
            Some(Duration::from_micros(600_500)),
        )
        .unwrap();
    assert_eq!(out12, WaveRtStepOutcome::Continue);
    assert_eq!(h12, 1);
    let (h13, out13) = ctrl
        .on_notification_at(
            &mut ring,
            &mut pump,
            false,
            None,
            Some(Duration::from_micros(650_500)),
        )
        .unwrap();
    assert_eq!(out13, WaveRtStepOutcome::Continue);
    assert_eq!(h13, 0);
}

#[test]
fn wavert_loop_controller_ignores_spurious_no_boundary_wakes_without_overwriting_pending_half() {
    // Review #4 Finding #18: Spurious driver wake mid-half (e.g. t=60 ms after wake 1 at t=50 ms)
    // or immediately after a coalesced wake (t=206 ms after t=201 ms) must NOT overwrite the
    // pending half and must NOT credit unplayed frames.
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let frames = vec![[99_i32, -99_i32]; 8000];
    let mut ring = vec![0u8; 6400];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout)
        .with_half_duration(Duration::from_millis(50))
        .with_wrap_mode(PositionWrapMode::Modular);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    // Wake 1 on time at t=50 ms, pos=3200 -> writes half 0, credits 400 frames.
    let (h1, out1) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(3200), Some(Duration::from_millis(50)))
        .unwrap();
    assert_eq!((h1, out1), (0, WaveRtStepOutcome::Continue));
    assert_eq!(ctrl.frames_played(), 400);

    // Spurious wake at t=60 ms, pos=3840 (still inside half 1, no boundary crossed).
    let ring_snapshot = ring.clone();
    let (_, out_spurious) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(3840), Some(Duration::from_millis(60)))
        .unwrap();
    assert_eq!(out_spurious, WaveRtStepOutcome::Spurious);
    assert_eq!(ctrl.spurious_wakes(), 1);
    assert_eq!(ctrl.frames_played(), 400, "spurious wake must not credit pending half");
    assert_eq!(ring, ring_snapshot, "spurious wake must not overwrite pending half 0");

    // Real wake 2 at t=100 ms, pos=0 (wrapped into half 0) -> writes half 1, credits 400 frames.
    let (h2, out2) = ctrl
        .on_notification_at(&mut ring, &mut pump, false, Some(0), Some(Duration::from_millis(100)))
        .unwrap();
    assert_eq!((h2, out2), (1, WaveRtStepOutcome::Continue));
    assert_eq!(ctrl.frames_played(), 800);
}

// ---------------------------------------------------------------------------
// 2. Live Windows Kernel Streaming (WDM-KS) Hardware Tests (Schiit Gungnir 2)
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[test]
fn live_wdm_ks_probes_notification_get_and_hardware_position_on_gungnir2() {
    let _lock = HARDWARE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sink = WdmKsSink::open("Gungnir 2").expect("failed to locate Gungnir 2 WDM-KS device");
    let probe = sink
        .probe_wavert_hardware_properties(384_000, DacWireFormat::Int32)
        .expect("live WaveRT hardware probe failed on Gungnir 2");
    println!("=== Live Gungnir 2 WaveRT Hardware Probe ===");
    println!("  {probe:?}");
    assert_eq!(probe.buffer_bytes % 8, 0);
    assert_eq!(probe.half_bytes % 8, 0);
    assert!(
        probe.register_get_ok && probe.unregister_get_ok,
        "expected WDK-standard KSPROPERTY_TYPE_GET notification register/unregister to succeed on Gungnir 2"
    );
    assert!(
        probe.position_register_supported || probe.audio_position_supported,
        "expected hardware WaveRT position query support on Gungnir 2"
    );
    assert_eq!(
        probe.sample_play_offsets.len(),
        4,
        "expected 4 live hardware play-offset samples"
    );

    // Feed the real hardware positions into WaveRtLoopController and verify zero false
    // coalescing: halves must strictly alternate [0, 1, 0, 1] and step by ~1 half per wake.
    let layout = WaveRtRingLayout::new(probe.buffer_bytes, 8).unwrap();
    let silent = vec![[0_i32, 0_i32]; (probe.buffer_bytes / 8) * 4];
    let mut pump = WaveRtFramePump::new(&silent, DacWireFormat::Int32);
    let mut ring = vec![0u8; probe.buffer_bytes];
    let mut ctrl = WaveRtLoopController::new(layout);
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    let mut chosen_halves = Vec::new();
    for &(pos, _) in &probe.sample_play_offsets {
        let (h, _) = ctrl
            .on_notification(&mut ring, &mut pump, false, Some(pos))
            .unwrap();
        chosen_halves.push(h);
    }
    assert_eq!(
        chosen_halves,
        vec![0, 1, 0, 1],
        "live Gungnir 2 notifications must alternate cleanly without false coalescing: offsets={:?}",
        probe.sample_play_offsets
    );
}

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

#[test]
fn review5_unavailable_anchor_tracks_plus_minus_100ppm_drift_over_20000_halves() {
    // Review #5 Finding #2: Unavailable position source, 50 ms nominal halves, steady 2 ms wake
    // latency, audio crystal drifting -100 ppm (slower) AND +100 ppm (faster) over 20,000 halves,
    // including periodic coalesced wakes landing 0.2 ms after a boundary. Must achieve 0 wrong-half
    // writes across all 20,000 halves in BOTH drift directions!
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let half_nom_ns: f64 = 50_000_000.0;
    let half_dur = Duration::from_millis(50);

    for ppm in [-100.0_f64, 100.0_f64] {
        // Real crystal half duration: slow crystal (-100 ppm) has LONGER half duration (+100 ppm
        // in wall-clock ns per half); fast crystal (+100 ppm) has SHORTER half duration.
        let real_half_ns = half_nom_ns * (1.0 - ppm * 1e-6);
        let mut ring = vec![0u8; 6400];
        let mut frames_pool = vec![[11_i32, -11_i32]; 400];
        let mut ctrl = WaveRtLoopController::new(layout).with_half_duration(half_dur);
        {
            let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
            ctrl.prefill(&mut ring, &mut pump).unwrap();
        }

        let mut k: u64 = 1;
        while k <= 20_000 {
            let is_coalesced = k > 100 && k % 2500 == 0;
            if is_coalesced {
                k += 1; // miss 1 boundary, wake lands 0.2 ms after boundary k
            }
            let wake_lat_ns: f64 = if is_coalesced { 200_000.0 } else { 2_000_000.0 };
            let now_ns = ((k as f64) * real_half_ns + wake_lat_ns).round() as u64;
            // Refill fresh audio pump so controller stays in active Continue state.
            frames_pool[0][0] = (k & 0x7FFF) as i32;
            let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
            let (written_half, outcome) = ctrl
                .on_notification_at(
                    &mut ring,
                    &mut pump,
                    false,
                    None,
                    Some(Duration::from_nanos(now_ns)),
                )
                .unwrap();
            let dma_half = (k & 1) as usize;
            let safe_half = dma_half ^ 1;
            assert_eq!(
                outcome,
                WaveRtStepOutcome::Continue,
                "ppm={ppm} k={k} coalesced={is_coalesced}: unexpected outcome {outcome:?}"
            );
            assert_eq!(
                written_half, safe_half,
                "ppm={ppm} k={k} coalesced={is_coalesced}: wrote active DMA half {dma_half} instead of safe half {safe_half}"
            );
            k += 1;
        }
        assert_eq!(ctrl.spurious_wakes(), 0, "ppm={ppm}: expected 0 spurious wakes");
    }
}

#[test]
fn review5_unavailable_early_first_wake_does_not_reject_or_invert_phase() {
    // Review #5 Finding #11: DMA starts 10 ms before `run_start` is captured, so wake 1 arrives
    // at 41 ms (> half/8 early relative to 50 ms). Controller must NEVER reject wake 1 as
    // Spurious, must anchor boundary 0 from it, and keep all subsequent wakes strictly in phase.
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let frames = vec![[33_i32, -33_i32]; 12_000];
    let mut ring = vec![0u8; 6400];
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WaveRtLoopController::new(layout).with_half_duration(Duration::from_millis(50));
    ctrl.prefill(&mut ring, &mut pump).unwrap();

    for k in 1u64..=20 {
        // Real boundaries at k*50 - 10 ms, wakes arrive 1 ms after real boundary.
        let t_ms = k * 50 - 9;
        let (h, out) = ctrl
            .on_notification_at(&mut ring, &mut pump, false, None, Some(Duration::from_millis(t_ms)))
            .unwrap();
        let expected_safe = ((k as usize) & 1) ^ 1;
        assert_eq!(out, WaveRtStepOutcome::Continue, "wake {k} at {t_ms} ms");
        assert_eq!(
            h, expected_safe,
            "wake {k} at {t_ms} ms wrote wrong half {h} (expected safe half {expected_safe})"
        );
    }
    assert_eq!(ctrl.spurious_wakes(), 0);
    assert_eq!(ctrl.frames_played(), 20 * 400);
}

#[test]
fn review5_large_cursor_lag_over_half_over_two_after_late_first_wake_stays_in_phase() {
    // Review #5 Finding #12: AUDIO_POSITION PlayOffset lagging 1700 B, 1920 B, or 2200 B (> half/2
    // on a 3200 B = 50 ms half) with a 14 ms late first wake, followed by on-time wakes.
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let frames = vec![[44_i32, -44_i32]; 12_000];
    let half_dur = Duration::from_millis(50);

    for true_lag in [1700u64, 1920, 2200] {
        for wrap_mode in [PositionWrapMode::Monotonic, PositionWrapMode::Modular] {
            let mut ring = vec![0u8; 6400];
            let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
            let mut ctrl = WaveRtLoopController::new(layout)
                .with_half_duration(half_dur)
                .with_wrap_mode(wrap_mode);
            ctrl.prefill(&mut ring, &mut pump).unwrap();

            // Wake 1: 14 ms late (t = 64 ms), real DMA = 3200 + 896 = 4096.
            let rep1 = 4096u64 - true_lag;
            let pos1 = if wrap_mode == PositionWrapMode::Modular {
                rep1 % 6400
            } else {
                rep1
            };
            let (h1, out1) = ctrl
                .on_notification_at(
                    &mut ring,
                    &mut pump,
                    false,
                    Some(pos1),
                    Some(Duration::from_millis(64)),
                )
                .unwrap();
            assert_eq!(out1, WaveRtStepOutcome::Continue);
            assert_eq!(h1, 0, "lag={true_lag} mode={wrap_mode:?} wake 1");

            // Wakes 2..=15: on-time at real k*3200 + 64 (1 ms after boundary).
            for k in 2u64..=15 {
                let real = k * 3200 + 64;
                let rep = real - true_lag;
                let pos = if wrap_mode == PositionWrapMode::Modular {
                    rep % 6400
                } else {
                    rep
                };
                let (h, out) = ctrl
                    .on_notification_at(
                        &mut ring,
                        &mut pump,
                        false,
                        Some(pos),
                        Some(Duration::from_millis(k * 50 + 1)),
                    )
                    .unwrap();
                let safe_half = ((k as usize) & 1) ^ 1;
                assert_eq!(
                    out,
                    WaveRtStepOutcome::Continue,
                    "lag={true_lag} mode={wrap_mode:?} wake {k}"
                );
                assert_eq!(
                    h, safe_half,
                    "lag={true_lag} mode={wrap_mode:?} wake {k}: wrote active half instead of {safe_half}"
                );
            }
            assert_eq!(ctrl.spurious_wakes(), 0);
            assert_eq!(ctrl.frames_played(), 15 * 400);
        }
    }
}

#[test]
fn review5_unbiased_lag_estimator_and_upper_boundary_guard_survive_randomized_stalls() {
    // Review #5 Finding #13:
    // Part A: exact scenario from Finding #13 — 640 B lag, 0.8 ms steady wake latency, then a
    // stall waking at t = 11100.03 ms (0.03 ms after boundary 222). Must write safe half 1 (DMA
    // in half 0), NOT active half 0!
    let layout = WaveRtRingLayout::new(6400, 8).unwrap();
    let half_dur = Duration::from_millis(50);
    let mut ring = vec![0u8; 6400];
    let mut frames_pool = vec![[66_i32, -66_i32]; 400];
    let mut ctrl = WaveRtLoopController::new(layout)
        .with_half_duration(half_dur)
        .with_wrap_mode(PositionWrapMode::Monotonic);
    {
        let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
        ctrl.prefill(&mut ring, &mut pump).unwrap();
    }

    let true_lag = 640u64;
    for k in 1u64..=219 {
        // 0.8 ms = 51.2 bytes after boundary
        let now_us = k * 50_000 + 800;
        let real = (now_us * 3200) / 50_000;
        let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
        let (h, out) = ctrl
            .on_notification_at(
                &mut ring,
                &mut pump,
                false,
                Some(real - true_lag),
                Some(Duration::from_micros(now_us)),
            )
            .unwrap();
        assert_eq!(out, WaveRtStepOutcome::Continue);
        assert_eq!(h, ((k as usize) & 1) ^ 1);
    }
    // Stall wakes at 11_100.03 ms (0.03 ms after boundary 222 -> real DMA = 710_401.92 -> 710_402).
    let stall_ns: u64 = 11_100_030_000;
    let stall_real: u64 = ((u128::from(stall_ns) * 3200) / 50_000_000) as u64;
    assert_eq!(stall_real / 3200, 222);
    {
        let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
        let (h_stall, out_stall) = ctrl
            .on_notification_at(
                &mut ring,
                &mut pump,
                false,
                Some(stall_real - true_lag),
                Some(Duration::from_nanos(stall_ns)),
            )
            .unwrap();
        assert_eq!(out_stall, WaveRtStepOutcome::Continue);
        assert_eq!(
            h_stall, 1,
            "stall waking 0.03 ms after boundary 222 (DMA in half 0) must write safe half 1"
        );
    }

    // Part B: deterministic randomized stall simulation (30 runs x 300 wakes, 5% stalls of
    // 5..140 ms, lags 640..1500 B, both Monotonic and Modular) — assert 0 wrong-half writes!
    let mut rng: u64 = 0x5EED_CAFE_BABE_1234;
    let mut next_u64 = || {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        rng
    };

    for run in 0..30usize {
        let lag = 640 + (next_u64() % 861); // [640, 1500]
        let wrap_mode = if run % 2 == 0 {
            PositionWrapMode::Monotonic
        } else {
            PositionWrapMode::Modular
        };
        let mut ring = vec![0u8; 6400];
        let mut ctrl = WaveRtLoopController::new(layout)
            .with_half_duration(half_dur)
            .with_wrap_mode(wrap_mode);
        {
            let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
            ctrl.prefill(&mut ring, &mut pump).unwrap();
        }

        let mut cur_bidx: u64 = 0;
        for step in 0..300usize {
            let is_stall = step > 2 && (next_u64() % 20 == 0);
            let extra_us = if is_stall {
                5_000 + (next_u64() % 135_000) // 5..140 ms stall
            } else {
                200 + (next_u64() % 1_500) // 0.2..1.7 ms normal wake latency
            };
            let target_bidx = cur_bidx + 1;
            let now_us = target_bidx * 50_000 + extra_us;
            let real = (now_us * 3200) / 50_000;
            let actual_bidx = real / 3200;
            let rep = real - lag;
            let pos = if wrap_mode == PositionWrapMode::Modular {
                rep % 6400
            } else {
                rep
            };
            frames_pool[0][0] = (step as i32) + 1;
            let mut pump = WaveRtFramePump::new(&frames_pool, DacWireFormat::Int32);
            let (h, out) = ctrl
                .on_notification_at(
                    &mut ring,
                    &mut pump,
                    false,
                    Some(pos),
                    Some(Duration::from_micros(now_us)),
                )
                .unwrap();
            let safe_half = ((actual_bidx as usize) & 1) ^ 1;
            assert_eq!(
                out,
                WaveRtStepOutcome::Continue,
                "run={run} step={step} lag={lag} mode={wrap_mode:?} stall={is_stall}"
            );
            assert_eq!(
                h, safe_half,
                "run={run} step={step} lag={lag} mode={wrap_mode:?} actual_bidx={actual_bidx} extra_us={extra_us}: wrote active DMA half!"
            );
            cur_bidx = actual_bidx;
        }
    }
}
