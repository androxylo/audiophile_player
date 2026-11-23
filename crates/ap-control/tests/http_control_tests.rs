//! TDD unit & live localhost HTTP integration tests for `ap-control`.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ap_control::GITHUB_MANUAL_URL;
use ap_control::log::ActionLog;
use ap_control::server::{
    ControlServer, DEFAULT_PORT_END, DEFAULT_PORT_START, bind_first_available_port,
};
use ap_control::state::{ControlCommand, SharedControl};

fn make_temp_music_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ap_control_test_{tag}_{}_{}",
        std::process::id(),
        stamp
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn http_request(addr: std::net::SocketAddr, raw: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to control server");
    stream.write_all(raw.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    resp
}

#[test]
fn action_log_is_strictly_bounded_and_discards_oldest_entries() {
    let mut log = ActionLog::new(50);
    for i in 0..120 {
        log.push(format!("event-{i}"));
    }
    assert_eq!(log.len(), 50);
    let snap = log.snapshot();
    assert!(snap.first().unwrap().contains("event-70"));
    assert!(snap.last().unwrap().contains("event-119"));
}

#[test]
fn default_port_range_starts_at_8085_avoids_80_and_8080_and_skips_loopback_and_wildcard_listeners() {
    assert_eq!(DEFAULT_PORT_START, 8085);
    assert!(!(DEFAULT_PORT_START..=DEFAULT_PORT_END).contains(&80));
    assert!(!(DEFAULT_PORT_START..=DEFAULT_PORT_END).contains(&8080));

    // Occupy port A on 127.0.0.1 and port B on wildcard 0.0.0.0 (which Windows otherwise
    // lets 127.0.0.1 shadow if not probed!) — picker must safely skip BOTH without disturbing them.
    let (occ_v4_loopback, port_a) = bind_first_available_port(8140, 8180).unwrap();
    let port_b = port_a + 1;
    let occ_wildcard = TcpListener::bind(("0.0.0.0", port_b));

    let (_picked, picked_port) = bind_first_available_port(port_a, 8190).unwrap();
    if occ_wildcard.is_ok() {
        assert_eq!(
            picked_port,
            port_b + 1,
            "must skip both 127.0.0.1:{port_a} AND wildcard 0.0.0.0:{port_b}"
        );
    } else {
        assert!(picked_port > port_a);
    }

    // Confirm original occupied listeners are still completely alive & untouched.
    assert_eq!(occ_v4_loopback.local_addr().unwrap().port(), port_a);
    drop(occ_wildcard);
    drop(occ_v4_loopback);
}

#[test]
fn http_server_serves_scrollable_no_volume_ui_and_routes_play_pause_commands() {
    let dir = make_temp_music_dir("http_e2e");
    fs::create_dir_all(dir.join("AlbumOne")).unwrap();
    fs::write(dir.join("sample.flac"), b"").unwrap();

    let shared = SharedControl::new(dir.clone(), "Gungnir 2 Unison 384");
    let stop_flag = Arc::new(AtomicBool::new(false));
    shared.attach_sink_stop_flag(Arc::clone(&stop_flag));

    // Bind cleanly in default conventional range
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();
    assert!(
        (DEFAULT_PORT_START..=DEFAULT_PORT_END).contains(&port),
        "unexpected port {port}"
    );

    // 1. GET / must render scrollable folder browser, manual link, Play Random/Sequential, Pause,
    //    bottom action log — and STRICTLY NO volume control.
    let page = http_request(server.addr(), "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(page.starts_with("HTTP/1.1 200 OK"));
    assert!(page.contains(GITHUB_MANUAL_URL));
    assert!(page.contains("Play Random"));
    assert!(page.contains("Play Sequential"));
    assert!(page.contains("Pause (Release DAC)"));
    assert!(page.contains("overflow-y:auto")); // scrollable folder & action log
    assert!(page.contains("AlbumOne"));
    assert!(page.contains("sample.flac"));
    assert!(
        !page.to_ascii_lowercase().contains("volume"),
        "volume control must NEVER exist on the page"
    );

    // 2. POST /action/play-sequential dispatches PlaySequential and trips stop_flag to switch cleanly
    let body = format!("dir={}", ap_control::browser::url_encode(&dir.to_string_lossy()));
    let post_seq = format!(
        "POST /action/play-sequential HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let resp_seq = http_request(server.addr(), &post_seq);
    assert!(resp_seq.starts_with("HTTP/1.1 303 See Other"));
    assert_eq!(
        shared.take_pending_command(),
        Some(ControlCommand::PlaySequential(dir.clone()))
    );
    assert!(stop_flag.load(Ordering::SeqCst));

    // 3. POST /action/pause dispatches unified Pause (release DAC) and sets stop_flag
    stop_flag.store(false, Ordering::SeqCst);
    let post_pause = format!(
        "POST /action/pause HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let resp_pause = http_request(server.addr(), &post_pause);
    assert!(resp_pause.starts_with("HTTP/1.1 303 See Other"));
    assert_eq!(shared.take_pending_command(), Some(ControlCommand::Pause));
    assert!(stop_flag.load(Ordering::SeqCst));

    // 4. GET /api/status returns valid JSON including recent bounded action log
    let status = http_request(
        server.addr(),
        "GET /api/status HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(status.starts_with("HTTP/1.1 200 OK"));
    assert!(status.contains("application/json"));
    assert!(status.contains("Command: Pause"));

    server.shutdown();
    // Port should be cleanly released once shutdown finishes
    assert!(TcpListener::bind(server.addr()).is_ok());
    let _ = fs::remove_dir_all(&dir);
}
