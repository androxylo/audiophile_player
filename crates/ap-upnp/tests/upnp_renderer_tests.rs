// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ap_control::state::{ControlCommand, SharedControl};
use ap_upnp::services::SINK_PROTOCOL_INFO;
use ap_upnp::ssdp::match_msearch_request;
use ap_upnp::xml::{
    deterministic_udn, extract_xml_element_text, format_upnp_duration, parse_didl_lite,
    parse_upnp_duration,
};
use ap_upnp::{TransportState, UpnpRendererConfig, UpnpRendererServer};

fn send_raw_http(port: u16, raw_req: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect UPnP server");
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(raw_req.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    resp
}

fn soap_post(port: u16, path: &str, service_urn: &str, action: &str, args_xml: &str) -> String {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service_urn}\">{args_xml}</u:{action}></s:Body>\
         </s:Envelope>"
    );
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPACTION: \"{service_urn}#{action}\"\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    send_raw_http(port, &req)
}

#[test]
fn didl_lite_and_duration_and_ssdp_matching_work_cleanly() {
    let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="1"><dc:title>Nocturne &amp; Op. 9</dc:title><upnp:artist>Chopin</upnp:artist><upnp:album>Nocturnes</upnp:album><res protocolInfo="http-get:*:audio/flac:*" duration="0:04:32.500" sampleFrequency="96000" bitsPerSample="24" nrAudioChannels="2">http://192.168.0.10:56923/track.flac</res></item></DIDL-Lite>"#;
    let parsed = parse_didl_lite(didl);
    assert_eq!(parsed.title.as_deref(), Some("Nocturne & Op. 9"));
    assert_eq!(parsed.artist.as_deref(), Some("Chopin"));
    assert_eq!(parsed.album.as_deref(), Some("Nocturnes"));
    assert_eq!(parsed.protocol_info.as_deref(), Some("http-get:*:audio/flac:*"));
    assert_eq!(parsed.sample_rate_hz, Some(96_000));
    assert_eq!(parsed.bits_per_sample, Some(24));
    assert_eq!(parsed.channels, Some(2));
    assert!((parsed.duration_s.unwrap() - 272.5).abs() < 1e-6);
    assert_eq!(format_upnp_duration(272.5), "00:04:32");
    assert_eq!(parse_upnp_duration("01:02:03"), Some(3723.0));

    let udn1 = deterministic_udn("Audiophile Player");
    let udn2 = deterministic_udn("Audiophile Player");
    assert_eq!(udn1, udn2);
    assert!(udn1.starts_with("uuid:"));

    let msearch_all = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: ssdp:all\r\n\r\n";
    assert_eq!(match_msearch_request(msearch_all, &udn1).len(), 6);

    let msearch_mr = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    assert_eq!(
        match_msearch_request(msearch_mr, &udn1),
        vec!["urn:schemas-upnp-org:device:MediaRenderer:1"]
    );
}

#[test]
fn serves_description_scpds_and_simulated_foobar2000_soap_session() {
    let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Gungnir 2 Unison 384");
    let mut srv = UpnpRendererServer::start(
        shared.clone(),
        UpnpRendererConfig {
            friendly_name: "Audiophile Player Test".to_string(),
            port: None,
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            enable_ssdp: false,
        },
    )
    .expect("start UPnP server");
    let port = srv.port();

    // 1. Device description + 3 SCPDs.
    let desc = send_raw_http(
        port,
        &format!("GET /description.xml HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert!(desc.starts_with("HTTP/1.1 200 OK"));
    assert!(desc.contains("<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>"));
    assert!(desc.contains("<friendlyName>Audiophile Player Test</friendlyName>"));
    assert!(desc.contains("/scpd/AVTransport.xml"));

    for scpd_path in [
        "/scpd/AVTransport.xml",
        "/scpd/ConnectionManager.xml",
        "/scpd/RenderingControl.xml",
    ] {
        let r = send_raw_http(
            port,
            &format!("GET {scpd_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
        );
        assert!(r.starts_with("HTTP/1.1 200 OK"), "SCPD failed for {scpd_path}");
        assert!(r.contains("<scpd xmlns=\"urn:schemas-upnp-org:service-1-0\">"));
    }

    // 2. ConnectionManager::GetProtocolInfo advertises lossless formats only (zero MP3/AAC).
    let proto = soap_post(
        port,
        "/upnp/control/ConnectionManager",
        "urn:schemas-upnp-org:service:ConnectionManager:1",
        "GetProtocolInfo",
        "",
    );
    assert!(proto.starts_with("HTTP/1.1 200 OK"));
    let sink_str = extract_xml_element_text(&proto, "Sink").unwrap();
    assert_eq!(sink_str, SINK_PROTOCOL_INFO);
    assert!(sink_str.contains("audio/flac"));
    assert!(sink_str.contains("audio/ogg"));
    assert!(sink_str.contains("audio/wav"));
    assert!(sink_str.contains("audio/L24"));
    assert!(!sink_str.contains("audio/mpeg"));
    assert!(!sink_str.contains("audio/aac"));

    // 3. RenderingControl: fixed 100, unmuted; SetVolume succeeds as bit-perfect no-op.
    let vol = soap_post(
        port,
        "/upnp/control/RenderingControl",
        "urn:schemas-upnp-org:service:RenderingControl:1",
        "GetVolume",
        "<InstanceID>0</InstanceID><Channel>Master</Channel>",
    );
    assert_eq!(extract_xml_element_text(&vol, "CurrentVolume").as_deref(), Some("100"));
    let set_vol = soap_post(
        port,
        "/upnp/control/RenderingControl",
        "urn:schemas-upnp-org:service:RenderingControl:1",
        "SetVolume",
        "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>42</DesiredVolume>",
    );
    assert!(set_vol.starts_with("HTTP/1.1 200 OK"));
    let vol_after = soap_post(
        port,
        "/upnp/control/RenderingControl",
        "urn:schemas-upnp-org:service:RenderingControl:1",
        "GetVolume",
        "<InstanceID>0</InstanceID><Channel>Master</Channel>",
    );
    assert_eq!(extract_xml_element_text(&vol_after, "CurrentVolume").as_deref(), Some("100"));

    // 4. Reject non-http:// URIs cleanly with UPnP SOAP fault 714.
    let bad_uri = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "SetAVTransportURI",
        "<InstanceID>0</InstanceID><CurrentURI>file:///C:/secret.flac</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
    );
    assert!(bad_uri.starts_with("HTTP/1.1 500 Internal Server Error"));
    assert_eq!(extract_xml_element_text(&bad_uri, "errorCode").as_deref(), Some("714"));

    // 5. foobar2000 sequence: SetAVTransportURI -> Play -> SetNextAVTransportURI -> GetPositionInfo -> Stop.
    let set_uri = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "SetAVTransportURI",
        "<InstanceID>0</InstanceID>\
         <CurrentURI>http://127.0.0.1:56923/track1.wav</CurrentURI>\
         <CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;item&gt;&lt;dc:title&gt;Track One&lt;/dc:title&gt;&lt;res duration=&quot;0:03:20&quot;&gt;&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>",
    );
    assert!(set_uri.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(srv.state().snapshot().transport_state, TransportState::Stopped);

    let play = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    );
    assert!(play.starts_with("HTTP/1.1 200 OK"));
    match shared.take_pending_command() {
        Some(ControlCommand::PlayUpnp(req)) => {
            assert_eq!(req.uri, "http://127.0.0.1:56923/track1.wav");
            assert_eq!(req.display_name, "Track One");
            assert_eq!(req.duration_s, Some(200.0));
        }
        other => panic!("expected PlayUpnp command, got {other:?}"),
    }

    // Engine marks Playing -> GetTransportInfo & GetPositionInfo reflect PLAYING & TrackDuration.
    srv.state()
        .engine_mark_playing("http://127.0.0.1:56923/track1.wav", Some(200.0));
    let tinfo = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "GetTransportInfo",
        "<InstanceID>0</InstanceID>",
    );
    assert_eq!(
        extract_xml_element_text(&tinfo, "CurrentTransportState").as_deref(),
        Some("PLAYING")
    );

    let set_next = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "SetNextAVTransportURI",
        "<InstanceID>0</InstanceID><NextURI>http://127.0.0.1:56923/track2.flac</NextURI><NextURIMetaData></NextURIMetaData>",
    );
    assert!(set_next.starts_with("HTTP/1.1 200 OK"));
    let promoted = srv
        .state()
        .engine_take_next_for_gapless()
        .expect("next URI queued for gapless");
    assert_eq!(promoted.uri, "http://127.0.0.1:56923/track2.flac");

    let stop = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Stop",
        "<InstanceID>0</InstanceID>",
    );
    assert!(stop.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(shared.take_pending_command(), Some(ControlCommand::Pause));
    assert_eq!(srv.state().snapshot().transport_state, TransportState::Stopped);

    srv.shutdown();
}

#[test]
fn gena_subscribe_delivers_initial_notify_and_blocks_public_wan_callbacks() {
    let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");
    let mut srv = UpnpRendererServer::start(
        shared,
        UpnpRendererConfig {
            friendly_name: "GENA Test".to_string(),
            port: None,
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            enable_ssdp: false,
        },
    )
    .unwrap();
    let port = srv.port();

    // WAN callback must be rejected (SSRF protection).
    let wan_sub = send_raw_http(
        port,
        &format!(
            "SUBSCRIBE /upnp/event/AVTransport HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             CALLBACK: <http://1.1.1.1:80/evil>\r\n\
             NT: upnp:event\r\n\
             TIMEOUT: Second-300\r\n\r\n"
        ),
    );
    assert!(wan_sub.starts_with("HTTP/1.1 412"));

    // Localhost callback succeeds and receives SEQ: 0 NOTIFY with LastChange.
    let cb_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let cb_port = cb_listener.local_addr().unwrap().port();
    let receiver = thread::spawn(move || {
        cb_listener.set_nonblocking(false).unwrap();
        let (mut conn, _) = cb_listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut collected = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match conn.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    collected.extend_from_slice(&buf[..n]);
                    if collected.windows(13).any(|w| w == b"</e:propertyset"[..13].as_ref()) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        String::from_utf8_lossy(&collected).into_owned()
    });

    let sub_resp = send_raw_http(
        port,
        &format!(
            "SUBSCRIBE /upnp/event/AVTransport HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             CALLBACK: <http://127.0.0.1:{cb_port}/cb>\r\n\
             NT: upnp:event\r\n\
             TIMEOUT: Second-300\r\n\r\n"
        ),
    );
    assert!(sub_resp.starts_with("HTTP/1.1 200 OK"));
    assert!(sub_resp.contains("SID: uuid:ap-gena-"));

    let notify_pkt = receiver.join().unwrap();
    assert!(notify_pkt.starts_with("NOTIFY /cb HTTP/1.1"));
    assert!(notify_pkt.contains("SEQ: 0"));
    assert!(notify_pkt.contains("LastChange"));

    srv.shutdown();
}

#[test]
fn seek_and_pause_resume_preserve_position_and_per_peer_cap_prevents_starvation() {
    let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Gungnir 2");
    let mut srv = UpnpRendererServer::start(
        shared.clone(),
        UpnpRendererConfig {
            friendly_name: "Seek/Resume Test".to_string(),
            port: None,
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            enable_ssdp: false,
        },
    )
    .unwrap();
    let port = srv.port();

    // Load a 3-minute track and start playing.
    let _ = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "SetAVTransportURI",
        "<InstanceID>0</InstanceID>\
         <CurrentURI>http://127.0.0.1:50000/song.flac</CurrentURI>\
         <CurrentURIMetaData>&lt;DIDL-Lite&gt;&lt;item&gt;&lt;res duration=&quot;0:03:00&quot;&gt;&lt;/res&gt;&lt;/item&gt;&lt;/DIDL-Lite&gt;</CurrentURIMetaData>",
    );
    let _ = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    );
    let _ = shared.take_pending_command();
    srv.state()
        .engine_mark_playing("http://127.0.0.1:50000/song.flac", Some(180.0));

    // Review #5 Finding #8: Seek(REL_TIME, 00:01:30) succeeds and dispatches PlayUpnp at 90.0 s!
    let seek_ok = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Seek",
        "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>00:01:30</Target>",
    );
    assert!(seek_ok.starts_with("HTTP/1.1 200 OK"), "seek failed: {seek_ok}");
    match shared.take_pending_command() {
        Some(ControlCommand::PlayUpnp(req)) => {
            assert_eq!(req.seek_offset_s, Some(90.0));
        }
        other => panic!("expected PlayUpnp(seek_offset_s=90.0), got {other:?}"),
    }
    srv.state()
        .engine_mark_playing("http://127.0.0.1:50000/song.flac", Some(180.0));

    // Review #5 Finding #7: Pause then Play resumes from >= 90 s (never resets to 0)!
    let _ = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Pause",
        "<InstanceID>0</InstanceID>",
    );
    assert_eq!(shared.take_pending_command(), Some(ControlCommand::Pause));
    assert_eq!(srv.state().snapshot().transport_state, TransportState::PausedPlayback);

    let _ = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Play",
        "<InstanceID>0</InstanceID><Speed>1</Speed>",
    );
    match shared.take_pending_command() {
        Some(ControlCommand::PlayUpnp(req)) => {
            assert!(
                req.seek_offset_s.is_some_and(|s| s >= 90.0),
                "resume from pause must keep elapsed offset >= 90s, got {:?}",
                req.seek_offset_s
            );
        }
        other => panic!("expected PlayUpnp resume, got {other:?}"),
    }

    // Unsupported Seek unit returns 710 cleanly.
    let bad_seek = soap_post(
        port,
        "/upnp/control/AVTransport",
        "urn:schemas-upnp-org:service:AVTransport:1",
        "Seek",
        "<InstanceID>0</InstanceID><Unit>TRACK_NR</Unit><Target>2</Target>",
    );
    assert!(bad_seek.starts_with("HTTP/1.1 500"));
    assert_eq!(extract_xml_element_text(&bad_seek, "errorCode").as_deref(), Some("710"));

    // Review #5 Finding #28: M-SEARCH without MX is rejected cleanly.
    let msearch_no_mx =
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nST: ssdp:all\r\n\r\n";
    assert!(match_msearch_request(msearch_no_mx, srv.udn()).is_empty());

    // Review #5 Finding #27: 4 idle connections from one peer trigger 503 on the 5th from that peer.
    let mut idle_conns = Vec::new();
    for _ in 0..4 {
        idle_conns.push(TcpStream::connect(("127.0.0.1", port)).unwrap());
    }
    thread::sleep(Duration::from_millis(60));
    let fifth = send_raw_http(
        port,
        &format!("GET /description.xml HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    );
    assert!(
        fifth.starts_with("HTTP/1.1 503"),
        "5th concurrent connection from same peer must hit per-peer cap: {fifth}"
    );
    drop(idle_conns);

    srv.shutdown();
}

#[test]
fn review6_repeat_one_seek_preserves_reltime_and_next_uri() {
    // Review #6 Finding #17: CurrentURI == NextURI (repeat-one). A Seek(REL_TIME, 00:01:00)
    // followed by engine_mark_playing (restart of current, next_gen = None) MUST preserve
    // RelTime >= 60 s and MUST NOT consume st.next!
    let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Gungnir 2");
    let mut srv = UpnpRendererServer::start(
        shared.clone(),
        UpnpRendererConfig {
            friendly_name: "Repeat-One Seek Test".to_string(),
            port: None,
            bind_ip: Some(Ipv4Addr::LOCALHOST),
            enable_ssdp: false,
        },
    )
    .unwrap();
    let st = srv.state();
    let uri = "http://127.0.0.1:50000/repeat.flac";
    st.set_av_transport_uri(
        uri.into(),
        r#"<DIDL-Lite><item><res duration="0:03:00"></res></item></DIDL-Lite>"#.into(),
    );
    st.play().unwrap();
    let _ = shared.take_pending_command();
    st.engine_mark_playing_with_gen(uri, Some(180.0), None);
    st.set_next_av_transport_uri(uri.into(), String::new());

    // Seek to 1:00 on current track.
    st.seek("REL_TIME", "00:01:00").unwrap();
    let seek_req = match shared.take_pending_command() {
        Some(ControlCommand::PlayUpnp(r)) => r,
        other => panic!("expected PlayUpnp from Seek, got {other:?}"),
    };
    assert_eq!(seek_req.seek_offset_s, Some(60.0));
    assert_eq!(
        seek_req.prefetched_next_gen, None,
        "Seek must carry prefetched_next_gen = None"
    );

    // TrackStart arrives for the seeked stream -> must NOT consume next or reset RelTime!
    st.engine_mark_playing_with_gen(&seek_req.uri, Some(180.0), seek_req.prefetched_next_gen);
    let snap = st.snapshot();
    assert!(
        snap.elapsed_s >= 59.9,
        "Seek on repeat-one must preserve RelTime ~= 60s, got {:.2}",
        snap.elapsed_s
    );
    assert_eq!(
        snap.next.as_ref().map(|n| n.uri.as_str()),
        Some(uri),
        "Seek on repeat-one must keep NextURI queued!"
    );

    srv.shutdown();
}

#[test]
fn review6_gena_per_sid_strict_seq_ordering_per_peer_cap_and_ssdp_unicast_mx0() {
    use ap_upnp::gena::{GenaEventHub, GenaService, SubscribeError};
    use ap_upnp::ssdp::parse_msearch_request;

    // 1. Finding #10: Per-SID strict FIFO SEQ ordering (0, 1, 2, ..., 20) across the 4-worker pool!
    let cb_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let cb_port = cb_listener.local_addr().unwrap().port();
    let expected_count = 21usize; // SEQ 0 + 20 notify_service events
    let receiver = thread::spawn(move || {
        let mut seqs = Vec::with_capacity(expected_count);
        for _ in 0..expected_count {
            let (mut conn, _) = cb_listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&raw);
            for line in text.split("\r\n") {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("SEQ") {
                        if let Ok(s) = v.trim().parse::<u32>() {
                            seqs.push(s);
                        }
                    }
                }
            }
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
        seqs
    });

    let hub = GenaEventHub::start();
    let (_sid, _timeout, initial_job) = hub
        .subscribe(
            GenaService::AvTransport,
            &format!("<http://127.0.0.1:{cb_port}/cb>"),
            Some("Second-300"),
            "<e:propertyset/>".to_string(),
        )
        .unwrap();
    hub.enqueue_initial(initial_job);
    for i in 1..=20 {
        hub.notify_service(GenaService::AvTransport, format!("<e:propertyset n=\"{i}\"/>"));
    }

    let got_seqs = receiver.join().unwrap();
    let expected: Vec<u32> = (0..=20).collect();
    assert_eq!(
        got_seqs, expected,
        "GENA NOTIFY events for the same SID must arrive strictly in SEQ 0..=20 order!"
    );

    // 2. Follow-up (gena.rs:129): Per-peer cap (max 4 subscriptions per peer IP) prevents one LAN
    //    peer from exhausting all 16 global slots!
    let hub_cap = GenaEventHub::new_for_test();
    for i in 0..4 {
        hub_cap
            .subscribe(
                GenaService::AvTransport,
                &format!("<http://127.0.0.1:{}/cb>", 50_000 + i),
                Some("Second-1800"),
                "<e:propertyset/>".to_string(),
            )
            .expect("first 4 subscriptions from 127.0.0.1 must succeed");
    }
    let fifth = hub_cap.subscribe(
        GenaService::AvTransport,
        "<http://127.0.0.1:50004/cb>",
        Some("Second-1800"),
        "<e:propertyset/>".to_string(),
    );
    assert_eq!(
        fifth.unwrap_err(),
        SubscribeError::CapacityExceeded,
        "5th healthy subscription from same peer IP must be rejected by per-peer cap"
    );
    // Different loopback IP (127.0.0.2) still succeeds cleanly!
    hub_cap
        .subscribe(
            GenaService::AvTransport,
            "<http://127.0.0.2:50005/cb>",
            Some("Second-1800"),
            "<e:propertyset/>".to_string(),
        )
        .expect("a different peer IP must not be starved by the first peer's 4 subscriptions");
    hub.shutdown();

    // 3. Finding #16: UDA 1.1 unicast M-SEARCH without MX AND M-SEARCH with MX: 0 are answered
    //    immediately (`mx_secs == 0`), while multicast M-SEARCH without MX is still ignored!
    let udn = deterministic_udn("Review 6 SSDP");
    let unicast_no_mx = "M-SEARCH * HTTP/1.1\r\nHOST: 192.168.1.10:1900\r\nMAN: \"ssdp:discover\"\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
    let matched_uni = parse_msearch_request(unicast_no_mx, &udn)
        .expect("unicast M-SEARCH without MX must be answered");
    assert_eq!(matched_uni.mx_secs, 0);
    assert_eq!(
        matched_uni.targets,
        vec!["urn:schemas-upnp-org:device:MediaRenderer:1"]
    );

    let mcast_mx0 = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 0\r\nST: ssdp:all\r\n\r\n";
    let matched_mx0 = parse_msearch_request(mcast_mx0, &udn)
        .expect("M-SEARCH with MX: 0 must be accepted as immediate");
    assert_eq!(matched_mx0.mx_secs, 0);
    assert_eq!(matched_mx0.targets.len(), 6);

    let mcast_no_mx = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nST: ssdp:all\r\n\r\n";
    assert!(
        parse_msearch_request(mcast_no_mx, &udn).is_none(),
        "multicast M-SEARCH missing MX must still be ignored"
    );
}
