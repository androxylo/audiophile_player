use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use ap_source::flac::AudioBlockSource;
use ap_source::http_stream::{
    HttpAudioSource, HttpStreamHints, ParsedHttpUrl, StreamContainerFormat, parse_lpcm_params,
};
use flac_codec::encode::{FlacSampleWriter, Options};

fn encode_test_flac_bytes(rate_hz: u32, bits: u32, samples: &[i32]) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    let mut writer = FlacSampleWriter::new(
        &mut out,
        Options::default(),
        rate_hz,
        bits,
        2,
        Some(samples.len() as u64),
    )
    .expect("FlacSampleWriter");
    writer.write(samples).expect("write flac samples");
    writer.finalize().expect("finalize flac");
    out.into_inner()
}

/// Wrap a native FLAC byte buffer into a minimal RFC 3533 Ogg FLAC v1.0 logical bitstream.
fn wrap_native_flac_in_ogg(native_flac: &[u8], serial: u32) -> Vec<u8> {
    assert!(&native_flac[0..4] == b"fLaC");
    // Locate the end of STREAMINFO (4-byte header + 34-byte body = 38 bytes after "fLaC").
    let streaminfo_end = 4 + 4 + 34;
    let mut packets: Vec<Vec<u8>> = Vec::new();

    // Packet 0: Ogg FLAC BOS header + "fLaC" + STREAMINFO.
    let mut bos = Vec::new();
    bos.push(0x7F);
    bos.extend_from_slice(b"FLAC");
    bos.push(1); // major
    bos.push(0); // minor
    // Count additional metadata packets after STREAMINFO.
    let mut pos = streaminfo_end;
    let mut extra_meta = Vec::new();
    let mut is_last = (native_flac[4] & 0x80) != 0;
    while !is_last {
        let hdr = &native_flac[pos..pos + 4];
        is_last = (hdr[0] & 0x80) != 0;
        let len = ((hdr[1] as usize) << 16) | ((hdr[2] as usize) << 8) | (hdr[3] as usize);
        extra_meta.push(native_flac[pos..pos + 4 + len].to_vec());
        pos += 4 + len;
    }
    bos.extend_from_slice(&(extra_meta.len() as u16).to_be_bytes());
    bos.extend_from_slice(&native_flac[..streaminfo_end]);
    packets.push(bos);
    packets.extend(extra_meta);
    // Remaining bytes are raw FLAC audio frames -> put into one audio packet for test.
    packets.push(native_flac[pos..].to_vec());

    let mut out = Vec::new();
    for (idx, pkt) in packets.iter().enumerate() {
        let mut htype = 0u8;
        if idx == 0 {
            htype |= 0x02; // BOS
        }
        if idx + 1 == packets.len() {
            htype |= 0x04; // EOS
        }
        write_ogg_page(&mut out, htype, serial, idx as u32, pkt);
    }
    out
}

fn write_ogg_page(out: &mut Vec<u8>, header_type: u8, serial: u32, seq: u32, pkt: &[u8]) {
    let mut segs = Vec::new();
    let mut rem = pkt.len();
    while rem >= 255 {
        segs.push(255u8);
        rem -= 255;
    }
    segs.push(rem as u8);
    assert!(segs.len() <= 255);

    out.extend_from_slice(b"OggS");
    out.push(0); // version
    out.push(header_type);
    out.extend_from_slice(&0u64.to_le_bytes()); // granule
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // crc (not verified by our demuxer)
    out.push(segs.len() as u8);
    out.extend_from_slice(&segs);
    out.extend_from_slice(pkt);
}

fn build_wav_bytes(
    rate_hz: u32,
    bits: u16,
    samples_le_bytes: &[u8],
    streaming_sentinel: bool,
) -> Vec<u8> {
    let channels: u16 = 2;
    let block_align = channels * (bits / 8);
    let byte_rate = rate_hz * u32::from(block_align);
    let data_len = if streaming_sentinel {
        0xFFFF_FFFFu32
    } else {
        samples_le_bytes.len() as u32
    };
    let riff_len = if streaming_sentinel {
        0xFFFF_FFFFu32
    } else {
        36 + data_len
    };
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate_hz.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(samples_le_bytes);
    out
}

fn spawn_single_http_server(
    content_type: &'static str,
    body: Vec<u8>,
    chunked: bool,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/stream");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut req_buf = [0u8; 1024];
        let _ = stream.read(&mut req_buf);
        if chunked {
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(hdr.as_bytes()).unwrap();
            for chunk in body.chunks(317) {
                let sz = format!("{:x}\r\n", chunk.len());
                stream.write_all(sz.as_bytes()).unwrap();
                stream.write_all(chunk).unwrap();
                stream.write_all(b"\r\n").unwrap();
            }
            stream.write_all(b"0\r\n\r\n").unwrap();
        } else {
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(hdr.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        }
        let _ = stream.flush();
    });
    (url, handle)
}

#[test]
fn parses_http_urls_and_rejects_unsafe_or_unsupported_schemes() {
    let u = ParsedHttpUrl::parse("http://192.168.0.10:56923/stream.wav?id=1").unwrap();
    assert_eq!(u.host, "192.168.0.10");
    assert_eq!(u.port, 56923);
    assert_eq!(u.path_and_query, "/stream.wav?id=1");
    assert_eq!(u.host_header, "192.168.0.10:56923");

    assert!(ParsedHttpUrl::parse("file:///C:/Windows/win.ini").is_err());
    assert!(ParsedHttpUrl::parse("http://127.0.0.1:8080/a\r\nX-Inject: 1").is_err());
    assert!(ParsedHttpUrl::parse("https://example.com/tidal.flac").is_err());
}

#[test]
fn streams_chunked_native_flac_bit_accurately() {
    let mut samples = Vec::new();
    for i in 0..2048i32 {
        samples.push((i * 100) % 8_000_000);
        samples.push(-((i * 75) % 8_000_000));
    }
    let flac_bytes = encode_test_flac_bytes(96_000, 24, &samples);
    let (url, srv) = spawn_single_http_server("audio/flac", flac_bytes, true);

    let cancel = Arc::new(AtomicBool::new(false));
    let mut src = HttpAudioSource::open(&url, &HttpStreamHints::default(), cancel).unwrap();
    assert_eq!(src.container_format(), StreamContainerFormat::Flac);
    assert!(src.is_streaming());
    assert_eq!(src.spec().sample_rate_hz, 96_000);
    assert_eq!(src.spec().bits_per_sample, 24);

    let mut total_l = Vec::new();
    let mut total_r = Vec::new();
    while let Some(blk) = src.read_next_block(512).unwrap() {
        total_l.extend_from_slice(&blk.left);
        total_r.extend_from_slice(&blk.right);
    }
    assert_eq!(total_l.len(), 2048);
    let inv = 1.0 / ((1_u64 << 31) as f64);
    for i in 0..2048 {
        let exp_l = ((samples[i * 2] << 8) as f64) * inv;
        let exp_r = ((samples[i * 2 + 1] << 8) as f64) * inv;
        assert!((total_l[i] - exp_l).abs() < 1e-15);
        assert!((total_r[i] - exp_r).abs() < 1e-15);
    }
    srv.join().unwrap();
}

#[test]
fn streams_chained_ogg_flac_same_rate_and_rate_change_segments() {
    let mut s1 = Vec::new();
    for i in 0..1024i32 {
        s1.push(i * 16);
        s1.push(-i * 16);
    }
    let mut s2 = Vec::new();
    for i in 0..512i32 {
        s2.push(1000 + i * 8);
        s2.push(-1000 - i * 8);
    }
    let mut s3 = Vec::new();
    for i in 0..768i32 {
        s3.push(i * 32);
        s3.push(i * 16);
    }

    // Chain: 44.1k (seg 1) + 44.1k (seg 2, merged seamlessly) + 96k (seg 3, rate change!).
    let mut body = wrap_native_flac_in_ogg(&encode_test_flac_bytes(44_100, 16, &s1), 101);
    body.extend_from_slice(&wrap_native_flac_in_ogg(
        &encode_test_flac_bytes(44_100, 16, &s2),
        102,
    ));
    body.extend_from_slice(&wrap_native_flac_in_ogg(
        &encode_test_flac_in_ogg_96k_helper(&s3),
        103,
    ));

    let (url, srv) = spawn_single_http_server("application/ogg", body, true);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut src = HttpAudioSource::open(&url, &HttpStreamHints::default(), cancel).unwrap();
    assert_eq!(src.container_format(), StreamContainerFormat::OggFlac);
    assert_eq!(src.spec().sample_rate_hz, 44_100);

    let mut seg12_frames = 0usize;
    while let Some(blk) = src.read_next_block(400).unwrap() {
        seg12_frames += blk.len();
    }
    assert_eq!(seg12_frames, 1024 + 512);

    let mut next_seg = src
        .take_next_chained_segment()
        .expect("96 kHz chained segment must be staged");
    assert_eq!(next_seg.spec().sample_rate_hz, 96_000);
    let mut seg3_frames = 0usize;
    while let Some(blk) = next_seg.read_next_block(400).unwrap() {
        seg3_frames += blk.len();
    }
    assert_eq!(seg3_frames, 768);
    assert!(next_seg.take_next_chained_segment().is_none());
    srv.join().unwrap();
}

fn encode_test_flac_in_ogg_96k_helper(samples: &[i32]) -> Vec<u8> {
    encode_test_flac_bytes(96_000, 24, samples)
}

#[test]
fn streams_chunked_wav_with_streaming_sentinel_header_24bit() {
    let mut pcm = Vec::new();
    for i in 0..1500i32 {
        let l = (i * 256) - 100_000;
        let r = -(i * 128) + 50_000;
        let lb = l.to_le_bytes();
        let rb = r.to_le_bytes();
        pcm.extend_from_slice(&lb[0..3]);
        pcm.extend_from_slice(&rb[0..3]);
    }
    let wav = build_wav_bytes(48_000, 24, &pcm, true);
    let (url, srv) = spawn_single_http_server("audio/wav", wav, true);

    let cancel = Arc::new(AtomicBool::new(false));
    let mut src = HttpAudioSource::open(&url, &HttpStreamHints::default(), cancel).unwrap();
    assert_eq!(src.container_format(), StreamContainerFormat::Wav);
    assert_eq!(src.spec().sample_rate_hz, 48_000);
    assert_eq!(src.spec().bits_per_sample, 24);
    assert_eq!(src.total_frames(), None); // 0xFFFFFFFF streaming sentinel!

    let mut total = 0usize;
    while let Some(blk) = src.read_next_block(500).unwrap() {
        total += blk.len();
    }
    assert_eq!(total, 1500);
    srv.join().unwrap();
}

#[test]
fn streams_raw_lpcm_l24_big_endian_from_content_type_params() {
    assert_eq!(
        parse_lpcm_params(
            Some("audio/L24;rate=88200;channels=2"),
            &HttpStreamHints::default()
        ),
        Some((24, 88_200, 2))
    );

    let mut raw = Vec::new();
    for i in 0..600i32 {
        let l: i32 = i * 500;
        let r: i32 = -i * 500;
        let lb = l.to_be_bytes();
        let rb = r.to_be_bytes();
        // Low 24 bits in big-endian are bytes [1..4] of i32.
        raw.extend_from_slice(&lb[1..4]);
        raw.extend_from_slice(&rb[1..4]);
    }
    let (url, srv) = spawn_single_http_server("audio/L24;rate=88200;channels=2", raw, false);

    let cancel = Arc::new(AtomicBool::new(false));
    let mut src = HttpAudioSource::open(&url, &HttpStreamHints::default(), cancel).unwrap();
    assert_eq!(src.container_format(), StreamContainerFormat::Lpcm24Be);
    assert_eq!(src.spec().sample_rate_hz, 88_200);
    assert_eq!(src.spec().bits_per_sample, 24);

    let mut frames = 0usize;
    while let Some(blk) = src.read_next_block(256).unwrap() {
        frames += blk.len();
    }
    assert_eq!(frames, 600);
    srv.join().unwrap();
}

#[test]
fn cooperative_cancel_aborts_stalled_http_stream_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/slow");
    let srv_done = Arc::new(AtomicBool::new(false));
    let srv_done_bg = Arc::clone(&srv_done);

    let srv = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut req = [0u8; 512];
        let _ = s.read(&mut req);
        // Send headers, then stall indefinitely without sending body bytes.
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\n\r\n");
        let _ = s.flush();
        while !srv_done_bg.load(Ordering::Acquire) {
            thread::sleep(std::time::Duration::from_millis(20));
        }
    });

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_bg = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(120));
        cancel_bg.store(true, Ordering::Release);
    });

    let started = std::time::Instant::now();
    let res = HttpAudioSource::open(&url, &HttpStreamHints::default(), cancel);
    assert!(res.is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "cancelled HTTP open must abort promptly"
    );
    srv_done.store(true, Ordering::Release);
    srv.join().unwrap();
}

#[test]
fn never_misinterprets_non_lpcm_stream_as_raw_pcm_when_didl_has_bits_per_sample() {
    // Review #5 Finding #1 [HIGH]: an HTML error page or unrecognized codec with
    // `protocolInfo="http-get:*:audio/flac:*"` and `bits_per_sample=24` MUST be rejected,
    // never played as full-scale raw LPCM noise!
    let hints = HttpStreamHints {
        protocol_info: Some("http-get:*:audio/flac:*".to_string()),
        sample_rate_hz: Some(96_000),
        channels: Some(2),
        bits_per_sample: Some(24),
    };
    assert_eq!(parse_lpcm_params(Some("text/html"), &hints), None);
    assert_eq!(parse_lpcm_params(Some("audio/flac"), &hints), None);

    let (url, srv) = spawn_single_http_server(
        "text/html",
        b"<html><body>404 Not Found</body></html>".to_vec(),
        false,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let err = match HttpAudioSource::open(&url, &hints, cancel) {
        Ok(_) => panic!("HTML body with FLAC DIDL hints must be rejected, not played as LPCM"),
        Err(e) => e,
    };
    assert!(
        err.contains("unsupported HTTP audio stream format"),
        "unexpected error: {err}"
    );
    srv.join().unwrap();
}

#[test]
fn skips_leading_id3v2_tag_before_native_flac_stream() {
    let mut samples = Vec::new();
    for i in 0..512i32 {
        samples.push(i * 100);
        samples.push(-i * 100);
    }
    let flac = encode_test_flac_bytes(44_100, 16, &samples);
    // Prepend a 26-byte ID3v2.3 tag (10-byte header + 16-byte payload).
    let mut body = Vec::new();
    body.extend_from_slice(b"ID3");
    body.extend_from_slice(&[3, 0, 0]); // v2.3, flags=0
    body.extend_from_slice(&[0, 0, 0, 16]); // synchsafe size = 16
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&flac);

    let (url, srv) = spawn_single_http_server("audio/flac", body, false);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut src = HttpAudioSource::open(&url, &HttpStreamHints::default(), cancel).unwrap();
    assert_eq!(src.container_format(), StreamContainerFormat::Flac);
    let mut total = 0usize;
    while let Some(b) = src.read_next_block(256).unwrap() {
        total += b.len();
    }
    assert_eq!(total, 512);
    srv.join().unwrap();
}

#[test]
fn detects_content_length_truncation_and_resumes_when_accept_ranges_supported() {
    // Review #5 Finding #9 [MEDIUM]:
    // Case A: Server declares Content-Length > bytes sent and no Accept-Ranges -> MUST return Err!
    let mut pcm = Vec::new();
    for i in 0..1024i16 {
        pcm.extend_from_slice(&i.to_le_bytes());
        pcm.extend_from_slice(&(-i).to_le_bytes());
    }
    let full_wav = build_wav_bytes(44_100, 16, &pcm, false);
    let half = full_wav.len() / 2;

    let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr_a = listener_a.local_addr().unwrap();
    let wav_a = full_wav.clone();
    let srv_a = thread::spawn(move || {
        let (mut s, _) = listener_a.accept().unwrap();
        let mut req = [0u8; 512];
        let _ = s.read(&mut req);
        let hdr = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            wav_a.len()
        );
        s.write_all(hdr.as_bytes()).unwrap();
        s.write_all(&wav_a[..half]).unwrap();
    });

    let mut src_a = HttpAudioSource::open(
        &format!("http://{addr_a}/trunc.wav"),
        &HttpStreamHints::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut saw_err = false;
    loop {
        match src_a.read_next_block(256) {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) => {
                assert!(e.contains("truncated") || e.contains("UnexpectedEof") || e.contains("EOF"), "err={e}");
                saw_err = true;
                break;
            }
        }
    }
    assert!(saw_err, "truncated Content-Length stream must report an error instead of silent EOF");
    srv_a.join().unwrap();

    // Case B: Server advertises Accept-Ranges: bytes -> single Range reconnect finishes all 1024 frames!
    let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr_b = listener_b.local_addr().unwrap();
    let wav_b = full_wav;
    let srv_b = thread::spawn(move || {
        // First connection drops halfway.
        {
            let (mut s, _) = listener_b.accept().unwrap();
            let mut req = [0u8; 512];
            let _ = s.read(&mut req);
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wav_b.len()
            );
            s.write_all(hdr.as_bytes()).unwrap();
            s.write_all(&wav_b[..half]).unwrap();
        }
        // Second connection receives `Range: bytes=<half>-` and serves the rest with 206.
        {
            let (mut s, _) = listener_b.accept().unwrap();
            let mut req = [0u8; 512];
            let n = s.read(&mut req).unwrap();
            let req_str = String::from_utf8_lossy(&req[..n]);
            assert!(
                req_str.contains(&format!("Range: bytes={half}-")),
                "expected Range resume header, got: {req_str}"
            );
            let rest = &wav_b[half..];
            let hdr = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                rest.len()
            );
            s.write_all(hdr.as_bytes()).unwrap();
            s.write_all(rest).unwrap();
        }
    });

    let mut src_b = HttpAudioSource::open(
        &format!("http://{addr_b}/resume.wav"),
        &HttpStreamHints::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut frames = 0usize;
    while let Some(b) = src_b.read_next_block(256).unwrap() {
        frames += b.len();
    }
    assert_eq!(frames, 1024);
    srv_b.join().unwrap();
}

#[test]
fn open_at_offset_seeks_accurately_into_flac_stream() {
    let mut samples = Vec::new();
    for i in 0..4410i32 {
        samples.push(i);
        samples.push(-i);
    }
    let flac = encode_test_flac_bytes(44_100, 16, &samples);
    let (url, srv) = spawn_single_http_server("audio/flac", flac, false);

    // Seek to 0.05 s = 2205 frames into 4410-frame (0.10 s) track -> exactly 2205 frames remain.
    let mut src = HttpAudioSource::open_at_offset(
        &url,
        &HttpStreamHints::default(),
        Some(0.05),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut remaining = 0usize;
    while let Some(b) = src.read_next_block(512).unwrap() {
        remaining += b.len();
    }
    assert_eq!(remaining, 2205);
    srv.join().unwrap();
}
