// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! The viewer-facing web server: HLS player page with a real anti-ghosting delay, plus /healthz and /version.
//!
//! The delay is enforced server side: the playlist hides the newest <delay> seconds and the segment
//! endpoint refuses those files.

use std::sync::Arc;

use axum::Router;
use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::app::App;
use crate::config::ct_eq;
use crate::hls::{Phase, WINDOW, delayed_view, is_segment_name, phase};
use crate::version::{APP_NAME, OWNER, RELEASE_DATE, VERSION};

const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{title}}</title>
{{icon_link}}<style>
 html,body{margin:0;height:100%;background:#0e0f13;color:#ddd;font-family:system-ui,sans-serif}
 .wrap{max-width:1280px;margin:0 auto;padding:12px}
 video{width:100%;background:#000;border-radius:8px}
 .bar{display:flex;gap:12px;align-items:center;padding:8px 2px}
 .live{background:#e5383b;color:#fff;border-radius:4px;padding:2px 8px;font-weight:600;font-size:13px}
 #msg,.foot{opacity:.7;font-size:14px}
 .foot{padding:6px 2px;font-size:12px}
 .brand{display:flex;align-items:center;gap:10px;padding:2px 2px 6px}
 .brand img{max-height:44px;max-width:160px;border-radius:6px}
 .brand span{font-size:18px;font-weight:700}
 #eye{margin-left:auto;font-size:14px;opacity:.85;white-space:nowrap}
</style></head><body><div class="wrap">
{{brand}}<div class="bar"><span class="live">LIVE</span><strong>{{title}}</strong><span id="msg">Loading the player...</span><span id="eye"></span></div>
<video id="v" controls autoplay muted playsinline></video>
<div id="dbg" class="foot"></div>
<div class="foot">{{app}} v{{version}} &middot; owner: {{owner}} &middot; free software (GNU GPL v3+)</div>
</div>
<noscript><p style="padding:8px">This player needs JavaScript.</p></noscript>
<script src="/player/hls.min.js"></script>
<script>
(function(){
 var v=document.getElementById('v'), msg=document.getElementById('msg'), dbg=document.getElementById('dbg'), eye=document.getElementById('eye');
 // a random id for this tab, so the bot can count viewers; nothing about you is sent or kept
 var vid=''; try{ var a=new Uint8Array(12); crypto.getRandomValues(a); for(var i=0;i<a.length;i++){ vid+=('0'+a[i].toString(16)).slice(-2); } }catch(e){ vid=String(Math.random()).slice(2)+String(Date.now()); }
 vid=vid.replace(/[^a-z0-9]/gi,'').slice(0,32);
 var key='{{key}}', src='/hls/'+key+'/live.m3u8', hls=null, playing=false, note='{{delay_note}}';
 function say(t){ msg.textContent=t; }
 function poll(){
  fetch('/status/'+key+'?v='+vid+'&p='+(playing?1:0),{credentials:'same-origin',cache:'no-store'}).then(function(r){
   if(r.status===403){ say('Access key missing - open the Watch link from Discord again.'); return null; }
   if(r.status===404){ say('This broadcast is not running right now.'); return null; }
   return r.json();
  }).then(function(j){ if(!j) return; if(!playing) say(j.message); if(typeof j.viewers==='number'){ eye.textContent='\uD83D\uDC41 '+j.viewers+' watching'; } }).catch(function(){ if(!playing) say('Cannot reach the video server - check your connection.'); });
 }
 setInterval(poll,2500); poll();
 function start(){
  if(window.Hls && Hls.isSupported()){
   hls=new Hls({liveSyncDurationCount:3, manifestLoadingMaxRetry:60, manifestLoadingRetryDelay:2000, manifestLoadingMaxRetryTimeout:5000,
                levelLoadingMaxRetry:60, levelLoadingRetryDelay:2000, fragLoadingMaxRetry:8});
   hls.loadSource(src); hls.attachMedia(v);
   hls.on(Hls.Events.MANIFEST_PARSED,function(){ var p=v.play(); if(p&&p.catch){ p.catch(function(){ say('Press the play button to start the video.'); }); } });
   hls.on(Hls.Events.ERROR,function(e,d){
    dbg.textContent='player: '+d.details+(d.response&&d.response.code?' (HTTP '+d.response.code+')':'');
    if(!d.fatal) return;
    if(d.type===Hls.ErrorTypes.MEDIA_ERROR){ hls.recoverMediaError(); return; }
    hls.destroy(); hls=null; setTimeout(start,3000);
   });
  } else if(v.canPlayType('application/vnd.apple.mpegurl')){ v.src=src; v.play&&v.play().catch(function(){}); }
  else { say('This browser cannot play live video (no HLS support). Try Chrome, Edge, Firefox or Safari.'); }
 }
 v.addEventListener('playing',function(){ playing=true; say(note||'Live'); });
 v.addEventListener('waiting',function(){ playing=false; });
 start();
})();
</script></body></html>
"##;

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> String {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).unwrap_or("").split(';').filter_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    }).next().unwrap_or_default()
}

fn text(code: StatusCode, body: &str) -> Response {
    (code, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body.to_string()).into_response()
}

/// A small readable page instead of a bare error line (people open the address in a browser).
fn info_page(code: StatusCode, title: &str, body_html: &str) -> Response {
    let page = format!("<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{t}</title>\
        <body style=\"font:16px system-ui,sans-serif;background:#0d0e14;color:#e8e9f0;max-width:34rem;margin:12vh auto;padding:0 1rem\">\
        <h1 style=\"font-size:1.3rem\">{t}</h1>{b}<p style=\"color:#8b8fa3;font-size:.85rem;margin-top:2rem\">{a} v{v} - {o} - free software (GNU GPL v3+), no warranty</p></body>",
        t = html_escape(title), b = body_html, a = html_escape(APP_NAME), v = html_escape(VERSION), o = html_escape(OWNER));
    (code, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], page).into_response()
}

const NO_STREAM: &str = "<p>Nothing with video is being broadcast right now.</p>\
    <p>The video link appears in Discord when someone starts a broadcast with video (<code>/gtv start</code> with <code>stream:True</code>). \
    Open that link, or refresh this page once a broadcast is running.</p>";

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let token = app.cfg().web_token();
    token.is_empty() || ct_eq(cookie_value(headers, "gtv").as_bytes(), token.as_bytes())
}

#[derive(Deserialize)]
struct WatchQuery {
    t: Option<String>,
}

#[derive(Deserialize)]
struct StatusQuery {
    /// The page's random id for this tab.
    v: Option<String>,
    /// 1 while the video is actually playing.
    p: Option<String>,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn version() -> Response {
    let body = serde_json::json!({"version": VERSION, "released": RELEASE_DATE, "owner": OWNER}).to_string();
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// The bare address (what people type or what the admin panel shows as the "web address").
async fn index(State(app): State<Arc<App>>, Query(q): Query<WatchQuery>, headers: HeaderMap) -> Response {
    index_inner(&app, q, &headers, None)
}

/// The bare address of a server's own port: straight to that server's player.
async fn index_only(State(app): State<Arc<App>>, Extension(only): Extension<OnlyServer>, Query(q): Query<WatchQuery>, headers: HeaderMap) -> Response {
    index_inner(&app, q, &headers, Some(&only.0))
}

fn index_inner(app: &App, q: WatchQuery, headers: &HeaderMap, only: Option<&str>) -> Response {
    let token = app.cfg().web_token();
    let mut cookie: Option<HeaderValue> = None;
    if !token.is_empty() {
        if q.t.as_deref().is_some_and(|given| ct_eq(given.as_bytes(), token.as_bytes())) {
            cookie = HeaderValue::from_str(&format!("gtv={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400", token)).ok();
        } else if !authorized(app, headers) {
            return info_page(StatusCode::FORBIDDEN, "Open the link from Discord",
                "<p>This player is private. Use the <b>Watch</b> button / link in the Discord broadcast message - it carries the access key.</p>");
        }
    }
    let mut keys: Vec<(String, String)> = app.streamers.lock().unwrap().iter().filter(|(k, _)| only.is_none_or(|o| o == k.as_str())).map(|(k, s)| (k.clone(), s.name_clean().to_string())).collect();
    keys.sort();
    let mut resp = match keys.len() {
        0 => info_page(StatusCode::OK, "No live broadcast", NO_STREAM),
        1 => (StatusCode::FOUND, [(header::LOCATION, format!("/watch/{}", keys[0].0))], "").into_response(),
        _ => {
            let items: String = keys.iter().map(|(k, n)| format!("<li><a style=\"color:#8ea2ff\" href=\"/watch/{k}\">{k}</a>{n}</li>", k = html_escape(k), n = if n.is_empty() { String::new() } else { format!(" - {}", html_escape(n)) })).collect();
            info_page(StatusCode::OK, "Live broadcasts", &format!("<ul>{}</ul>", items))
        }
    };
    if let Some(c) = cookie {
        resp.headers_mut().append(header::SET_COOKIE, c);
    }
    resp
}

async fn not_found() -> Response {
    info_page(StatusCode::NOT_FOUND, "Nothing here", "<p>This is the video player's address, not a page. <a style=\"color:#8ea2ff\" href=\"/\">Go to the live broadcast</a>.</p>")
}

async fn watch_page(State(app): State<Arc<App>>, Path(key): Path<String>, Query(q): Query<WatchQuery>, headers: HeaderMap) -> Response {
    let Some(s) = app.streamer(&key) else { return info_page(StatusCode::NOT_FOUND, "No live stream for this server", NO_STREAM) };
    let token = app.cfg().web_token();
    let mut extra: Vec<(header::HeaderName, HeaderValue)> = vec![];
    if !token.is_empty() {
        if q.t.as_deref().is_some_and(|given| ct_eq(given.as_bytes(), token.as_bytes())) {
            let cookie = format!("gtv={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400", token);
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                extra.push((header::SET_COOKIE, v));
            }
        } else if !authorized(&app, &headers) {
            return text(StatusCode::FORBIDDEN, "Use the link from Discord.");
        }
    }
    let note = if s.delay() > 0 { format!("{}s delay", s.delay()) } else { String::new() };
    let title = if s.name_clean().is_empty() { key.clone() } else { s.name_clean().to_string() };
    let cfg = app.cfg();
    let brand = page_brand(&cfg.branding.name, &cfg.branding.logo);
    let icon_link = match page_src(&cfg.branding.icon) {
        Some(src) => format!("<link rel=\"icon\" href=\"{}\">\n", html_escape(&src)),
        None => String::new(),
    };
    let body = PAGE
        .replace("{{brand}}", &brand)
        .replace("{{icon_link}}", &icon_link)
        .replace("{{title}}", &html_escape(&format!("{} - live", title)))
        .replace("{{key}}", &html_escape(&key))
        .replace("{{delay_note}}", &html_escape(&note))
        .replace("{{app}}", &html_escape(APP_NAME))
        .replace("{{version}}", &html_escape(VERSION))
        .replace("{{owner}}", &html_escape(OWNER));
    let mut resp = ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response();
    for (k, v) in extra {
        resp.headers_mut().append(k, v);
    }
    resp
}

async fn hls_file(State(app): State<Arc<App>>, Path((key, file)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let Some(s) = app.streamer(&key) else { return text(StatusCode::NOT_FOUND, "not available") };
    if !authorized(&app, &headers) {
        return text(StatusCode::FORBIDDEN, "not available");
    }
    let dir = s.hls_dir();
    let not_ready = || (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "3"), (header::CONTENT_TYPE, "text/plain; charset=utf-8")], "stream not ready").into_response();
    if file == "live.m3u8" {
        let Ok(raw) = tokio::fs::read_to_string(dir.join("live.m3u8")).await else { return not_ready() };
        return match delayed_view(&raw, s.delay() as f64, WINDOW).0 {
            Some(view) => ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl"), (header::CACHE_CONTROL, "no-store")], view).into_response(),
            None => not_ready(),
        };
    }
    if !is_segment_name(&file) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let Ok(raw) = tokio::fs::read_to_string(dir.join("live.m3u8")).await else { return text(StatusCode::NOT_FOUND, "not found") };
    let (_, allowed) = delayed_view(&raw, s.delay() as f64, WINDOW);
    if !allowed.contains(&file) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    match tokio::fs::read(dir.join(&file)).await {
        Ok(data) => ([(header::CONTENT_TYPE, "video/mp2t")], data).into_response(),
        Err(_) => text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Where the page loads a stored picture or typed address from (the same server for stored files).
fn page_src(value: &str) -> Option<String> {
    if crate::brand::valid_asset_name(value) {
        Some(format!("/brand/{}", value))
    } else if crate::brand::is_web_address(value) {
        Some(value.to_string())
    } else {
        None
    }
}

/// The clan's name and logo above the player (nothing when neither is set).
fn page_brand(name: &str, logo: &str) -> String {
    let name = name.trim();
    let img = page_src(logo).map(|src| format!("<img src=\"{}\" alt=\"\">", html_escape(&src))).unwrap_or_default();
    if name.is_empty() && img.is_empty() {
        return String::new();
    }
    let label = if name.is_empty() { String::new() } else { format!("<span>{}</span>", html_escape(name)) };
    format!("<div class=\"brand\">{}{}</div>\n", img, label)
}

/// Clan pictures. Public on purpose: Discord's servers fetch them to show the logo in the posts. Only files the admin
/// panel stored (a fixed name pattern in one folder) can be reached.
async fn brand_file(State(app): State<Arc<App>>, Path(file): Path<String>) -> Response {
    if !crate::brand::valid_asset_name(&file) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let ext = file.rsplit('.').next().unwrap_or("png");
    match tokio::fs::read(crate::brand::dir(&app.cfg()).join(&file)).await {
        Ok(data) => (
            [(header::CONTENT_TYPE, crate::brand::content_type(ext)), (header::CACHE_CONTROL, "public, max-age=86400, immutable"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")],
            data,
        ).into_response(),
        Err(_) => text(StatusCode::NOT_FOUND, "not found"),
    }
}

/// The map pictures the bot made. Public on purpose (Discord's servers fetch them); only files with the exact name this
/// bot gives them can be reached, and the name carries a hash of the content, so browsers and Discord may keep them.
async fn levelshot_file(State(app): State<Arc<App>>, Path(file): Path<String>) -> Response {
    if !crate::levelshot::valid_file_name(&file) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    match tokio::fs::read(crate::levelshot::dir(&app.cfg()).join(&file)).await {
        Ok(data) => (
            [(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "public, max-age=86400, immutable"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")],
            data,
        ).into_response(),
        Err(_) => text(StatusCode::NOT_FOUND, "not found"),
    }
}

const HLS_JS: &[u8] = include_bytes!("../assets/hls.min.js");

/// The player library is served by the bot itself, so viewers behind a school/work/ad-blocking network that blocks
/// public CDNs still get a working player (and nothing is fetched from a third party).
async fn hls_js() -> Response {
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=86400")], HLS_JS).into_response()
}

/// What is going on behind the player, in words a viewer understands (polled by the page).
async fn status(State(app): State<Arc<App>>, Path(key): Path<String>, Query(q): Query<StatusQuery>, headers: HeaderMap) -> Response {
    let Some(s) = app.streamer(&key) else { return text(StatusCode::NOT_FOUND, "not available") };
    if !authorized(&app, &headers) {
        return text(StatusCode::FORBIDDEN, "not available");
    }
    if let Some(id) = q.v.as_deref() {
        app.viewers.beat(&key, id, q.p.as_deref() == Some("1") && s.is_running());
    }
    let counts = app.viewers.counts(&key);
    let pl = s.hls_dir().join("live.m3u8");
    let raw = tokio::fs::read_to_string(&pl).await.ok();
    let age = tokio::fs::metadata(&pl).await.ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).map(|d| d.as_secs_f64());
    let delay = s.delay() as f64;
    let (ph, buffered) = if s.is_running() { phase(raw.as_deref(), delay, age) } else { (Phase::Starting, 0.0) };
    let (state, message) = match &ph {
        _ if !s.is_running() => ("stopped", "The broadcast is starting or was stopped - this page will update when it is live.".to_string()),
        Phase::Starting => ("starting", "The video source is starting (the game client has to load the map) - usually under a minute.".to_string()),
        Phase::Warming { wait } => ("warming", format!("Building the {}s anti-ghosting delay - the picture starts in about {} seconds.", delay as u64, wait.ceil() as u64)),
        Phase::Stalled => ("stalled", "The video source stopped updating; the bot is restarting it. Keep this page open.".to_string()),
        Phase::Live => ("live", if delay > 0.0 { format!("{}s delay", delay as u64) } else { "Live".to_string() }),
    };
    let body = serde_json::json!({"state": state, "message": message, "delay": delay, "buffered": buffered, "viewers": counts.watching, "open": counts.open}).to_string();
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// Marks a router that serves one server only (see `router_for`).
#[derive(Clone)]
struct OnlyServer(String);

/// On a server's own port, refuse the pages and files of every other server.
async fn only_gate(Extension(only): Extension<OnlyServer>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut parts = req.uri().path().trim_start_matches('/').split('/');
    if matches!(parts.next(), Some("watch" | "status" | "hls")) && parts.next().is_some_and(|k| k != only.0) {
        return text(StatusCode::NOT_FOUND, "not available");
    }
    next.run(req).await
}

/// The web player for ONE server, for that server's own port: same pages, same access key, but nothing of the other
/// servers' streams can be reached through it.
pub fn router_for(app: Arc<App>, key: &str) -> Router {
    Router::new()
        .route("/", get(index_only))
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/watch/{key}", get(watch_page))
        .route("/status/{key}", get(status))
        .route("/player/hls.min.js", get(hls_js))
        .route("/brand/{file}", get(brand_file))
        .route("/levelshot/{file}", get(levelshot_file))
        .route("/hls/{key}/{file}", get(hls_file))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(only_gate))
        .layer(Extension(OnlyServer(key.to_string())))
        .with_state(app)
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .route("/watch/{key}", get(watch_page))
        .route("/status/{key}", get(status))
        .route("/player/hls.min.js", get(hls_js))
        .route("/brand/{file}", get(brand_file))
        .route("/levelshot/{file}", get(levelshot_file))
        .route("/hls/{key}/{file}", get(hls_file))
        .fallback(not_found)
        .with_state(app)
}

pub async fn bind(host: &str, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let addr = if host.contains(':') { format!("[{}]:{}", host, port) } else { format!("{}:{}", host, port) };
    tokio::net::TcpListener::bind(&addr).await.map_err(|e| anyhow::anyhow!("cannot listen on {}: {}", addr, e))
}

pub async fn serve(listener: tokio::net::TcpListener, router: Router, mut stop: crate::util::StopRx) {
    let shutdown = async move {
        stop.sleep(std::time::Duration::from_secs(86_400 * 365)).await;
    };
    if let Err(e) = axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
        tracing::error!("web server stopped: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SpectatorConfig, WebConfig};
    use crate::logbuf::LogBuffer;
    use crate::spectator::{Platform, Streamer};
    use crate::util::Stop;

    const PLAYLIST_HEAD: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:10\n";

    async fn get(port: u16, path: &str, cookie: Option<&str>) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let ck = cookie.map(|c| format!("Cookie: {}\r\n", c)).unwrap_or_default();
        s.write_all(format!("GET {} HTTP/1.1\r\nHost: x\r\n{}Connection: close\r\n\r\n", path, ck).as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (code, head.to_string(), body.to_string())
    }

    #[tokio::test]
    async fn token_cookie_delay_and_segment_rules() {
        let work = std::env::temp_dir().join(format!("gtv-web-{}", crate::config::random_token(4)));
        let mut cfg = Config::default();
        cfg.web = Some(WebConfig { token: "sekret".into(), ..Default::default() });
        let app = App::new(work.join("config.json"), cfg, LogBuffer::new(), None);
        let sp = SpectatorConfig { work_dir: work.to_string_lossy().into_owned(), ..Default::default() };
        let streamer = Streamer::with_platform("m", "h", 1, &sp, 90, 10, None, Platform::Linux).unwrap();
        let hls = streamer.hls_dir();
        std::fs::create_dir_all(&hls).unwrap();
        let mut pl = PLAYLIST_HEAD.to_string();
        for i in 10..30 {
            pl.push_str(&format!("#EXTINF:2.000,\nseg_{:06}.ts\n", i));
            std::fs::write(hls.join(format!("seg_{:06}.ts", i)), b"x").unwrap();
        }
        std::fs::write(hls.join("live.m3u8"), pl).unwrap();
        app.streamers.lock().unwrap().insert("m".into(), streamer.clone());

        let stop = Stop::new();
        let l = bind("127.0.0.1", 0).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, router(app.clone()), stop.rx()));

        assert_eq!(get(port, "/healthz", None).await.2, "ok");
        assert_eq!(get(port, "/hls/m/live.m3u8", None).await.0, 403); // no cookie
        let (code, head, body) = get(port, "/watch/m?t=sekret", None).await;
        assert_eq!(code, 200);
        assert!(body.contains("[TFG]Aciddrop") && body.contains("10s delay"));
        assert!(head.to_lowercase().contains("set-cookie: gtv=sekret; path=/;"), "cookie must cover /hls and /status, not only /watch: {}", head);
        assert_eq!(get(port, "/watch/m?t=bad", None).await.0, 403);
        // the bare address: private without the key, otherwise straight to the one running stream
        let (code, _, body) = get(port, "/", None).await;
        assert_eq!(code, 403);
        assert!(body.contains("Open the link from Discord"));
        let (code, head, _) = get(port, "/?t=sekret", None).await;
        assert_eq!(code, 302);
        assert!(head.to_lowercase().contains("location: /watch/m") && head.to_lowercase().contains("set-cookie: gtv=sekret; path=/;"));
        assert_eq!(get(port, "/", Some("gtv=sekret")).await.0, 302);
        let (code, _, body) = get(port, "/nothing-here", None).await;
        assert_eq!(code, 404);
        assert!(body.contains("Nothing here"));
        assert_eq!(get(port, "/watch/nope?t=sekret", None).await.0, 404);
        let ck = Some("gtv=sekret");
        let (code, _, body) = get(port, "/hls/m/live.m3u8", ck).await;
        assert_eq!(code, 200);
        assert!(body.contains("seg_000024.ts") && !body.contains("seg_000025.ts"));
        assert_eq!(get(port, "/hls/m/seg_000024.ts", ck).await.0, 200);
        assert_eq!(get(port, "/hls/m/seg_000025.ts", ck).await.0, 404); // newer than the delay: refused
        assert_eq!(get(port, "/hls/m/..%2fconfig.json", ck).await.0, 404);
        assert_eq!(get(port, "/hls/m/seg_000024.ts", None).await.0, 403);
        // the player library is served by the bot itself (no third-party CDN needed), without a key
        let (code, head, body) = get(port, "/player/hls.min.js", None).await;
        assert_eq!(code, 200);
        assert!(head.to_lowercase().contains("application/javascript") && body.len() > 100_000);
        let (code, _, body) = get(port, "/watch/m?t=sekret", None).await;
        assert!(code == 200 && body.contains("/player/hls.min.js") && !body.contains("cdn.jsdelivr"));
        // viewers are told what is going on, but only with the key
        assert_eq!(get(port, "/status/m", None).await.0, 403);
        let (code, _, body) = get(port, "/status/m", ck).await;
        assert!(code == 200 && body.contains("\"state\""), "{}", body);
        // playlist not there yet -> 503 with Retry-After
        std::fs::remove_file(hls.join("live.m3u8")).unwrap();
        let (code, head, _) = get(port, "/hls/m/live.m3u8", ck).await;
        assert_eq!(code, 503);
        assert!(head.to_lowercase().contains("retry-after: 3"));
        // an open server (no token) lets anyone in
        let mut open = (*app.cfg()).clone();
        open.web = Some(WebConfig::default());
        app.set_cfg(open);
        assert_eq!(get(port, "/watch/m", None).await.0, 200);
        assert_eq!(get(port, "/", None).await.0, 302);
        // no broadcast running: a readable 200 page, not an error
        app.streamers.lock().unwrap().clear();
        let (code, _, body) = get(port, "/", None).await;
        assert_eq!(code, 200);
        assert!(body.contains("Nothing with video is being broadcast"));
        let (code, _, body) = get(port, "/watch/m", None).await;
        assert_eq!(code, 404);
        assert!(body.contains("/gtv start"));
        stop.stop();
        let _ = std::fs::remove_dir_all(work);
    }

    /// Manual end-to-end harness (not part of the normal test run): real Xvfb + a moving picture + the bot's real
    /// ffmpeg command + the real web server, so a real browser can be pointed at it.
    ///   GTV_HARNESS_PORT=18099 GTV_HARNESS_DELAY=10 cargo test --bin forge-gtv harness_ -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn harness_real_pipeline_for_a_browser() {
        use std::process::{Command, Stdio};
        let port: u16 = std::env::var("GTV_HARNESS_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(18099);
        let delay: u32 = std::env::var("GTV_HARNESS_DELAY").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
        let work = std::env::temp_dir().join("gtv-harness");
        let _ = std::fs::create_dir_all(&work);
        let mut cfg = Config::default();
        cfg.web = Some(WebConfig { token: "sekret".into(), port, ..Default::default() });
        let app = App::new(work.join("config.json"), cfg, LogBuffer::new(), None);
        let sp = SpectatorConfig { work_dir: work.to_string_lossy().into_owned(), width: 640, height: 360, fps: 15, video_kbps: 800, segment_seconds: 2, ..Default::default() };
        let streamer = Streamer::with_platform("m", "h", 1, &sp, 88, delay, None, Platform::Linux).unwrap();
        let hls = streamer.hls_dir();
        let _ = std::fs::remove_dir_all(&hls);
        std::fs::create_dir_all(&hls).unwrap();
        let mut kids = vec![];
        kids.push(Command::new("Xvfb").args([":88", "-screen", "0", "640x360x24", "-nolisten", "tcp"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        kids.push(Command::new("ffplay").args(["-loglevel", "quiet", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=15", "-fs"]).env("DISPLAY", ":88").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        let argv = crate::spectator::build_ffmpeg_argv(&sp, ":88", &hls.to_string_lossy(), Platform::Linux, crate::gpu::Encoder::Cpu).unwrap();
        kids.push(Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
        app.streamers.lock().unwrap().insert("m".into(), streamer.clone());
        streamer.mark_running_for_test();
        let stop = Stop::new();
        let l = bind("127.0.0.1", port).await.unwrap();
        tokio::spawn(serve(l, router(app.clone()), stop.rx()));
        eprintln!("HARNESS READY on {}", port);
        tokio::time::sleep(std::time::Duration::from_secs(150)).await;
        for mut k in kids {
            let _ = k.kill();
        }
    }

    #[test]
    fn escaping_and_cookie_parsing() {
        assert_eq!(html_escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#x27;");
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; gtv=tok; b=2"));
        assert_eq!(cookie_value(&h, "gtv"), "tok");
        assert_eq!(cookie_value(&h, "zzz"), "");
    }

    #[tokio::test]
    async fn clan_pictures_are_public_but_only_stored_ones_and_viewers_are_counted() {
        let work = std::env::temp_dir().join(format!("gtv-web-{}", crate::config::random_token(4)));
        let mut cfg = Config::default();
        cfg.data_dir = work.join("data").to_string_lossy().into_owned();
        cfg.web = Some(WebConfig { token: "sekret".into(), ..Default::default() });
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";
        let name = crate::brand::store(&crate::brand::dir(&cfg), png).unwrap();
        cfg.branding = crate::config::BrandingConfig { name: "The <Forge> & Co".into(), logo: name.clone(), icon: "https://x.example/i.png".into(), color: String::new() };
        let app = App::new(work.join("config.json"), cfg, LogBuffer::new(), None);
        let sp = SpectatorConfig { work_dir: work.to_string_lossy().into_owned(), ..Default::default() };
        let streamer = Streamer::with_platform("m", "h", 1, &sp, 90, 0, None, Platform::Linux).unwrap();
        app.streamers.lock().unwrap().insert("m".into(), streamer);
        let stop = Stop::new();
        let l = bind("127.0.0.1", 0).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, router(app.clone()), stop.rx()));

        // the logo: no key needed (Discord fetches it), right type, cacheable
        let (code, head, body) = get(port, &format!("/brand/{}", name), None).await;
        assert_eq!(code, 200);
        assert!(head.to_lowercase().contains("content-type: image/png") && head.to_lowercase().contains("nosniff") && body.starts_with("\u{fffd}PNG"));
        // nothing else in or out of that folder
        for bad in ["/brand/..%2Fconfig.json", "/brand/config.json", "/brand/0123456789abcdef.png", "/brand/0123456789abcdef.svg", "/brand/"] {
            assert_ne!(get(port, bad, None).await.0, 200, "{}", bad);
        }
        // the player page: the clan name is escaped, the logo and icon are used, the ownership credit stays
        let (code, _, page) = get(port, "/watch/m?t=sekret", None).await;
        assert_eq!(code, 200);
        assert!(page.contains("The &lt;Forge&gt; &amp; Co") && !page.contains("<Forge>"), "name must be escaped");
        assert!(page.contains(&format!("src=\"/brand/{}\"", name)) && page.contains("rel=\"icon\" href=\"https://x.example/i.png\""));
        assert!(page.contains("[TFG]Aciddrop") && page.contains("GNU GPL"));
        // viewers: counted only with the key, ids must look right, repeat reports are one viewer
        let ck = Some("gtv=sekret");
        assert_eq!(get(port, "/status/m?v=aaaaaaaa1&p=1", None).await.0, 403);
        let (_, _, j) = get(port, "/status/m?v=aaaaaaaa1&p=0", ck).await;
        assert!(j.contains("\"open\":1"), "{}", j);
        let (_, _, j) = get(port, "/status/m?v=aaaaaaaa1&p=0", ck).await;
        assert!(j.contains("\"open\":1"), "{}", j);
        let (_, _, j) = get(port, "/status/m?v=bbbbbbbb2&p=0", ck).await;
        assert!(j.contains("\"open\":2") && j.contains("\"viewers\":0"), "{}", j);
        let (_, _, j) = get(port, "/status/m?v=bad&p=1", ck).await;
        assert!(j.contains("\"open\":2"), "a malformed id is ignored: {}", j);
        let (_, _, j) = get(port, "/status/m", ck).await;
        assert!(j.contains("\"open\":2"), "no id: only reads the count: {}", j);
        assert_eq!(app.viewers.counts("m").open, 2);
        stop.stop();
    }

    #[tokio::test]
    async fn map_pictures_are_public_but_only_the_files_the_bot_made() {
        use crate::levelshot::tests::{sample, tmp};
        let work = tmp("web");
        let mut cfg = Config::default();
        cfg.data_dir = work.join("data").to_string_lossy().into_owned();
        // a token on the player must not hide the pictures: Discord's servers fetch them without a key
        cfg.web = Some(WebConfig { token: "sekret".into(), ..Default::default() });
        let src = work.join("q3ut4");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ut4_turnpike.jpg"), sample(256, 192, "jpg")).unwrap();
        let name = crate::levelshot::Levelshots::new(crate::levelshot::dir(&cfg)).resolve(&[src], "ut4_turnpike", 320, crate::levelshot::Fit::Crop).unwrap();
        std::fs::write(work.join("data").join("secret.jpg"), b"not for you").unwrap();
        let app = App::new(work.join("config.json"), cfg, LogBuffer::new(), None);
        let stop = Stop::new();
        let l = bind("127.0.0.1", 0).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(serve(l, router(app.clone()), stop.rx()));
        // and on a server's own port as well
        let l2 = bind("127.0.0.1", 0).await.unwrap();
        let port2 = l2.local_addr().unwrap().port();
        tokio::spawn(serve(l2, router_for(app.clone(), "m"), stop.rx()));

        for p in [port, port2] {
            let (code, head, body) = get(p, &format!("/levelshot/{}", name), None).await;
            let head = head.to_lowercase();
            assert_eq!(code, 200, "port {}", p);
            assert!(head.contains("content-type: image/jpeg") && head.contains("nosniff") && head.contains("immutable"), "{}", head);
            assert!(body.starts_with('\u{fffd}'), "a JPEG");
            for bad in ["/levelshot/..%2Fsecret.jpg", "/levelshot/secret.jpg", "/levelshot/ut4_turnpike-320c-00000000.jpg", "/levelshot/ut4_turnpike.jpg", "/levelshot/"] {
                assert_ne!(get(p, bad, None).await.0, 200, "{} on {}", bad, p);
            }
        }
        let _ = std::fs::remove_dir_all(work);
    }
}
