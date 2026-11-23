//! Ultra-minimal HTML control page and lightweight JSON status serializer.
//!
//! Deliberately excludes volume controls, EQ/DSP sliders, and heavy JS frameworks.
//! Features:
//! - Direct link to the GitHub manual (`https://github.com/androxylo/audiophile_player`).
//! - Scrollable folder browser resilient to large directories.
//! - Unified controls: **Play Random**, **Play Sequential**, and **Pause** (releases DAC driver).
//! - Bounded bottom action log (auto-discards old history, live-updated via `/api/status`).

use crate::GITHUB_MANUAL_URL;
use crate::browser::{FolderListing, html_escape, json_escape, url_encode};
use crate::state::ControlSnapshot;

/// Render the minimal control page for the currently browsed folder and player state.
pub fn render_control_page(snapshot: &ControlSnapshot, listing: &FolderListing) -> String {
    let dir_str = listing.current_dir.to_string_lossy();
    let dir_html = html_escape(&dir_str);
    let dac_html = html_escape(&snapshot.dac_name);
    let status_badge = if snapshot.is_playing {
        format!(
            "<span class=\"badge playing\">{}</span>",
            html_escape(snapshot.mode.as_str())
        )
    } else {
        "<span class=\"badge paused\">Paused (DAC released)</span>".to_string()
    };

    let now_playing = match (&snapshot.current_track, &snapshot.current_format) {
        (Some(t), Some(f)) => format!("{} &mdash; {}", html_escape(t), html_escape(f)),
        (Some(t), None) => html_escape(t),
        _ => "Idle &mdash; DAC pin closed (available to Audirvana / other players)".to_string(),
    };

    let mut items_html = String::new();
    if let Some(err) = &listing.error {
        items_html.push_str(&format!(
            "<div class=\"err\">{}</div>",
            html_escape(err)
        ));
    }
    if let Some(parent) = &listing.parent_dir {
        let p_enc = url_encode(&parent.to_string_lossy());
        items_html.push_str(&format!(
            "<div class=\"row dir\"><a href=\"/?dir={p_enc}\">.. (Parent Folder)</a></div>"
        ));
    }
    for sub in &listing.subdirs {
        let name = sub
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| sub.to_string_lossy().into_owned());
        let p_enc = url_encode(&sub.to_string_lossy());
        items_html.push_str(&format!(
            "<div class=\"row dir\"><a href=\"/?dir={p_enc}\">[DIR] {}</a></div>",
            html_escape(&name)
        ));
    }
    for file in &listing.flac_files {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string_lossy().into_owned());
        let f_html = html_escape(&file.to_string_lossy());
        items_html.push_str(&format!(
            "<div class=\"row file\">\
             <form method=\"POST\" action=\"/action/play-file\">\
             <input type=\"hidden\" name=\"path\" value=\"{f_html}\">\
             <input type=\"hidden\" name=\"dir\" value=\"{dir_html}\">\
             <button type=\"submit\" class=\"link-btn\">&#9835; {}</button>\
             </form></div>",
            html_escape(&name)
        ));
    }
    if listing.subdirs.is_empty() && listing.flac_files.is_empty() && listing.error.is_none() {
        items_html.push_str("<div class=\"row empty\">(No subfolders or .flac files here)</div>");
    }

    let mut log_html = String::new();
    for line in &snapshot.recent_actions {
        log_html.push_str(&html_escape(line));
        log_html.push('\n');
    }

    format!(
        "<!doctype html>\n\
<html lang=\"en\">\n\
<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
<title>Audiophile Player</title>\n\
<style>\n\
:root{{--bg:#121316;--panel:#1b1d22;--border:#2d313a;--fg:#e6e8ec;--muted:#9aa0ab;--accent:#4c9aff;--ok:#3fb950;--warn:#d29922;}}\n\
*{{box-sizing:border-box;}}body{{margin:0;padding:16px;font-family:system-ui,-apple-system,Segoe UI,sans-serif;background:var(--bg);color:var(--fg);max-width:960px;margin-inline:auto;}}\n\
header{{display:flex;justify-content:space-between;align-items:baseline;border-bottom:1px solid var(--border);padding-bottom:8px;margin-bottom:12px;}}\n\
h1{{margin:0;font-size:1.15rem;letter-spacing:.02em;}}a{{color:var(--accent);text-decoration:none;}}a:hover{{text-decoration:underline;}}\n\
.card{{background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:10px 12px;margin-bottom:12px;}}\n\
.status-line{{display:flex;gap:10px;align-items:center;flex-wrap:wrap;font-size:.92rem;}}\n\
.badge{{padding:2px 8px;border-radius:4px;font-weight:600;font-size:.82rem;}}\n\
.badge.playing{{background:rgba(63,185,80,.18);color:var(--ok);border:1px solid rgba(63,185,80,.4);}}\n\
.badge.paused{{background:rgba(210,153,34,.18);color:var(--warn);border:1px solid rgba(210,153,34,.4);}}\n\
.now{{margin-top:6px;font-size:.88rem;color:var(--muted);word-break:break-all;}}\n\
.controls{{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:10px;}}\n\
button{{background:#272c36;color:var(--fg);border:1px solid var(--border);border-radius:5px;padding:7px 14px;cursor:pointer;font-size:.9rem;font-weight:600;}}\n\
button:hover{{background:#323946;}}button.primary{{border-color:var(--accent);}}button.pause{{border-color:var(--warn);}}\n\
.path-bar{{display:flex;gap:6px;margin-bottom:8px;}}\n\
.path-bar input{{flex:1;background:#0f1115;color:var(--fg);border:1px solid var(--border);border-radius:4px;padding:6px 8px;font-family:Consolas,monospace;font-size:.86rem;}}\n\
.browser{{max-height:44vh;overflow-y:auto;border:1px solid var(--border);border-radius:4px;background:#0f1115;padding:4px 0;}}\n\
.row{{padding:4px 10px;font-family:Consolas,monospace;font-size:.86rem;border-bottom:1px solid #1a1d24;}}\n\
.row:last-child{{border-bottom:none;}}.row.empty{{color:var(--muted);}}.err{{color:#f85149;padding:8px 10px;}}\n\
.row form{{margin:0;}}.link-btn{{background:none;border:none;color:var(--fg);padding:0;font-family:Consolas,monospace;font-size:.86rem;font-weight:400;text-align:left;width:100%;}}\n\
.link-btn:hover{{background:none;color:var(--accent);}}\n\
.log-title{{font-size:.82rem;color:var(--muted);margin-bottom:6px;display:flex;justify-content:space-between;}}\n\
pre#action-log{{margin:0;max-height:170px;overflow-y:auto;background:#0f1115;border:1px solid var(--border);border-radius:4px;padding:8px;font-family:Consolas,monospace;font-size:.8rem;line-height:1.4;white-space:pre-wrap;word-break:break-word;}}\n\
</style>\n\
</head>\n\
<body>\n\
<header>\n\
  <h1>Audiophile Player <small style=\"color:var(--muted);font-weight:400\">(WDM-KS WaveRT &middot; Zero Mixer)</small></h1>\n\
  <a href=\"{GITHUB_MANUAL_URL}\" target=\"_blank\" rel=\"noopener\">Manual (GitHub)</a>\n\
</header>\n\
<section class=\"card\">\n\
  <div class=\"status-line\">\n\
    <span id=\"status-badge\">{status_badge}</span>\n\
    <span>DAC: <strong>{dac_html}</strong></span>\n\
  </div>\n\
  <div class=\"now\" id=\"now-playing\">{now_playing}</div>\n\
</section>\n\
<section class=\"card\">\n\
  <div class=\"controls\">\n\
    <form method=\"POST\" action=\"/action/play-random\">\n\
      <input type=\"hidden\" name=\"dir\" value=\"{dir_html}\">\n\
      <button type=\"submit\" class=\"primary\">Play Random</button>\n\
    </form>\n\
    <form method=\"POST\" action=\"/action/play-sequential\">\n\
      <input type=\"hidden\" name=\"dir\" value=\"{dir_html}\">\n\
      <button type=\"submit\" class=\"primary\">Play Sequential</button>\n\
    </form>\n\
    <form method=\"POST\" action=\"/action/pause\">\n\
      <input type=\"hidden\" name=\"dir\" value=\"{dir_html}\">\n\
      <button type=\"submit\" class=\"pause\">Pause (Release DAC)</button>\n\
    </form>\n\
  </div>\n\
  <form class=\"path-bar\" method=\"GET\" action=\"/\">\n\
    <input type=\"text\" name=\"dir\" value=\"{dir_html}\" aria-label=\"Folder path\">\n\
    <button type=\"submit\">Open Folder</button>\n\
  </form>\n\
  <div class=\"browser\" aria-label=\"Scrollable folder contents\">\n\
    {items_html}\n\
  </div>\n\
</section>\n\
<section class=\"card\">\n\
  <div class=\"log-title\"><span>Recent Actions</span><span>Bounded history (latest 50)</span></div>\n\
  <pre id=\"action-log\">{log_html}</pre>\n\
</section>\n\
<script>\n\
async function poll(){{try{{const r=await fetch('/api/status');if(!r.ok)return;const d=await r.json();\n\
const b=document.getElementById('status-badge');b.innerHTML=d.playing?'<span class=\"badge playing\">'+d.mode+'</span>':'<span class=\"badge paused\">Paused (DAC released)</span>';\n\
const n=document.getElementById('now-playing');n.textContent=d.track?(d.format?d.track+' — '+d.format:d.track):'Idle — DAC pin closed (available to Audirvana / other players)';\n\
const l=document.getElementById('action-log');const atBottom=(l.scrollHeight-l.scrollTop-l.clientHeight)<24;l.textContent=d.log.join('\\n');if(atBottom)l.scrollTop=l.scrollHeight;}}catch(_){{}}}}\n\
setInterval(poll,2000);\n\
</script>\n\
</body>\n\
</html>\n"
    )
}

/// Serialize minimal status & bounded action log to JSON for `/api/status` (zero serde deps).
pub fn render_status_json(snapshot: &ControlSnapshot) -> String {
    let track_json = match &snapshot.current_track {
        Some(t) => json_escape(t),
        None => "null".to_string(),
    };
    let fmt_json = match &snapshot.current_format {
        Some(f) => json_escape(f),
        None => "null".to_string(),
    };
    let log_items: Vec<String> = snapshot
        .recent_actions
        .iter()
        .map(|s| json_escape(s))
        .collect();
    format!(
        "{{\"playing\":{},\"mode\":{},\"dac\":{},\"folder\":{},\"track\":{},\"format\":{},\"log\":[{}]}}",
        snapshot.is_playing,
        json_escape(snapshot.mode.as_str()),
        json_escape(&snapshot.dac_name),
        json_escape(&snapshot.active_folder.to_string_lossy()),
        track_json,
        fmt_json,
        log_items.join(",")
    )
}
