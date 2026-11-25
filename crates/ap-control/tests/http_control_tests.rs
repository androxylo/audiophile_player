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
    let page = http_request(
        server.addr(),
        &format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n"),
    );
    assert!(page.starts_with("HTTP/1.1 200 OK"), "got: {page}");
    assert!(page.contains(GITHUB_MANUAL_URL));
    assert!(page.contains("Play Random"));
    assert!(page.contains("Play Sequential"));
    assert!(page.contains("/action/next"));
    assert!(page.contains(">Next<"));
    assert!(page.contains("Pause (Release DAC)"));
    assert!(page.contains("overflow-y:auto")); // scrollable folder & action log
    assert!(page.contains("AlbumOne"));
    assert!(page.contains("sample.flac"));
    assert!(
        !page.contains(".. (Parent Folder)"),
        "root folder view must not show parent link above allowed root"
    );
    assert!(
        !page.to_ascii_lowercase().contains("volume"),
        "volume control must NEVER exist on the page"
    );

    // 2. POST /action/play-sequential dispatches PlaySequential and trips stop_flag to switch cleanly
    let body = format!("dir={}", ap_control::browser::url_encode(&dir.to_string_lossy()));
    let post_seq = format!(
        "POST /action/play-sequential HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let resp_seq = http_request(server.addr(), &post_seq);
    assert!(resp_seq.starts_with("HTTP/1.1 303 See Other"), "got: {resp_seq}");
    assert_eq!(
        shared.take_pending_command(),
        Some(ControlCommand::PlaySequential(shared.allowed_root()))
    );
    assert!(stop_flag.load(Ordering::SeqCst));

    // 3. POST /action/next dispatches Next and sets stop_flag for smooth skip
    stop_flag.store(false, Ordering::SeqCst);
    let post_next = format!(
        "POST /action/next HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let resp_next = http_request(server.addr(), &post_next);
    assert!(resp_next.starts_with("HTTP/1.1 303 See Other"));
    assert_eq!(shared.take_pending_command(), Some(ControlCommand::Next));
    assert!(stop_flag.load(Ordering::SeqCst));

    // 4. POST /action/pause dispatches unified Pause (release DAC) and sets stop_flag
    stop_flag.store(false, Ordering::SeqCst);
    let post_pause = format!(
        "POST /action/pause HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let resp_pause = http_request(server.addr(), &post_pause);
    assert!(resp_pause.starts_with("HTTP/1.1 303 See Other"));
    assert_eq!(shared.take_pending_command(), Some(ControlCommand::Pause));
    assert!(stop_flag.load(Ordering::SeqCst));

    // 5. GET /api/status returns valid JSON including recent bounded action log
    let status = http_request(
        server.addr(),
        &format!("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\n\r\n"),
    );
    assert!(status.starts_with("HTTP/1.1 200 OK"));
    assert!(status.contains("application/json"));
    assert!(status.contains("Command: Pause"));

    server.shutdown();
    // Port should be cleanly released once shutdown finishes
    assert!(TcpListener::bind(server.addr()).is_ok());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn folder_browser_lists_windows_ntfs_junction_mounted_directories() {
    let root = make_temp_music_dir("junction_root");
    let target = make_temp_music_dir("junction_target");
    fs::write(target.join("mounted_track.flac"), b"").unwrap();

    let junction_path = root.join("Favorite");
    let status = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junction_path.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .expect("mklink /J failed");
    assert!(status.success(), "mklink /J exited with {status:?}");

    let listing = ap_control::browser::list_folder_entries(&root);
    assert!(
        listing.subdirs.iter().any(|d| d.ends_with("Favorite")),
        "mounted junction 'Favorite' was missing from subdirs: {:?}",
        listing.subdirs
    );

    let sub_listing = ap_control::browser::list_folder_entries(&junction_path);
    assert!(sub_listing
        .flac_files
        .iter()
        .any(|f| f.ends_with("mounted_track.flac")));

    // Verify the live HTTP server also permits browsing AND playing a .flac inside the NTFS
    // junction mounted under the allowed root (e.g. `~/Music/Favorite`).
    let shared = SharedControl::new(root.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    let j_enc = ap_control::browser::url_encode(&junction_path.to_string_lossy());
    let j_page = http_request(
        server.addr(),
        &format!("GET /?dir={j_enc} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert!(j_page.starts_with("HTTP/1.1 200 OK"), "got: {j_page}");
    assert!(j_page.contains("mounted_track.flac"));
    assert!(j_page.contains(".. (Parent Folder)"));

    let track_via_junction = junction_path.join("mounted_track.flac");
    let body = format!(
        "path={}&dir={}",
        ap_control::browser::url_encode(&track_via_junction.to_string_lossy()),
        j_enc
    );
    let play_resp = http_request(
        server.addr(),
        &format!(
            "POST /action/play-file HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        ),
    );
    assert!(
        play_resp.starts_with("HTTP/1.1 303 See Other"),
        "expected junction play-file to succeed, got: {play_resp}"
    );
    assert!(matches!(
        shared.take_pending_command(),
        Some(ControlCommand::PlayFile(p)) if p.ends_with("mounted_track.flac")
    ));

    server.shutdown();
    let _ = fs::remove_dir(&junction_path);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&target);
}

#[test]
fn rejects_invalid_missing_or_dns_rebinding_host_headers() {
    let dir = make_temp_music_dir("host_check");
    let shared = SharedControl::new(dir.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    let bad_requests = [
        format!("GET / HTTP/1.1\r\nHost: evil.example:{port}\r\n\r\n"),
        "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:9999\r\n\r\n".to_string(),
        "GET /api/status HTTP/1.1\r\n\r\n".to_string(),
        format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nHost: evil.example:{port}\r\n\r\n"),
    ];
    for req in &bad_requests {
        let resp = http_request(server.addr(), req);
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 for {req:?}, got {resp:?}"
        );
    }

    for good_host in [format!("127.0.0.1:{port}"), format!("LOCALHOST:{port}")] {
        let resp = http_request(
            server.addr(),
            &format!("GET /api/status HTTP/1.1\r\nHost: {good_host}\r\n\r\n"),
        );
        assert!(
            resp.starts_with("HTTP/1.1 200 OK"),
            "expected 200 for Host {good_host}, got {resp:?}"
        );
    }

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rejects_cross_site_csrf_posts_and_cross_site_dir_probes() {
    let dir = make_temp_music_dir("csrf_check");
    let shared = SharedControl::new(dir.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    let csrf_posts = [
        format!("POST /action/next HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nContent-Length: 0\r\n\r\n"),
        format!("POST /action/pause HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: null\r\nContent-Length: 0\r\n\r\n"),
        format!("POST /action/next HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: cross-site\r\nContent-Length: 0\r\n\r\n"),
        format!("POST /action/pause HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: same-site\r\nContent-Length: 0\r\n\r\n"),
    ];
    for req in &csrf_posts {
        let resp = http_request(server.addr(), req);
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 CSRF rejection for {req:?}, got {resp:?}"
        );
        assert_eq!(shared.take_pending_command(), None);
    }

    // Cross-site GET /?dir=... and /api/status are also rejected.
    let dir_enc = ap_control::browser::url_encode(&dir.to_string_lossy());
    for req in [
        format!("GET /?dir={dir_enc} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: cross-site\r\n\r\n"),
        format!("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Site: cross-site\r\n\r\n"),
    ] {
        let resp = http_request(server.addr(), &req);
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 for cross-site GET {req:?}, got {resp:?}"
        );
    }

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rejects_unc_device_reserved_and_out_of_root_paths_on_get_and_post() {
    let dir = make_temp_music_dir("path_sec");
    let sub = dir.join("AlbumOne");
    fs::create_dir_all(&sub).unwrap();
    fs::write(dir.join("notes.txt"), b"not audio").unwrap();
    let valid_flac = dir.join("ok.flac");
    fs::write(&valid_flac, b"").unwrap();

    let shared = SharedControl::new(dir.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    let parent_escape = dir
        .join("..")
        .join("outside")
        .to_string_lossy()
        .into_owned();
    let bad_dirs = [
        r"\\attacker.example\share",
        r"//attacker.example/share",
        r"\\?\C:\Windows",
        r"\\.\pipe\ap_fake_pipe",
        r"C:\Windows\System32",
        "CONIN$",
        parent_escape.as_str(),
    ];

    for bad in bad_dirs {
        let enc = ap_control::browser::url_encode(bad);
        let resp = http_request(
            server.addr(),
            &format!("GET /?dir={enc} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 for GET ?dir={bad}, got {resp:?}"
        );

        let body = format!("dir={enc}");
        let post_resp = http_request(
            server.addr(),
            &format!(
                "POST /action/play-random HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        );
        assert!(
            post_resp.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 for POST play-random dir={bad}, got {post_resp:?}"
        );
        assert_eq!(shared.take_pending_command(), None);
    }

    // PlayFile must reject devices, UNC, directories, non-.flac files, and out-of-root paths.
    let bad_files = [
        "CONIN$".to_string(),
        dir.join("CON.flac").to_string_lossy().into_owned(),
        r"\\.\pipe\ap_fake_pipe".to_string(),
        r"\\attacker.example\share\evil.flac".to_string(),
        dir.join("notes.txt").to_string_lossy().into_owned(),
        sub.to_string_lossy().into_owned(),
        r"C:\Windows\System32\drivers\etc\hosts".to_string(),
    ];
    for bad in &bad_files {
        let body = format!("path={}", ap_control::browser::url_encode(bad));
        let resp = http_request(
            server.addr(),
            &format!(
                "POST /action/play-file HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        );
        assert!(
            resp.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 for play-file path={bad}, got {resp:?}"
        );
        assert_eq!(shared.take_pending_command(), None);
    }

    // Valid regular .flac file inside root succeeds.
    let good_body = format!(
        "path={}",
        ap_control::browser::url_encode(&valid_flac.to_string_lossy())
    );
    let good_resp = http_request(
        server.addr(),
        &format!(
            "POST /action/play-file HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{}",
            good_body.len(),
            good_body
        ),
    );
    assert!(
        good_resp.starts_with("HTTP/1.1 303 See Other"),
        "expected 303 for valid flac, got {good_resp:?}"
    );
    assert!(matches!(
        shared.take_pending_command(),
        Some(ControlCommand::PlayFile(p)) if p.ends_with("ok.flac")
    ));

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn slow_idle_client_does_not_block_concurrent_status_or_shutdown_and_caps_8k_headers() {
    let dir = make_temp_music_dir("concurrency_caps");
    let shared = SharedControl::new(dir.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let addr = server.addr();
    let port = addr.port();

    // 1. Open a slowloris/idle client that sends a single byte and stalls.
    let mut slow_client = TcpStream::connect(addr).expect("slow connect");
    slow_client.write_all(b"G").unwrap();
    slow_client.flush().unwrap();

    // 2. Concurrent /api/status must respond promptly (< 500 ms) without waiting on the slow client.
    let t0 = std::time::Instant::now();
    let status = http_request(
        addr,
        &format!("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert!(status.starts_with("HTTP/1.1 200 OK"), "got: {status}");
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(500),
        "concurrent request stalled behind slow client: {:?}",
        t0.elapsed()
    );

    // 3. Headers exceeding MAX_HEADER_BYTES (8 KiB) must be rejected with 431.
    let huge_header = "X-Pad: ".to_string() + &"a".repeat(9000);
    let resp_431 = http_request(
        addr,
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{huge_header}\r\n\r\n"),
    );
    assert!(
        resp_431.starts_with("HTTP/1.1 431 Request Header Fields Too Large"),
        "expected 431 for >8KiB headers, got: {resp_431:?}"
    );

    // 4. Shutdown must complete quickly (< 1 s) even while the slow client stays connected,
    //    and must immediately release the TCP port.
    let t_sd = std::time::Instant::now();
    server.shutdown();
    assert!(
        t_sd.elapsed() < std::time::Duration::from_secs(1),
        "shutdown stalled on slow client: {:?}",
        t_sd.elapsed()
    );
    assert!(TcpListener::bind(addr).is_ok());

    drop(slow_client);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn round_trips_unpaired_utf16_surrogate_paths_losslessly_on_windows() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let root = make_temp_music_dir("surrogate_roundtrip");

    // Build a real NTFS directory and .flac filename containing an unpaired UTF-16 surrogate 0xD800.
    let mut sub_units: Vec<u16> = "Album_".encode_utf16().collect();
    sub_units.push(0xD800);
    let sub_name = OsString::from_wide(&sub_units);
    let sub_dir = root.join(&sub_name);
    fs::create_dir_all(&sub_dir).expect("NTFS should allow unpaired UTF-16 surrogate dir name");

    let mut file_units: Vec<u16> = "Track_".encode_utf16().collect();
    file_units.push(0xD800);
    file_units.extend(".flac".encode_utf16());
    let file_name = OsString::from_wide(&file_units);
    let flac_path = sub_dir.join(&file_name);
    fs::write(&flac_path, b"").expect("NTFS should allow unpaired UTF-16 surrogate file name");

    let shared = SharedControl::new(root.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    // 1. GET / must render an exact encoded token href for the surrogate subfolder.
    let root_page = http_request(
        server.addr(),
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert!(root_page.starts_with("HTTP/1.1 200 OK"));
    let href_marker = "href=\"/?dir=";
    let href_start = root_page
        .find(href_marker)
        .expect("expected dir href in root page")
        + href_marker.len();
    let href_end = root_page[href_start..].find('"').unwrap() + href_start;
    let encoded_dir_param = &root_page[href_start..href_end];
    assert!(
        encoded_dir_param.starts_with("ap-os-u16%3A"),
        "expected lossless UTF-16 token href, got {encoded_dir_param}"
    );

    // 2. Follow the rendered link to browse inside the surrogate subfolder.
    let sub_page = http_request(
        server.addr(),
        &format!("GET /?dir={encoded_dir_param} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert!(sub_page.starts_with("HTTP/1.1 200 OK"), "got: {sub_page}");
    assert!(sub_page.contains("Track_"), "surrogate track missing in HTML");

    // 3. Extract the rendered hidden input `name="path" value="..."` and POST to /action/play-file.
    let path_marker = "name=\"path\" value=\"";
    let p_start = sub_page
        .find(path_marker)
        .expect("expected hidden path input")
        + path_marker.len();
    let p_end = sub_page[p_start..].find('"').unwrap() + p_start;
    let rendered_path_token = &sub_page[p_start..p_end];
    assert!(rendered_path_token.starts_with("ap-os-u16:"));

    let post_body = format!(
        "path={}&dir={}",
        ap_control::browser::url_encode(rendered_path_token),
        encoded_dir_param
    );
    let play_resp = http_request(
        server.addr(),
        &format!(
            "POST /action/play-file HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: {}\r\n\r\n{}",
            post_body.len(),
            post_body
        ),
    );
    assert!(
        play_resp.starts_with("HTTP/1.1 303 See Other"),
        "expected 303 for surrogate play-file, got: {play_resp}"
    );
    assert_eq!(
        shared.take_pending_command(),
        Some(ControlCommand::PlayFile(flac_path.clone())),
        "decoded PlayFile path must match exact OS path with unpaired UTF-16 surrogate"
    );

    server.shutdown();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn browser_listing_uses_natural_order_and_skips_hidden_and_appledouble_entries() {
    let root = make_temp_music_dir("browser_natural_hidden");
    fs::create_dir_all(root.join("Zappa")).unwrap();
    fs::create_dir_all(root.join("abba")).unwrap();
    fs::create_dir_all(root.join("Disc 10")).unwrap();
    fs::create_dir_all(root.join("Disc 2")).unwrap();
    fs::create_dir_all(root.join("...And Justice for All")).unwrap();
    fs::create_dir_all(root.join(".Trash")).unwrap();
    fs::create_dir_all(root.join("$RECYCLE.BIN")).unwrap();
    fs::create_dir_all(root.join("System Volume Information")).unwrap();

    fs::write(root.join("10.flac"), b"").unwrap();
    fs::write(root.join("1.flac"), b"").unwrap();
    fs::write(root.join("2.flac"), b"").unwrap();
    fs::write(root.join("._01.flac"), b"").unwrap();
    fs::write(root.join(".DS_Store"), b"").unwrap();

    #[cfg(windows)]
    {
        let hidden = root.join("win_hidden.flac");
        fs::write(&hidden, b"").unwrap();
        let st = std::process::Command::new("attrib")
            .args(["+H", &hidden.to_string_lossy()])
            .status()
            .expect("attrib +H failed");
        assert!(st.success());
    }

    let listing = ap_control::browser::list_folder_entries_in_root(&root, &root);
    assert!(listing.error.is_none());
    let dir_names: Vec<_> = listing
        .subdirs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    let flac_names: Vec<_> = listing
        .flac_files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();

    assert_eq!(
        dir_names,
        vec!["...And Justice for All", "abba", "Disc 2", "Disc 10", "Zappa"]
    );
    assert_eq!(flac_names, vec!["1.flac", "2.flac", "10.flac"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn trusted_unc_music_root_allows_own_share_and_blocks_foreign_unc_and_pause_always_works() {
    #[cfg(windows)]
    {
        use std::path::Path;
        use ap_control::security::{
            validate_local_path_syntax, validate_path_syntax_for_root, validate_path_within_root,
        };

        // Direct syntax checks asserting specific rejection reasons on Windows (Review #3 #23).
        let err_dos = validate_local_path_syntax(&PathBuf::from("C:\\Music").join("CON.flac"))
            .expect_err("CON.flac must be rejected as reserved DOS name");
        assert!(
            err_dos.contains("reserved Windows device name"),
            "unexpected err: {err_dos}"
        );
        let err_unc = validate_local_path_syntax(Path::new(r"\\attacker\share\a.flac"))
            .expect_err("UNC must be rejected for local root");
        assert!(err_unc.contains("UNC"), "unexpected err: {err_unc}");
        let err_escape =
            validate_local_path_syntax(&PathBuf::from("C:\\").join("..").join("outside"))
                .expect_err("escaping above root must be rejected");
        assert!(
            err_escape.contains("escapes filesystem root"),
            "unexpected err: {err_escape}"
        );

        // Trusted UNC root `\\NAS\Music`: allows paths under `\\NAS\Music`, blocks foreign shares
        // and device paths.
        let unc_root = Path::new(r"\\NAS\Music");
        let sub_ok = validate_path_within_root(Path::new(r"\\nas\music\Artist\Album"), unc_root)
            .expect("subfolder under configured UNC root must be allowed");
        assert!(
            sub_ok.to_string_lossy().to_ascii_lowercase().contains("artist"),
            "unexpected normalized UNC path: {}",
            sub_ok.display()
        );

        let err_foreign = validate_path_within_root(Path::new(r"\\Evil\Share\Album"), unc_root)
            .expect_err("foreign UNC share must remain blocked even with UNC music root");
        assert!(err_foreign.contains("UNC"), "unexpected err: {err_foreign}");

        let err_same_srv_other_share =
            validate_path_within_root(Path::new(r"\\NAS\Private\Secrets"), unc_root)
                .expect_err("different share on same NAS must remain blocked");
        assert!(
            err_same_srv_other_share.contains("UNC")
                || err_same_srv_other_share.contains("outside"),
            "unexpected err: {err_same_srv_other_share}"
        );

        let err_dev =
            validate_path_syntax_for_root(Path::new(r"\\.\pipe\foo"), Some(("nas", "music")))
                .expect_err("Win32 device namespace must stay blocked even with UNC root");
        assert!(err_dev.contains("UNC") || err_dev.contains("device"));

        let err_dos_unc =
            validate_path_within_root(Path::new(r"\\NAS\Music\CON.flac"), unc_root)
                .expect_err("reserved DOS name inside UNC root must still be blocked");
        assert!(err_dos_unc.contains("reserved Windows device name"));
    }

    // HTTP Pause and Next must ALWAYS succeed even if the hidden `dir` field is stale/invalid!
    let dir = make_temp_music_dir("pause_always_works");
    let shared = SharedControl::new(dir.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    for (endpoint, expected_cmd) in [
        ("/action/pause", ControlCommand::Pause),
        ("/action/next", ControlCommand::Next),
    ] {
        let body = format!(
            "dir={}",
            ap_control::browser::url_encode(r"\\stale-server\other\folder")
        );
        let resp = http_request(
            server.addr(),
            &format!(
                "POST {endpoint} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        );
        assert!(
            resp.starts_with("HTTP/1.1 303 See Other"),
            "expected {endpoint} to succeed even with invalid hidden dir, got {resp:?}"
        );
        assert_eq!(shared.take_pending_command(), Some(expected_cmd));
    }

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn anti_clickjacking_headers_and_sec_fetch_dest_block_framing() {
    let dir = make_temp_music_dir("clickjacking_sec");
    let shared = SharedControl::new(dir.clone(), "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    // Normal GET / must carry X-Frame-Options, CSP frame-ancestors 'none', and nosniff.
    let resp = http_request(
        server.addr(),
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
    );
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "unexpected resp: {resp}");
    assert!(
        resp.contains("X-Frame-Options: DENY\r\n"),
        "missing X-Frame-Options header"
    );
    assert!(
        resp.contains("Content-Security-Policy: frame-ancestors 'none'\r\n"),
        "missing CSP frame-ancestors header"
    );
    assert!(
        resp.contains("X-Content-Type-Options: nosniff\r\n"),
        "missing X-Content-Type-Options header"
    );

    // Framed GET / with Sec-Fetch-Dest: iframe/frame/object/embed must be rejected with 403!
    for dest in ["iframe", "frame", "object", "embed"] {
        let framed = http_request(
            server.addr(),
            &format!(
                "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nSec-Fetch-Dest: {dest}\r\n\r\n"
            ),
        );
        assert!(
            framed.starts_with("HTTP/1.1 403 Forbidden"),
            "expected 403 for Sec-Fetch-Dest: {dest}, got {framed:?}"
        );
    }

    server.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn reparse_tag_detector_identifies_ntfs_junctions_and_rejects_cyclic_link_chains() {
    use ap_control::security::{is_symlink_or_mount_point, validate_path_within_root};

    let root = make_temp_music_dir("reparse_tag_check");
    let target = make_temp_music_dir("reparse_tag_target");
    let junction = root.join("MountPoint");
    let st = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &junction.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .status()
        .expect("mklink /J failed");
    assert!(st.success());

    let root_meta = fs::symlink_metadata(&root).unwrap();
    let junc_meta = fs::symlink_metadata(&junction).unwrap();
    assert!(!is_symlink_or_mount_point(&root, &root_meta));
    assert!(
        is_symlink_or_mount_point(&junction, &junc_meta),
        "NTFS junction (IO_REPARSE_TAG_MOUNT_POINT) must be detected by FileType::is_symlink()"
    );
    assert!(validate_path_within_root(&junction, &root).is_ok());

    // Cyclic junction chain (J1 -> J2 -> J1) must be rejected cleanly by MAX_REPARSE_CHAIN_HOPS.
    let cyc_dir = make_temp_music_dir("reparse_cycle_check");
    let j1 = cyc_dir.join("J1");
    let j2 = cyc_dir.join("J2");
    let s1 = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", &j1.to_string_lossy(), &j2.to_string_lossy()])
        .status()
        .unwrap();
    let s2 = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", &j2.to_string_lossy(), &j1.to_string_lossy()])
        .status()
        .unwrap();
    assert!(s1.success() && s2.success());
    let err = validate_path_within_root(&j1, &cyc_dir)
        .expect_err("cyclic junction chain must exceed hop limit and be rejected");
    assert!(err.contains("exceeded"), "unexpected err: {err}");

    let _ = fs::remove_dir(&junction);
    let _ = fs::remove_dir(&j1);
    let _ = fs::remove_dir(&j2);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&target);
    let _ = fs::remove_dir_all(&cyc_dir);
}

#[test]
fn pause_and_next_never_touch_filesystem_and_fs_ops_enforce_bounded_timeout() {
    use ap_control::server::run_fs_op_bounded;
    use std::time::{Duration, Instant};

    // 1. Review #3 Finding #4: even with a blackhole UNC music root (`\\192.0.2.1\Music`, RFC 5737
    // TEST-NET-1 where any `fs::symlink_metadata` would hang for tens of seconds), POST /action/pause
    // and POST /action/next MUST submit commands and return 303 immediately (< 500 ms)!
    let blackhole_root = PathBuf::from(r"\\192.0.2.1\Music");
    let shared = SharedControl::new(blackhole_root, "Test DAC");
    let mut server = ControlServer::start(Arc::clone(&shared), None).unwrap();
    let port = server.addr().port();

    for (endpoint, expected_cmd) in [
        ("/action/pause", ControlCommand::Pause),
        ("/action/next", ControlCommand::Next),
    ] {
        let body = format!(
            "dir={}",
            ap_control::browser::url_encode(r"\\192.0.2.1\Music\StalledArtist\Album")
        );
        let t0 = Instant::now();
        let resp = http_request(
            server.addr(),
            &format!(
                "POST {endpoint} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        );
        let elapsed = t0.elapsed();
        assert!(
            resp.starts_with("HTTP/1.1 303 See Other"),
            "expected 303 for {endpoint}, got {resp:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "{endpoint} must NOT block on filesystem I/O (took {elapsed:?})"
        );
        assert_eq!(shared.take_pending_command(), Some(expected_cmd));
    }

    server.shutdown();

    // 2. Bounded FS helper returns Err(()) promptly when a storage call stalls past its deadline.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let t1 = Instant::now();
    let timed_out = run_fs_op_bounded(Duration::from_millis(40), move || {
        let _ = rx.recv_timeout(Duration::from_secs(5));
        42_u32
    });
    let elapsed_fs = t1.elapsed();
    let _ = tx.send(());
    assert!(timed_out.is_err(), "stalled FS op must time out cleanly");
    assert!(
        elapsed_fs < Duration::from_millis(500),
        "bounded FS op took too long to time out: {elapsed_fs:?}"
    );
}

#[test]
fn stalled_root_renders_full_control_html_with_pause_and_never_starves_other_volumes() {
    // Review #4 Finding #3 (MEDIUM):
    // 1. Saturating the per-root helper budget for a stalled NAS (`unc:\\stalled-nas\music`)
    //    must NOT starve operations on a local drive (`drive:c` or another root).
    // 2. When `GET /` hits a stalled/saturated root, it MUST render the full HTML control page
    //    (with Pause, Next, Play buttons + error banner) instead of a bare 504 text page!
    use ap_control::server::{
        MAX_FS_HELPERS_PER_ROOT, fs_volume_key, run_fs_op_bounded_for_key,
    };
    use std::sync::{Arc as StdArc, Condvar, Mutex};
    use std::time::Duration;

    let stalled_root = PathBuf::from(r"\\192.0.2.99\StalledShare");
    let stalled_key = fs_volume_key(&stalled_root);
    let local_dir = make_temp_music_dir("local_unstarved");
    let local_key = fs_volume_key(&local_dir);
    assert_ne!(stalled_key, local_key);

    let release = StdArc::new((Mutex::new(false), Condvar::new()));

    // Fill ALL `MAX_FS_HELPERS_PER_ROOT` slots for `stalled_key` with blocked helper threads.
    for _ in 0..MAX_FS_HELPERS_PER_ROOT {
        let rel = StdArc::clone(&release);
        let res = run_fs_op_bounded_for_key(&stalled_key, Duration::from_millis(15), move || {
            let (lock, cv) = &*rel;
            let mut done = lock.lock().unwrap();
            while !*done {
                let (g, _) = cv.wait_timeout(done, Duration::from_secs(5)).unwrap();
                done = g;
            }
        });
        assert!(res.is_err(), "blocked helper should time out while holding its per-root slot");
    }

    // Now the stalled NAS bucket is 100% saturated:
    // (a) A local-drive HTTP GET / MUST still succeed immediately (unstarved!).
    let local_shared = SharedControl::new(local_dir.clone(), "Test DAC");
    let mut local_server = ControlServer::start(Arc::clone(&local_shared), None).unwrap();
    let local_port = local_server.addr().port();
    let local_resp = http_request(
        local_server.addr(),
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{local_port}\r\n\r\n"),
    );
    assert!(
        local_resp.starts_with("HTTP/1.1 200 OK"),
        "local drive must NOT be starved by stalled NAS helpers, got: {local_resp:?}"
    );
    assert!(local_resp.contains("/action/pause"));
    local_server.shutdown();

    // (b) GET / on the saturated stalled NAS MUST return full HTML with Pause/Next buttons and
    // an error banner (NOT a bare text 504 page!).
    let stalled_shared = SharedControl::new(stalled_root, "Test DAC");
    let mut stalled_server = ControlServer::start(Arc::clone(&stalled_shared), None).unwrap();
    let stalled_port = stalled_server.addr().port();
    let stalled_resp = http_request(
        stalled_server.addr(),
        &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{stalled_port}\r\n\r\n"),
    );
    assert!(
        stalled_resp.starts_with("HTTP/1.1 200 OK"),
        "GET / on stalled root must render HTML control page, not bare 504: {stalled_resp:?}"
    );
    assert!(
        stalled_resp.contains("text/html"),
        "expected text/html content-type: {stalled_resp:?}"
    );
    assert!(
        stalled_resp.contains("/action/pause") && stalled_resp.contains("/action/next"),
        "fallback page must keep Pause and Next buttons reachable!"
    );
    assert!(
        stalled_resp.contains("Storage timed out or is busy"),
        "fallback page must show clear error banner: {stalled_resp:?}"
    );
    stalled_server.shutdown();

    // Release blocked helper threads cleanly.
    let (lock, cv) = &*release;
    *lock.lock().unwrap() = true;
    cv.notify_all();
    let _ = fs::remove_dir_all(&local_dir);
}
