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
    KsPinConnectBlob, KsRenderDevice, StreamBlock, StreamingWaveRtPump, WaveRtFramePump,
    WaveRtLoopController, WaveRtPumpSource, WaveRtRingLayout, WaveRtStepOutcome, WdmKsSink,
    classify_ks_pin_error, enumerate_wdm_ks_render_devices, select_wdm_ks_device,
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
fn wavert_frame_pump_rounds_reduced_depths_and_zeros_int24_in_32_low_byte() {
    assert_eq!(DacWireFormat::Int32.bytes_per_frame(), 8);
    assert_eq!(DacWireFormat::Int24In32.bytes_per_frame(), 8);
    assert_eq!(DacWireFormat::Int24Packed.bytes_per_frame(), 6);
    assert_eq!(DacWireFormat::Int16.bytes_per_frame(), 4);

    // Non-zero low bytes that round up cleanly without truncation bias.
    let frames = vec![[0x1234_56C0_i32, 0x7FFF_FFFF_i32]];

    let mut p24_32 = WaveRtFramePump::new(&frames, DacWireFormat::Int24In32);
    let mut out24_32 = vec![0xAA_u8; 8];
    assert_eq!(p24_32.fill_slice(&mut out24_32), 1);
    let l = i32::from_le_bytes(out24_32[0..4].try_into().unwrap());
    let r = i32::from_le_bytes(out24_32[4..8].try_into().unwrap());
    assert_eq!(l & 0xFF, 0, "Int24In32 low byte must be zeroed");
    assert_eq!(r & 0xFF, 0, "Int24In32 low byte must be zeroed");
    assert_eq!(l, 0x1234_5700);
    assert_eq!(r, 0x7FFF_FF00);

    // Fade-out on Int24In32 must also keep low byte zero on every frame.
    let fade_frames = vec![[0x4000_0000_i32, -0x4000_0000_i32]; 16];
    let mut pf = WaveRtFramePump::new(&fade_frames, DacWireFormat::Int24In32);
    let mut out_f = vec![0u8; 16 * 8];
    assert_eq!(pf.fill_fade_out_slice(&mut out_f), 16);
    for i in 0..16 {
        assert_eq!(out_f[i * 8], 0, "Int24In32 faded L low byte must be 0");
        assert_eq!(out_f[i * 8 + 4], 0, "Int24In32 faded R low byte must be 0");
    }

    // Int16 rounding: 0x1234_C000 rounds up to 0x1235 instead of truncating to 0x1234.
    let frames16 = vec![[0x1234_C000_i32, i32::MAX]];
    let mut p16 = WaveRtFramePump::new(&frames16, DacWireFormat::Int16);
    let mut out16 = vec![0u8; 4];
    assert_eq!(p16.fill_slice(&mut out16), 1);
    assert_eq!(i16::from_le_bytes(out16[0..2].try_into().unwrap()), 0x1235);
    assert_eq!(i16::from_le_bytes(out16[2..4].try_into().unwrap()), i16::MAX);
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
