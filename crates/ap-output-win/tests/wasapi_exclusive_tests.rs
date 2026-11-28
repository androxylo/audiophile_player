// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Unit and hardware integration tests for the event-driven WASAPI Exclusive backend.

use ap_core::format::DacWireFormat;
use ap_output_win::sink::OutputError;
use ap_output_win::wasapi_exclusive::{
    WasapiLoopController, WasapiRenderDevice, WasapiStepOutcome, aligned_buffer_duration_hns,
    classify_wasapi_hr, negotiate_channel_mask_with_probe, select_wasapi_device,
    should_fallback_wasapi_event_to_push,
};
use ap_output_win::wdm_ks::{StreamBlock, StreamingWaveRtPump, WaveRtFramePump};

#[test]
fn aligned_buffer_duration_hns_matches_msdn_rounding_formula() {
    // 3840 frames at 384 kHz == exactly 10 ms == 100,000 hns.
    assert_eq!(aligned_buffer_duration_hns(3840, 384_000), 100_000);
    // 441 frames at 44.1 kHz == exactly 10 ms == 100,000 hns.
    assert_eq!(aligned_buffer_duration_hns(441, 44_100), 100_000);
    // 3528 frames at 352.8 kHz == exactly 10 ms == 100,000 hns.
    assert_eq!(aligned_buffer_duration_hns(3528, 352_800), 100_000);
    // Non-integer hns rounds to nearest integer (+0.5).
    // 448 / 44100 * 1e7 = 101587.3015... -> 101587.
    assert_eq!(aligned_buffer_duration_hns(448, 44_100), 101_587);
}

#[test]
fn classify_wasapi_hr_distinguishes_busy_unsupported_and_invalidated() {
    let busy = classify_wasapi_hr(
        0x8889_000A_u32 as i32,
        "Initialize",
        "Gungnir 2",
        Some(384_000),
        Some(DacWireFormat::Int32),
    );
    assert!(busy.is_device_busy(), "expected DeviceBusy, got {busy:?}");

    let disallowed = classify_wasapi_hr(
        0x8889_000E_u32 as i32,
        "Initialize",
        "Gungnir 2",
        Some(384_000),
        Some(DacWireFormat::Int32),
    );
    assert!(disallowed.is_device_busy());

    let unsup = classify_wasapi_hr(
        0x8889_0008_u32 as i32,
        "IsFormatSupported",
        "Gungnir 2",
        Some(384_000),
        Some(DacWireFormat::Int32),
    );
    assert!(matches!(unsup, OutputError::UnsupportedFormat { .. }));

    let unplug = classify_wasapi_hr(
        0x8889_0004_u32 as i32,
        "GetBuffer",
        "Gungnir 2",
        None,
        None,
    );
    assert!(matches!(unplug, OutputError::DeviceNotFound(_)));
}

#[test]
fn select_wasapi_device_prefers_usb_dac_and_excludes_own_virtual_endpoint() {
    let devices = vec![
        WasapiRenderDevice {
            friendly_name: "Line (Audiophile Player Exclusive)".to_string(),
            endpoint_id: "{0.0.0.00000000}.{apexclusiveaudio-1}".to_string(),
        },
        WasapiRenderDevice {
            friendly_name: "NVIDIA High Definition Audio (HDMI)".to_string(),
            endpoint_id: "{0.0.0.00000000}.{hdmi-1}".to_string(),
        },
        WasapiRenderDevice {
            friendly_name: "Speakers (Schiit Gungnir 2 Unison 384)".to_string(),
            endpoint_id: "{0.0.0.00000000}.{usb-dac-1}".to_string(),
        },
    ];

    let auto = select_wasapi_device(&devices, None).unwrap();
    assert!(auto.friendly_name.contains("Gungnir 2"));

    let filtered = select_wasapi_device(&devices, Some("nvidia")).unwrap();
    assert!(filtered.friendly_name.contains("NVIDIA"));

    // Own virtual capture endpoint must never be selectable even when matched by substring.
    let err = select_wasapi_device(&devices, Some("Audiophile Player")).unwrap_err();
    assert!(matches!(err, OutputError::DeviceNotFound(_)));
}

#[test]
fn wasapi_loop_controller_drains_cleanly_at_eof_and_fades_on_stop() {
    // Review #10 OD3: both a partial final audio period and a stop fade-out period written at
    // event k only begin clocking out at event k+1, so 2 full trailing silent periods MUST follow
    // the last audible period before returning `Finished`.
    // 1. Complete EOF playback: 10 frames across period_frames=4 -> prefill(4), ev1(4), ev2(2+pad),
    //    ev3(silent #1 -> Continue), ev4(silent #2 -> Finished).
    let frames: Vec<[i32; 2]> = (1..=10).map(|i| [i * 1000, -i * 1000]).collect();
    let mut pump = WaveRtFramePump::new(&frames, DacWireFormat::Int32);
    let mut ctrl = WasapiLoopController::new(4, 8).unwrap();
    let mut buf = vec![0u8; ctrl.period_bytes()];

    assert!(!ctrl.prefill(&mut buf, &mut pump).unwrap());
    assert_eq!(ctrl.frames_played(), 4);

    let (s1, o1) = ctrl.on_event(&mut buf, &mut pump, false).unwrap();
    assert!(!s1 && o1 == WasapiStepOutcome::Continue);
    assert_eq!(ctrl.frames_played(), 8);

    let (s2, o2) = ctrl.on_event(&mut buf, &mut pump, false).unwrap();
    assert!(!s2 && o2 == WasapiStepOutcome::Continue);
    assert_eq!(ctrl.frames_played(), 10);

    let (s3, o3) = ctrl.on_event(&mut buf, &mut pump, false).unwrap();
    assert!(s3 && o3 == WasapiStepOutcome::Continue, "OD3: partial final period needs 2 silent periods");

    let (s4, o4) = ctrl.on_event(&mut buf, &mut pump, false).unwrap();
    assert!(s4 && o4 == WasapiStepOutcome::Finished);
    assert_eq!(ctrl.underrun_periods(), 0);

    // 2. Mid-track stop fade-out: queues 1 faded period then 2 full silent periods -> Finished.
    let long_frames = vec![[1_000_000_000_i32, -1_000_000_000_i32]; 64];
    let mut pump2 = WaveRtFramePump::new(&long_frames, DacWireFormat::Int32);
    let mut ctrl2 = WasapiLoopController::new(8, 8).unwrap();
    let mut buf2 = vec![0u8; ctrl2.period_bytes()];

    ctrl2.prefill(&mut buf2, &mut pump2).unwrap();
    let (fade_silent, fade_out) = ctrl2.on_event(&mut buf2, &mut pump2, true).unwrap();
    assert!(!fade_silent && fade_out == WasapiStepOutcome::Continue);
    // Verify the fade-out monotonically decreases in amplitude toward 0.
    let first_l = i32::from_le_bytes(buf2[0..4].try_into().unwrap()).abs();
    let last_l = i32::from_le_bytes(buf2[56..60].try_into().unwrap()).abs();
    assert!(first_l > last_l, "expected fade-out ramp: first={first_l}, last={last_l}");

    let (sil1, out1) = ctrl2.on_event(&mut buf2, &mut pump2, true).unwrap();
    assert!(sil1 && out1 == WasapiStepOutcome::Continue, "OD3: fade period needs 2 silent periods");

    let (end_silent, end_out) = ctrl2.on_event(&mut buf2, &mut pump2, true).unwrap();
    assert!(end_silent && end_out == WasapiStepOutcome::Finished);
}

#[test]
fn od4_wasapi_event_to_push_fallback_classifier_only_triggers_on_event_errors() {
    assert!(should_fallback_wasapi_event_to_push(&OutputError::Win32 {
        operation: "WaitForSingleObject(WASAPI)",
        code: 258,
        detail: "timeout".to_string(),
    }));
    assert!(should_fallback_wasapi_event_to_push(&OutputError::Win32 {
        operation: "IAudioClient::SetEventHandle",
        code: 0x8007_0057,
        detail: "failed".to_string(),
    }));
    assert!(!should_fallback_wasapi_event_to_push(&OutputError::DeviceBusy {
        device: "DAC".to_string(),
        os_error: 0x8889_000A,
        detail: "in use".to_string(),
    }));
    assert!(!should_fallback_wasapi_event_to_push(&OutputError::UnsupportedFormat {
        sample_rate_hz: 384_000,
        wire_format: DacWireFormat::Int32,
        os_error: 0x8889_0008,
        detail: "unsupported".to_string(),
    }));
    assert!(!should_fallback_wasapi_event_to_push(&OutputError::DeviceNotFound(
        "unplugged".to_string()
    )));
}

#[test]
fn wasapi_loop_controller_recovers_from_streaming_underrun() {
    let mut calls = 0usize;
    let mut pump = StreamingWaveRtPump::new(48_000, DacWireFormat::Int32, move || {
        calls += 1;
        match calls {
            1 => StreamBlock::Underrun,
            2 => StreamBlock::Ready(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 48_000,
                frames: vec![[42, -42]; 4],
            }),
            _ => StreamBlock::EndOfTrack,
        }
    });
    let mut ctrl = WasapiLoopController::new(4, 8).unwrap();
    let mut buf = vec![0u8; ctrl.period_bytes()];

    // Prefill hits underrun -> silent buffer, underrun counted, NOT finished.
    assert!(ctrl.prefill(&mut buf, &mut pump).unwrap());
    assert_eq!(ctrl.underrun_periods(), 1);

    // Next event recovers with real audio.
    let (s1, o1) = ctrl.on_event(&mut buf, &mut pump, false).unwrap();
    assert!(!s1 && o1 == WasapiStepOutcome::Continue);
    assert_eq!(ctrl.frames_played(), 4);
}

#[test]
fn format_negotiation_retries_zero_channel_mask_and_never_swallows_busy() {
    // Quirk 1: driver rejects FL|FR (0x3) with UnsupportedFormat, accepts mask 0.
    let mut seen = Vec::new();
    let mask = negotiate_channel_mask_with_probe(|m| {
        seen.push(m);
        if m == 0x3 {
            Err(OutputError::UnsupportedFormat {
                sample_rate_hz: 384_000,
                wire_format: DacWireFormat::Int32,
                os_error: 0x8889_0008,
                detail: "rejects FL|FR mask".to_string(),
            })
        } else {
            Ok(())
        }
    })
    .unwrap();
    assert_eq!(mask, 0);
    assert_eq!(seen, vec![0x3, 0]);

    // Quirk 2: DeviceBusy stops immediately without retrying mask 0.
    let mut seen2 = Vec::new();
    let err = negotiate_channel_mask_with_probe(|m| {
        seen2.push(m);
        Err(OutputError::DeviceBusy {
            device: "DAC".to_string(),
            os_error: 0x8889_000A,
            detail: "in use".to_string(),
        })
    })
    .unwrap_err();
    assert!(err.is_device_busy());
    assert_eq!(seen2, vec![0x3]);
}

#[cfg(windows)]
#[test]
fn build_wasapi_wave_format_builds_exact_integer_pcm_containers() {
    use ap_output_win::wasapi_exclusive::build_wasapi_wave_format;

    let f32 = build_wasapi_wave_format(384_000, 2, DacWireFormat::Int32);
    let fmt32 = f32.Format;
    assert_eq!({ fmt32.nSamplesPerSec }, 384_000);
    assert_eq!({ fmt32.wBitsPerSample }, 32);
    assert_eq!({ fmt32.nBlockAlign }, 8);
    assert_eq!(unsafe { f32.Samples.wValidBitsPerSample }, 32);

    let f24in32 = build_wasapi_wave_format(352_800, 2, DacWireFormat::Int24In32);
    let fmt24in32 = f24in32.Format;
    assert_eq!({ fmt24in32.wBitsPerSample }, 32);
    assert_eq!({ fmt24in32.nBlockAlign }, 8);
    assert_eq!(unsafe { f24in32.Samples.wValidBitsPerSample }, 24);

    let f24p = build_wasapi_wave_format(192_000, 2, DacWireFormat::Int24Packed);
    let fmt24p = f24p.Format;
    assert_eq!({ fmt24p.wBitsPerSample }, 24);
    assert_eq!({ fmt24p.nBlockAlign }, 6);
    assert_eq!(unsafe { f24p.Samples.wValidBitsPerSample }, 24);

    let f16 = build_wasapi_wave_format(48_000, 2, DacWireFormat::Int16);
    let fmt16 = f16.Format;
    assert_eq!({ fmt16.wBitsPerSample }, 16);
    assert_eq!({ fmt16.nBlockAlign }, 4);
    assert_eq!(unsafe { f16.Samples.wValidBitsPerSample }, 16);
}

#[cfg(windows)]
#[test]
#[ignore = "hardware"]
fn hardware_live_enumerate_and_probe_wasapi_exclusive_devices() {
    use ap_output_win::wasapi_exclusive::{
        enumerate_wasapi_render_devices, probe_wasapi_device_format,
    };

    let devices = enumerate_wasapi_render_devices().expect("WASAPI enumeration failed");
    println!("=== Native Rust WASAPI Render Endpoints ({}) ===", devices.len());
    for d in &devices {
        println!("  - {} [{}]", d.friendly_name, d.endpoint_id);
        for rate in [44_100, 48_000, 192_000, 352_800, 384_000] {
            let res = probe_wasapi_device_format(d, rate, DacWireFormat::Int32);
            let label = match res {
                Ok(()) => "OK".to_string(),
                Err(OutputError::DeviceBusy { .. }) => "BUSY".to_string(),
                Err(OutputError::UnsupportedFormat { .. }) => "unsupported".to_string(),
                Err(e) => format!("err({e})"),
            };
            println!("      Int32 @ {rate:>6} Hz: {label}");
        }
    }
    assert!(
        !devices.is_empty(),
        "expected at least one active Windows WASAPI render endpoint"
    );
}
