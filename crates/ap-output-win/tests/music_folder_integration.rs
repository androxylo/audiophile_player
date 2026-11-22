//! Full end-to-end integration test: pick a random FLAC from `~/Music` using the
//! scalable O(depth) subfolder-walk picker (resilient to 20,000+ track libraries;
//! zero copyrighted music is ever stored in the repository) and play it for 10 seconds
//! through native Rust Windows Kernel Streaming (`WDM-KS` WaveRT) on the Schiit Gungnir 2,
//! then stop gracefully with a click-free fade-out.

#[cfg(windows)]
mod random_music_folder_test {
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    use ap_core::format::DacWireFormat;
    use ap_output_win::sink::ZeroMixerAudioSink;
    use ap_output_win::wdm_ks::WdmKsSink;
    use ap_source::flac::decode_flac_full_memory_int32;
    use ap_source::library::{default_user_music_dir, pick_random_flac_walk};

    #[test]
    fn plays_random_flac_from_user_music_for_10_seconds_via_wdm_ks() {
        let Some(music_dir) = default_user_music_dir() else {
            println!("Skipping: could not resolve ~/Music directory.");
            return;
        };
        let pick_start = Instant::now();
        let chosen = match pick_random_flac_walk(&music_dir) {
            Ok(p) => p,
            Err(err) => {
                println!("Skipping: {err}");
                return;
            }
        };
        let pick_ms = pick_start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "=== Random ~/Music Integration Test (O(depth) walk picked in {pick_ms:.2} ms) ==="
        );
        println!("  Selected track: {}", chosen.display());

        let decoded = decode_flac_full_memory_int32(&chosen)
            .expect("failed to decode random FLAC from ~/Music into RAM");
        let total_track_s =
            decoded.pcm.frames.len() as f64 / decoded.spec.sample_rate_hz as f64;
        println!(
            "  Decoded into RAM: {}-bit / {} Hz, {} ch, {:.1} s ({} frames)",
            decoded.spec.bits_per_sample,
            decoded.spec.sample_rate_hz,
            decoded.spec.channels,
            total_track_s,
            decoded.pcm.frames.len()
        );

        let mut sink =
            WdmKsSink::open("Gungnir 2").expect("failed to locate Gungnir 2 WDM-KS device");
        let stop = sink.stop_handle();

        // Schedule graceful click-free stop after 10 seconds of playback.
        let play_limit = Duration::from_secs(10);
        let timer = thread::spawn(move || {
            thread::sleep(play_limit);
            stop.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let stats = match sink.play_full_memory(&decoded.pcm, DacWireFormat::Int32) {
            Ok(s) => s,
            Err(err) if err.is_device_busy() => {
                panic!("DAC LOCKED BY ANOTHER PLAYER (e.g. Audirvana): {err}");
            }
            Err(err) => panic!("WDM-KS playback of random ~/Music FLAC failed: {err}"),
        };
        let elapsed = started.elapsed();
        let _ = timer.join();

        println!(
            "  Stopped gracefully after {:.2} s: {stats:?}",
            elapsed.as_secs_f64()
        );

        assert_eq!(stats.sample_rate_hz, decoded.spec.sample_rate_hz);
        assert!(stats.mmcss_pro_audio);
        assert!(stats.frames_played > 0);
        if total_track_s >= 10.5 {
            assert!(
                elapsed >= Duration::from_secs(9) && elapsed <= Duration::from_secs(12),
                "expected ~10 s playback window, got {elapsed:?}"
            );
        }
    }
}
