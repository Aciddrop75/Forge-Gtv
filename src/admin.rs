// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! The admin panel: a small password-protected web UI + JSON API for installing, configuring and
//! running the bot. Listens on 127.0.0.1 by default.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Json;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::App;
use crate::config::{Config, FsConfig, LOCAL_HOST, REMOTE_HOST, REMOTE_MIN_PASSWORD, ServerConfig, hash_password, ip_allowed, parse_cidr, random_token, valid_key, verify_password};
use crate::doctor;
use crate::spectator::Platform;
use crate::version::{OWNER, RELEASE_DATE, VERSION};
use crate::web::cookie_value;

const UI: &str = include_str!("../assets/admin.html");
pub const MASK: &str = "********";
const SESSION_HOURS: u64 = 12;

pub struct AdminState {
    pub app: Arc<App>,
    sessions: Mutex<HashMap<String, Instant>>,
    fails: Mutex<Vec<Instant>>,
}

type St = State<Arc<AdminState>>;
type Api = Result<Json<Value>, ApiError>;

pub struct ApiError(StatusCode, String, Vec<String>);

impl ApiError {
    fn bad(msg: impl Into<String>) -> ApiError { ApiError(StatusCode::BAD_REQUEST, msg.into(), vec![]) }
    fn not_found(msg: impl Into<String>) -> ApiError { ApiError(StatusCode::NOT_FOUND, msg.into(), vec![]) }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"ok": false, "error": self.1, "problems": self.2}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self { ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e), vec![]) }
}

fn ok(v: Value) -> Api {
    let mut v = v;
    if let Some(o) = v.as_object_mut() {
        o.entry("ok").or_insert(Value::Bool(true));
    }
    Ok(Json(v))
}

// ------------------------------------------------------------------------------------------ secrets

fn mask_in_place(c: &mut Config) {
    let m = |s: &mut String| if !s.is_empty() { *s = MASK.into() };
    m(&mut c.discord.token);
    if let Some(w) = c.web.as_mut() { m(&mut w.token); }
    for f in c.filesystems.values_mut() { m(&mut f.password); }
    for s in c.servers.values_mut() {
        m(&mut s.rcon_password);
        if let Some(sp) = s.spectator.as_mut() { m(&mut sp.password); m(&mut sp.rtmp_url); }
    }
    c.admin.password_hash.clear();
}

/// Replace every MASK placeholder in `new` with the value `old` had; keep the admin password hash.
fn unmask(new: &mut Config, old: &Config) {
    let fix = |n: &mut String, o: Option<&String>| if n == MASK { *n = o.cloned().unwrap_or_default() };
    fix(&mut new.discord.token, Some(&old.discord.token));
    if let Some(w) = new.web.as_mut() { fix(&mut w.token, old.web.as_ref().map(|x| &x.token)); }
    for (k, f) in new.filesystems.iter_mut() { fix(&mut f.password, old.filesystems.get(k).map(|x| &x.password)); }
    for (k, s) in new.servers.iter_mut() {
        let o = old.servers.get(k);
        fix(&mut s.rcon_password, o.map(|x| &x.rcon_password));
        if let Some(sp) = s.spectator.as_mut() {
            let osp = o.and_then(|x| x.spectator.as_ref());
            fix(&mut sp.password, osp.map(|x| &x.password));
            fix(&mut sp.rtmp_url, osp.map(|x| &x.rtmp_url));
        }
    }
    new.admin.password_hash = old.admin.password_hash.clone();
    // who can reach the panel is changed only through the "Access" card (it asks for the password again)
    new.admin.host = old.admin.host.clone();
    new.admin.allowed_ips = old.admin.allowed_ips.clone();
}

fn masked_json(c: &Config) -> Value {
    let mut m = c.clone();
    mask_in_place(&mut m);
    serde_json::to_value(&m).unwrap_or(Value::Null)
}

// ------------------------------------------------------------------------------------------ auth

fn peer_is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Who may reach the panel at all. This computer always may. Everyone else only when remote access is switched on, and
/// then only from the allowed addresses if a list is set. Checked on every request against the current settings, so
/// turning remote access off cuts other computers off at once.
async fn gate(State(st): St, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let cfg = st.app.cfg();
    let allowed = peer.ip().is_loopback() || (cfg.admin.remote() && (cfg.admin.allowed_ips.is_empty() || ip_allowed(&cfg.admin.allowed_ips, peer.ip())));
    if !allowed {
        return (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "The admin panel is not available from this address. Open it on the computer the bot runs on, or ask the owner to allow remote access.").into_response();
    }
    next.run(req).await
}

async fn require_login(State(st): St, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let open = !path.starts_with("/api/") || ["/api/login", "/api/setup", "/api/hello"].contains(&path.as_str());
    if !open {
        let token = cookie_value(req.headers(), "gtv_admin");
        let valid = {
            let mut s = st.sessions.lock().unwrap();
            s.retain(|_, t| t.elapsed() < Duration::from_secs(SESSION_HOURS * 3600));
            s.contains_key(&token)
        };
        if !valid {
            return ApiError(StatusCode::UNAUTHORIZED, "Please sign in.".into(), vec![]).into_response();
        }
        // SameSite=Strict already blocks cross-site posts; the header makes it explicit
        if req.method() != axum::http::Method::GET && req.headers().get("x-requested-with").and_then(|v| v.to_str().ok()) != Some("gtv-admin") {
            return ApiError(StatusCode::FORBIDDEN, "Missing X-Requested-With header.".into(), vec![]).into_response();
        }
    }
    next.run(req).await
}

fn new_session(st: &AdminState, headers: &HeaderMap) -> HeaderValue {
    let token = random_token(24);
    st.sessions.lock().unwrap().insert(token.clone(), Instant::now());
    // behind an HTTPS reverse proxy the cookie must never travel over plain HTTP
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("https"));
    HeaderValue::from_str(&format!("gtv_admin={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}", token, SESSION_HOURS * 3600, if secure { "; Secure" } else { "" })).unwrap()
}

fn with_cookie(mut resp: Response, cookie: HeaderValue) -> Response {
    resp.headers_mut().append(header::SET_COOKIE, cookie);
    resp
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

async fn hello(State(st): St) -> Api {
    let configured = !st.app.cfg().admin.password_hash.is_empty();
    ok(json!({"version": VERSION, "owner": OWNER, "released": RELEASE_DATE, "configured": configured}))
}

async fn login(State(st): St, headers: HeaderMap, Json(b): Json<PasswordBody>) -> Result<Response, ApiError> {
    {
        let mut f = st.fails.lock().unwrap();
        f.retain(|t| t.elapsed() < Duration::from_secs(60));
        if f.len() >= 8 {
            return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, "Too many wrong passwords - wait a minute.".into(), vec![]));
        }
    }
    let hash = st.app.cfg().admin.password_hash.clone();
    if hash.is_empty() {
        return Err(ApiError::bad("No admin password is set yet."));
    }
    if !verify_password(&b.password, &hash) {
        st.fails.lock().unwrap().push(Instant::now());
        tokio::time::sleep(Duration::from_millis(600)).await;
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong password.".into(), vec![]));
    }
    Ok(with_cookie(Json(json!({"ok": true})).into_response(), new_session(&st, &headers)))
}

/// First run without the installer: set the admin password, from this computer only.
/// `Host:` must name this computer - stops a web page from reaching the first-run form by DNS rebinding.
fn host_is_local(headers: &HeaderMap) -> bool {
    let h = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let name = if let Some(rest) = h.strip_prefix('[') { rest.split(']').next().unwrap_or("") } else { h.split(':').next().unwrap_or("") };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

async fn setup(State(st): St, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(b): Json<PasswordBody>) -> Result<Response, ApiError> {
    if !host_is_local(&headers) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Open the panel as http://127.0.0.1:PORT or http://localhost:PORT to set the first password.".into(), vec![]));
    }
    if !st.app.cfg().admin.password_hash.is_empty() {
        return Err(ApiError(StatusCode::FORBIDDEN, "The admin password is already set.".into(), vec![]));
    }
    if !peer_is_loopback(&peer) {
        return Err(ApiError(StatusCode::FORBIDDEN, "The first password can only be set from the machine the bot runs on (open the panel at http://127.0.0.1:PORT there).".into(), vec![]));
    }
    if b.password.chars().count() < 8 {
        return Err(ApiError::bad("Use at least 8 characters."));
    }
    let mut cfg = (*st.app.cfg()).clone();
    cfg.admin.password_hash = hash_password(&b.password);
    cfg.save(&st.app.config_path)?;
    st.app.set_cfg(cfg);
    Ok(with_cookie(Json(json!({"ok": true})).into_response(), new_session(&st, &headers)))
}

async fn logout(State(st): St, headers: HeaderMap) -> Api {
    st.sessions.lock().unwrap().remove(&cookie_value(&headers, "gtv_admin"));
    ok(json!({}))
}

#[derive(Deserialize)]
struct ChangePw {
    current: String,
    new: String,
}

async fn change_password(State(st): St, Json(b): Json<ChangePw>) -> Api {
    let mut cfg = (*st.app.cfg()).clone();
    if !verify_password(&b.current, &cfg.admin.password_hash) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "The current password is wrong.".into(), vec![]));
    }
    let min = if cfg.admin.remote() { REMOTE_MIN_PASSWORD } else { 8 };
    if b.new.chars().count() < min {
        return Err(ApiError::bad(format!("Use at least {} characters{}.", min, if min > 8 { " (the panel is open to other computers)" } else { "" })));
    }
    cfg.admin.password_hash = hash_password(&b.new);
    cfg.save(&st.app.config_path)?;
    st.app.set_cfg(cfg);
    st.sessions.lock().unwrap().clear();
    ok(json!({"signed_out": true}))
}

// ------------------------------------------------------------------------------------------ state + config

fn checklist(app: &App) -> Vec<Value> {
    let cfg = app.cfg();
    let discord = app.discord.read().unwrap().clone();
    vec![
        json!({"id": "token", "title": "Add your Discord bot token", "done": !cfg.discord_token().is_empty()}),
        json!({"id": "discord", "title": "Connect the bot to Discord", "done": matches!(discord, crate::app::DiscordStatus::Ready(_))}),
        json!({"id": "server", "title": "Add a game server to watch", "done": !cfg.servers.is_empty()}),
        json!({"id": "invite", "title": "Invite the bot to your Discord server", "done": false, "manual": true}),
    ]
}

async fn state(State(st): St) -> Api {
    let app = &st.app;
    let cfg = app.cfg();
    ok(json!({
        "version": VERSION, "owner": OWNER, "released": RELEASE_DATE,
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "config_path": app.config_path.to_string_lossy(),
        "runtime": app.runtime_state(),
        "problems": cfg.problems(),
        "checklist": checklist(app),
        "web_token_set": !cfg.web_token().is_empty(),
        "access": access_json(&cfg),
        "branding": branding_json(&cfg),
        "servers": cfg.servers.iter().map(|(k, s)| json!({
            "key": k, "name": s.display_name(), "host": s.host, "port": s.port,
            "video": s.spectator.is_some(), "log": !s.log_path.is_empty(),
            "web_port": s.spectator.as_ref().map(|x| x.web_port).unwrap_or(0),
        })).collect::<Vec<_>>(),
    }))
}

async fn get_config(State(st): St) -> Api {
    ok(json!({"config": masked_json(&st.app.cfg())}))
}

fn commit(st: &AdminState, mut new: Config) -> Api {
    new.fix_keys();
    let problems = new.problems();
    new.save(&st.app.config_path)?;
    st.app.set_cfg(new);
    ok(json!({"saved": true, "problems": problems, "needs_restart": st.app.needs_restart()}))
}

async fn put_config(State(st): St, Json(v): Json<Value>) -> Api {
    let mut new: Config = serde_json::from_value(v.get("config").cloned().unwrap_or(v)).map_err(|e| ApiError::bad(format!("The configuration is not valid: {}", e)))?;
    unmask(&mut new, &st.app.cfg());
    if let Some(w) = new.web.as_mut() {
        w.public_url = crate::config::fix_public_url(&w.public_url).map_err(|e| ApiError::bad(format!("Public address: {}", e)))?;
    }
    commit(&st, new)
}

async fn put_server(State(st): St, Path(key): Path<String>, Json(v): Json<Value>) -> Api {
    if !valid_key(&key) {
        return Err(ApiError::bad("The server key may only contain letters, digits, '-' and '_' (max 32)."));
    }
    let mut sc: ServerConfig = serde_json::from_value(v).map_err(|e| ApiError::bad(format!("Invalid server settings: {}", e)))?;
    let old = (*st.app.cfg()).clone();
    let mut new = old.clone();
    new.servers.insert(key.clone(), sc.clone());
    unmask(&mut new, &old);
    sc = new.servers[&key].clone();
    if sc.host.trim().is_empty() {
        return Err(ApiError::bad("The server needs a host (IP address or name)."));
    }
    commit(&st, new)
}

async fn delete_server(State(st): St, Path(key): Path<String>) -> Api {
    if st.app.watches.lock().unwrap().values().any(|w| w.cfg.key == key) {
        return Err(ApiError::bad("A broadcast is running for this server - stop it first."));
    }
    let mut new = (*st.app.cfg()).clone();
    if new.servers.shift_remove(&key).is_none() {
        return Err(ApiError::not_found("No such server."));
    }
    commit(&st, new)
}

async fn put_filesystem(State(st): St, Path(name): Path<String>, Json(v): Json<Value>) -> Api {
    if !valid_key(&name) {
        return Err(ApiError::bad("The name may only contain letters, digits, '-' and '_'."));
    }
    let fs: FsConfig = serde_json::from_value(v).map_err(|e| ApiError::bad(format!("Invalid filesystem settings: {}", e)))?;
    let old = (*st.app.cfg()).clone();
    let mut new = old.clone();
    new.filesystems.insert(name, fs);
    unmask(&mut new, &old);
    commit(&st, new)
}

async fn delete_filesystem(State(st): St, Path(name): Path<String>) -> Api {
    let mut new = (*st.app.cfg()).clone();
    if new.filesystems.shift_remove(&name).is_none() {
        return Err(ApiError::not_found("No such filesystem."));
    }
    commit(&st, new)
}

// ------------------------------------------------------------------------------------------ tests + checks

fn checks_json(checks: &[doctor::Check]) -> Value {
    json!({"checks": checks, "problems": doctor::problems(checks)})
}

/// Test a game server. Body = the (possibly unsaved) server settings; secrets may be masked.
async fn test_server(State(st): St, Path(key): Path<String>, body: Option<Json<Value>>) -> Api {
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    let cfg = (*st.app.cfg()).clone();
    let sc: ServerConfig = if v.as_object().is_some_and(|o| !o.is_empty()) {
        serde_json::from_value(v).map_err(|e| ApiError::bad(format!("Invalid server settings: {}", e)))?
    } else {
        cfg.servers.get(&key).cloned().ok_or_else(|| ApiError::not_found("No such server."))?
    };
    ok(checks_json(&doctor::check_server(&cfg, &sc).await))
}

/// Look for GPUs and test-encode with each encoder; the body may override ffmpeg_binary/width/height/fps/vaapi_device.
async fn detect_gpu(State(st): St, body: Option<Json<Value>>) -> Api {
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    let mut sp = crate::config::SpectatorConfig::default();
    if let Some(s) = v.get("ffmpeg_binary").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) { sp.ffmpeg_binary = s.trim().into(); }
    if let Some(s) = v.get("vaapi_device").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) { sp.vaapi_device = s.trim().into(); }
    for (name, slot) in [("width", &mut sp.width), ("height", &mut sp.height), ("fps", &mut sp.fps), ("video_kbps", &mut sp.video_kbps)] {
        if let Some(n) = v.get(name).and_then(Value::as_u64) { *slot = n.min(8000) as u32; }
    }
    let _ = &st;
    let report = crate::gpu::detect(&sp, Platform::current()).await;
    ok(json!({"report": report, "text": crate::gpu::render(&report)}))
}

async fn test_stream(State(st): St, Path(key): Path<String>) -> Api {
    let cfg = (*st.app.cfg()).clone();
    let sc = cfg.servers.get(&key).cloned().ok_or_else(|| ApiError::not_found("No such server."))?;
    ok(checks_json(&doctor::check_stream(&cfg, &key, &sc, Platform::current()).await))
}

#[derive(Deserialize)]
struct FsTest {
    #[serde(flatten)]
    fs: FsConfig,
    /// A path to look at on the remote side (optional).
    path: Option<String>,
    /// A file (e.g. games.log) that must exist on the remote side, checked after the connection works.
    file: Option<String>,
    /// Record an unknown host key the first time (the user confirmed the fingerprint).
    learn_host_key: Option<bool>,
    name: Option<String>,
}

/// The connection to use: a saved one (`name` alone), or what was typed (a saved password stays hidden).
fn effective_fs(st: &AdminState, mut fs: FsConfig, name: Option<&str>, learn_host_key: Option<bool>) -> FsConfig {
    let old = st.app.cfg();
    if let Some(saved) = name.and_then(|n| old.filesystems.get(n)) {
        if fs.host.is_empty() && fs.user.is_empty() {
            fs = saved.clone();
        } else if fs.password == MASK {
            fs.password = saved.password.clone();
        }
    }
    if fs.password == MASK {
        fs.password.clear();
    }
    if learn_host_key == Some(true) {
        fs.trust_unknown_hosts = true;
    }
    fs
}

async fn test_filesystem(State(st): St, Json(b): Json<FsTest>) -> Api {
    let fs = effective_fs(&st, b.fs, b.name.as_deref(), b.learn_host_key);
    let mut map = indexmap::IndexMap::new();
    map.insert("test".to_string(), fs);
    let fss = crate::fs::FileSystems::new(&map).map_err(|e| ApiError::bad(e.to_string()))?;
    let spec = format!("test:{}", b.path.filter(|p| !p.is_empty()).unwrap_or_else(|| "/".into()));
    let (sftp, path) = fss.resolve(&spec).map_err(|e| ApiError::bad(e.to_string()))?;
    match tokio::time::timeout(Duration::from_secs(30), sftp.size(&path)).await {
        Ok(Ok(_)) => {
            let mut out = json!({"connected": true, "label": sftp.label(), "detail": "logged in over SSH/SFTP"});
            if let Some(file) = b.file.filter(|f| !f.trim().is_empty()) {
                let (_, fpath) = fss.resolve(&format!("test:{}", file.trim())).map_err(|e| ApiError::bad(e.to_string()))?;
                match tokio::time::timeout(Duration::from_secs(30), sftp.size(&fpath)).await {
                    Ok(Ok(n)) => {
                        out["file_ok"] = json!(true);
                        out["file_detail"] = json!(format!("{} found ({} bytes)", fpath, n));
                    }
                    Ok(Err(e)) => {
                        out["file_ok"] = json!(false);
                        out["file_error"] = json!(format!("Logged in, but {} cannot be read: {:#}. Check the path and spelling; the file must exist (the game server writes it after `g_log` is set) and this login must be allowed to read it.", fpath, e));
                    }
                    Err(_) => {
                        out["file_ok"] = json!(false);
                        out["file_error"] = json!("Logged in, but looking for the file timed out.");
                    }
                }
            }
            ok(out)
        }
        Ok(Err(e)) => {
            let msg = format!("{:#}", e);
            let untrusted = msg.contains("is not trusted");
            ok(json!({"connected": false, "error": msg, "needs_host_key_approval": untrusted}))
        }
        Err(_) => ok(json!({"connected": false, "error": "timed out connecting - is the host name/port right and SSH reachable from this machine?"})),
    }
}

#[derive(Deserialize)]
struct BrowseBody {
    #[serde(flatten)]
    fs: FsConfig,
    /// Browse this computer instead of an SSH connection.
    local: Option<bool>,
    name: Option<String>,
    path: Option<String>,
    learn_host_key: Option<bool>,
}

/// Folder listing for the file picker (names only). Works on this machine or over a saved/typed SSH login.
async fn browse(State(st): St, Json(b): Json<BrowseBody>) -> Api {
    let path = b.path.unwrap_or_default();
    let local = b.local.unwrap_or(false);
    let listing = if local {
        crate::fs::Fs::list_dir(&crate::fs::LocalFs, &path).await
    } else {
        let fs = effective_fs(&st, b.fs, b.name.as_deref(), b.learn_host_key);
        let mut map = indexmap::IndexMap::new();
        map.insert("browse".to_string(), fs);
        let fss = crate::fs::FileSystems::new(&map).map_err(|e| ApiError::bad(e.to_string()))?;
        let sftp = fss.get("browse").map_err(|e| ApiError::bad(e.to_string()))?;
        match tokio::time::timeout(Duration::from_secs(30), crate::fs::Fs::list_dir(&*sftp, &path)).await {
            Ok(r) => r,
            Err(_) => return ok(json!({"listed": false, "error": "timed out - is the host name/port right and SSH reachable from this machine?"})),
        }
    };
    match listing {
        Ok(l) => ok(json!({"listed": true, "path": l.path, "parent": l.parent, "entries": l.entries, "truncated": l.truncated})),
        Err(e) => {
            let msg = format!("{:#}", e);
            let untrusted = msg.contains("is not trusted");
            // a typo in a local path: offer the folder that was probably meant
            let suggest = if local { crate::fs::closest_existing(&path) } else { None };
            ok(json!({"listed": false, "error": msg, "needs_host_key_approval": untrusted, "suggest": suggest}))
        }
    }
}

#[derive(Deserialize)]
struct TokenBody {
    token: Option<String>,
}

async fn discord_info(State(st): St, Json(b): Json<TokenBody>) -> Api {
    let token = match b.token.filter(|t| !t.is_empty() && t != MASK) {
        Some(t) => t,
        None => st.app.cfg().discord_token(),
    };
    if token.is_empty() {
        return Err(ApiError::bad("Enter the bot token first."));
    }
    match crate::discord::check_token(&token).await {
        Ok((name, id)) => ok(json!({"valid": true, "name": name, "app_id": id, "invite_url": crate::discord::invite_url(&id)})),
        Err(e) => ok(json!({"valid": false, "error": e})),
    }
}

/// The roles of every Discord server the bot is in, for choosing the server-admin role (`@everyone` and bot/integration
/// roles are left out: they cannot be given to people).
async fn discord_roles(State(st): St) -> Api {
    let Some(http) = st.app.discord_http() else {
        return Err(ApiError::bad("The bot is not connected to Discord right now - save the token and connect (Setup tab) first, or type the role id by hand."));
    };
    let guilds = http.get_guilds(None, Some(100)).await.map_err(|e| ApiError::bad(format!("Discord would not list the servers: {}", e)))?;
    let mut out = Vec::new();
    for g in guilds {
        let Ok(mut roles) = http.get_guild_roles(g.id).await else { continue };
        roles.retain(|r| r.id.get() != g.id.get() && !r.managed);
        roles.sort_by(|a, b| b.position.cmp(&a.position));
        out.push(json!({
            "id": g.id.get().to_string(), "name": g.name,
            "roles": roles.iter().map(|r| json!({"id": r.id.get().to_string(), "name": r.name})).collect::<Vec<_>>(),
        }));
    }
    ok(json!({"guilds": out}))
}

#[derive(Deserialize)]
struct PersonQuery {
    id: Option<String>,
    q: Option<String>,
}

fn person_json(u: &serenity::model::user::User) -> Value {
    json!({"id": u.id.get().to_string(), "name": u.name, "display": u.global_name.clone().unwrap_or_else(|| u.name.clone()), "bot": u.bot})
}

/// Look up one Discord account by its user id, so the panel can show a name next to a number (and tell a typo from a person).
/// Needs no special permission: any bot can fetch a user by id.
async fn discord_user(State(st): St, Query(q): Query<PersonQuery>) -> Api {
    let id = q.id.unwrap_or_default().trim().parse::<u64>().ok().filter(|n| *n != 0)
        .ok_or_else(|| ApiError::bad("A Discord user id is a long number (Discord with Developer Mode on: right-click the person, Copy User ID)."))?;
    let Some(http) = st.app.discord_http() else {
        return Err(ApiError::bad("The bot is not connected to Discord right now, so the name cannot be looked up. The id can still be added."));
    };
    let u = http.get_user(serenity::model::id::UserId::new(id)).await.map_err(|_| ApiError::bad("Discord does not know a person with that id - check the number."))?;
    ok(person_json(&u))
}

/// Find people by (part of) a name in the Discord servers the bot is in, for choosing who may use the rcon commands. Discord only
/// answers this when the "Server Members Intent" is switched on for the bot (Developer Portal -> Bot), so a refusal is explained.
async fn discord_members(State(st): St, Query(q): Query<PersonQuery>) -> Api {
    let query = q.q.unwrap_or_default().trim().chars().take(32).collect::<String>();
    if query.is_empty() {
        return Err(ApiError::bad("Type part of a name to search for."));
    }
    let Some(http) = st.app.discord_http() else {
        return Err(ApiError::bad("The bot is not connected to Discord right now - save the token and connect (Setup tab) first, or add people by their user id."));
    };
    let guilds = http.get_guilds(None, Some(100)).await.map_err(|e| ApiError::bad(format!("Discord would not list the servers: {}", e)))?;
    let mut out: Vec<Value> = Vec::new();
    let mut refused = false;
    for g in guilds {
        match http.search_guild_members(g.id, &query, Some(15)).await {
            Ok(members) => {
                for m in members.into_iter().filter(|m| !m.user.bot) {
                    let mut v = person_json(&m.user);
                    v["guild"] = json!(g.name);
                    if !out.iter().any(|o| o["id"] == v["id"]) {
                        out.push(v);
                    }
                }
            }
            Err(_) => refused = true,
        }
    }
    if out.is_empty() && refused {
        return Err(ApiError::bad("Discord refused the name search. Switch on the \"Server Members Intent\" for the bot (discord.com/developers/applications -> your application -> Bot -> Privileged Gateway Intents), then try again - or add the person by their user id."));
    }
    ok(json!({"people": out}))
}

async fn discord_reconnect(State(st): St) -> Api {
    crate::discord::connect(&st.app).await;
    ok(json!({"discord": *st.app.discord.read().unwrap()}))
}

// ------------------------------------------------------------------------------------------ control

#[derive(Deserialize)]
struct StartBody {
    channel_id: String,
    server: String,
    delay: Option<u32>,
    stream: Option<bool>,
}

fn channel(s: &str) -> Result<u64, ApiError> {
    s.trim().parse::<u64>().map_err(|_| ApiError::bad("The channel id must be a number (Discord: right-click the channel > Copy Channel ID; turn on Developer Mode in Settings > Advanced)."))
}

async fn broadcast_start(State(st): St, Json(b): Json<StartBody>) -> Api {
    let ch = channel(&b.channel_id)?;
    match st.app.start_watch(ch, &b.server, b.delay.unwrap_or(0).min(180), b.stream.unwrap_or(false)).await {
        Ok(msg) => ok(json!({"message": msg})),
        Err(e) => Err(ApiError::bad(e)),
    }
}

#[derive(Deserialize)]
struct ChannelBody {
    channel_id: String,
    /// Which server's broadcast; needed when the channel shows more than one.
    server: Option<String>,
}

async fn broadcast_stop(State(st): St, Json(b): Json<ChannelBody>) -> Api {
    match st.app.stop_watch(channel(&b.channel_id)?, b.server.as_deref()).await {
        Ok(msg) => ok(json!({"message": msg})),
        Err(e) => Err(ApiError::bad(e)),
    }
}

#[derive(Deserialize)]
struct LogQuery {
    n: Option<usize>,
}

async fn logs(State(st): St, Query(q): Query<LogQuery>) -> Api {
    ok(json!({"lines": st.app.logs.tail(q.n.unwrap_or(300).min(1000))}))
}

#[derive(Deserialize)]
struct ImportBody {
    text: String,
}

/// Bring in a config.json from the Python version (pasted text). Keeps this install's admin settings.
async fn import_config(State(st): St, Json(b): Json<ImportBody>) -> Api {
    let imported = Config::parse(&b.text).map_err(|e| ApiError::bad(format!("{:#}", e)))?;
    let old = (*st.app.cfg()).clone();
    let mut new = old.clone();
    if !imported.discord.token.is_empty() { new.discord.token = imported.discord.token.clone(); }
    if !imported.discord.restrict_to_user_ids.is_empty() { new.discord.restrict_to_user_ids = imported.discord.restrict_to_user_ids.clone(); }
    if imported.web.is_some() { new.web = imported.web.clone(); }
    for (k, v) in imported.filesystems { new.filesystems.insert(k, v); }
    let count = imported.servers.len();
    for (k, v) in imported.servers { new.servers.insert(k, v); }
    let r = commit(&st, new)?;
    let mut v = r.0;
    v["imported_servers"] = json!(count);
    Ok(Json(v))
}

async fn restart(State(st): St) -> Api {
    let app = st.app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        app.stop_all().await;
        restart_process();
    });
    ok(json!({"restarting": true, "under_service": crate::util::under_service()}))
}

/// Exit code the supervisors (systemd / the Windows supervisor) treat as "start me again".
pub const EXIT_RESTART: i32 = 75;

pub fn restart_process() -> ! {
    if crate::util::under_service() {
        std::process::exit(EXIT_RESTART);
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(std::env::args().skip(1)).spawn();
    }
    std::process::exit(0);
}

// ------------------------------------------------------------------------------------------ remote access

/// Ask for the password again before a change that matters; wrong guesses count towards the same limit as sign-in.
async fn reauth(st: &AdminState, password: &str) -> Result<(), ApiError> {
    {
        let mut f = st.fails.lock().unwrap();
        f.retain(|t| t.elapsed() < Duration::from_secs(60));
        if f.len() >= 8 {
            return Err(ApiError(StatusCode::TOO_MANY_REQUESTS, "Too many wrong passwords - wait a minute.".into(), vec![]));
        }
    }
    if !verify_password(password, &st.app.cfg().admin.password_hash) {
        st.fails.lock().unwrap().push(Instant::now());
        tokio::time::sleep(Duration::from_millis(600)).await;
        return Err(ApiError(StatusCode::UNAUTHORIZED, "The password is wrong.".into(), vec![]));
    }
    Ok(())
}

#[derive(Deserialize)]
struct AccessBody {
    /// true = other computers may open the panel; false = only this computer.
    remote: bool,
    /// The current password (asked again).
    password: String,
    /// Required when switching remote access ON: a new password of at least 12 characters.
    new_password: Option<String>,
    /// Only these addresses / ranges may open the panel remotely (empty = any address that knows the password).
    allowed_ips: Option<Vec<String>>,
}

async fn set_access(State(st): St, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(b): Json<AccessBody>) -> Result<Response, ApiError> {
    reauth(&st, &b.password).await?;
    let mut cfg = (*st.app.cfg()).clone();
    let was_remote = cfg.admin.remote();
    let mut new_session_needed = false;
    if let Some(list) = &b.allowed_ips {
        let list: Vec<String> = list.iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect();
        if let Some(bad) = list.iter().find(|e| parse_cidr(e).is_none()) {
            return Err(ApiError::bad(format!("'{}' is not an address like 203.0.113.7 or a range like 10.0.0.0/8.", bad)));
        }
        if b.remote && !list.is_empty() && !peer.ip().is_loopback() && !ip_allowed(&list, peer.ip()) {
            return Err(ApiError::bad(format!("Your own address ({}) is not in that list - you would lock yourself out. Add it, or leave the list empty.", peer.ip())));
        }
        cfg.admin.allowed_ips = list;
    }
    if b.remote {
        if !was_remote {
            let np = b.new_password.clone().unwrap_or_default();
            if np.chars().count() < REMOTE_MIN_PASSWORD {
                return Err(ApiError::bad(format!("Remote access needs a new password of at least {} characters - a long phrase of random words is best.", REMOTE_MIN_PASSWORD)));
            }
            cfg.admin.password_hash = hash_password(&np);
            new_session_needed = true;
        } else if let Some(np) = b.new_password.as_deref().filter(|p| !p.is_empty()) {
            if np.chars().count() < REMOTE_MIN_PASSWORD {
                return Err(ApiError::bad(format!("Use at least {} characters.", REMOTE_MIN_PASSWORD)));
            }
            cfg.admin.password_hash = hash_password(np);
            new_session_needed = true;
        }
        cfg.admin.host = REMOTE_HOST.into();
    } else {
        cfg.admin.host = LOCAL_HOST.into();
    }
    cfg.save(&st.app.config_path)?;
    st.app.set_cfg(cfg);
    let body = json!({"ok": true, "remote": b.remote, "needs_restart": st.app.needs_restart(), "signed_in_again": new_session_needed});
    let resp = Json(body).into_response();
    if new_session_needed {
        st.sessions.lock().unwrap().clear();
        return Ok(with_cookie(resp, new_session(&st, &headers)));
    }
    Ok(resp)
}

fn access_json(c: &Config) -> Value {
    json!({
        "remote": c.admin.remote(), "host": c.admin.host, "port": c.admin.port,
        "allowed_ips": c.admin.allowed_ips, "min_password": REMOTE_MIN_PASSWORD,
    })
}

// ------------------------------------------------------------------------------------------ branding

#[derive(Deserialize)]
struct BrandBody {
    name: String,
    color: String,
    logo: String,
    icon: String,
}

/// Files in the picture folder that no setting refers to any more.
fn tidy_pictures(c: &Config, before: &[String]) {
    let dir = crate::brand::dir(c);
    for old in before {
        if !old.is_empty() && old != &c.branding.logo && old != &c.branding.icon {
            crate::brand::remove(&dir, old);
        }
    }
}

async fn put_branding(State(st): St, Json(b): Json<BrandBody>) -> Api {
    let old = (*st.app.cfg()).clone();
    let (name, color) = (b.name.trim().to_string(), b.color.trim().to_string());
    if name.chars().count() > 40 || name.contains(['\n', '\r']) {
        return Err(ApiError::bad("The clan name can be up to 40 characters, on one line."));
    }
    if !color.is_empty() && crate::config::parse_color(&color).is_none() {
        return Err(ApiError::bad("The colour must look like #E67E22 (six letters/digits after the #)."));
    }
    let dir = crate::brand::dir(&old);
    for (what, v) in [("logo", &b.logo), ("icon", &b.icon)] {
        if !crate::brand::valid_asset_ref(v) {
            return Err(ApiError::bad(format!("The {} must be an uploaded picture or an https:// address of one.", what)));
        }
        if crate::brand::valid_asset_name(v) && !dir.join(v).is_file() {
            return Err(ApiError::bad(format!("The {} file is missing - upload it again.", what)));
        }
    }
    let mut new = old.clone();
    new.branding = crate::config::BrandingConfig { name, logo: b.logo, icon: b.icon, color };
    let before = vec![old.branding.logo.clone(), old.branding.icon.clone()];
    let r = commit(&st, new)?;
    tidy_pictures(&st.app.cfg(), &before);
    Ok(r)
}

/// The picture arrives as the raw request body (no multipart); its type comes from its bytes, never from a name.
async fn upload_picture(State(st): St, Path(slot): Path<String>, body: axum::body::Bytes) -> Api {
    if slot != "logo" && slot != "icon" {
        return Err(ApiError::not_found("Unknown picture slot."));
    }
    let old = (*st.app.cfg()).clone();
    let name = crate::brand::store(&crate::brand::dir(&old), &body).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut new = old.clone();
    let before = vec![old.branding.logo.clone(), old.branding.icon.clone()];
    if slot == "logo" { new.branding.logo = name.clone() } else { new.branding.icon = name.clone() }
    let r = commit(&st, new)?;
    tidy_pictures(&st.app.cfg(), &before);
    let mut v = r.0;
    v["file"] = json!(name);
    Ok(Json(v))
}

/// Preview inside the panel (the public copy is served by the web player for Discord).
async fn picture_preview(State(st): St, Path(name): Path<String>) -> Response {
    if !crate::brand::valid_asset_name(&name) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let ext = name.rsplit('.').next().unwrap_or("png").to_string();
    match tokio::fs::read(crate::brand::dir(&st.app.cfg()).join(&name)).await {
        Ok(data) => ([(header::CONTENT_TYPE, crate::brand::content_type(&ext)), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], data).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Deserialize)]
struct FindQuery {
    server: String,
    map: String,
}

/// "Does the bot find a picture for this map on this server?" - the panel's test button.
async fn levelshot_find(State(st): St, Query(q): Query<FindQuery>) -> Api {
    let cfg = st.app.cfg();
    let sc = cfg.servers.get(&q.server).ok_or_else(|| ApiError::not_found("No such server."))?.clone();
    let live = crate::brand::Live::from_config(&cfg);
    let sources = sc.levelshot_sources();
    let levels = st.app.levels.clone();
    let (map, size, fit) = (q.map.clone(), live.level_size, live.level_fit);
    let result = tokio::task::spawn_blocking(move || levels.resolve(&sources, &map, size, fit)).await.map_err(|e| ApiError::bad(e.to_string()))?;
    let searched: Vec<String> = sc.levelshot_sources().iter().map(|p| p.display().to_string()).collect();
    match result {
        Ok(file) => ok(json!({
            "found": true, "file": file, "preview": format!("/api/levelshot/{}", file), "size": live.level_size, "searched": searched,
            "discord_url": if live.public_url.is_empty() { Value::Null } else { json!(format!("{}/levelshot/{}", live.public_url.trim_end_matches('/'), file)) },
            "shown_in_discord": live.level_on,
        })),
        Err(message) => ok(json!({"found": false, "message": message, "searched": searched})),
    }
}

async fn levelshot_preview(State(st): St, Path(file): Path<String>) -> Response {
    if !crate::levelshot::valid_file_name(&file) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match tokio::fs::read(crate::levelshot::dir(&st.app.cfg()).join(&file)).await {
        Ok(data) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], data).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn branding_json(c: &Config) -> Value {
    let live = crate::brand::Live::from_config(c);
    json!({
        "name": c.branding.name, "color": c.branding.color, "logo": c.branding.logo, "icon": c.branding.icon,
        "logo_visible_in_discord": !live.logo_url.is_empty() || c.branding.logo.is_empty(),
        "icon_visible_in_discord": !live.icon_url.is_empty() || c.branding.icon.is_empty(),
        "public_address_set": c.web.as_ref().is_some_and(|w| !w.public_url.trim().is_empty()),
        "new_post_each_map": c.discord.new_post_each_map, "delete_old_posts": c.discord.delete_old_posts,
        "max_bytes": crate::brand::MAX_IMAGE_BYTES,
        "levelshots": {"enabled": c.levelshots.enabled, "size": live.level_size, "fit": c.levelshots.fit,
                       "min": crate::levelshot::MIN_SIZE, "max": crate::levelshot::MAX_SIZE},
    })
}

async fn index() -> Response {
    Html(UI.replace("{{version}}", VERSION)).into_response()
}

pub fn router(app: Arc<App>) -> Router {
    let st = Arc::new(AdminState { app, sessions: Mutex::new(HashMap::new()), fails: Mutex::new(vec![]) });
    Router::new()
        .route("/", get(index))
        .route("/api/hello", get(hello))
        .route("/api/login", post(login))
        .route("/api/setup", post(setup))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .route("/api/access", post(set_access))
        .route("/api/branding", put(put_branding))
        .route("/api/branding/{slot}", put(upload_picture).layer(DefaultBodyLimit::max(crate::brand::MAX_IMAGE_BYTES + 4096)))
        .route("/api/picture/{name}", get(picture_preview))
        .route("/api/levelshot-find", get(levelshot_find))
        .route("/api/levelshot/{file}", get(levelshot_preview))
        .route("/api/state", get(state))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/servers/{key}", put(put_server).delete(delete_server))
        .route("/api/servers/{key}/test", post(test_server))
        .route("/api/servers/{key}/test-stream", post(test_stream))
        .route("/api/gpu/detect", post(detect_gpu))
        .route("/api/filesystems/{name}", put(put_filesystem).delete(delete_filesystem))
        .route("/api/test/filesystem", post(test_filesystem))
        .route("/api/browse", post(browse))
        .route("/api/discord/info", post(discord_info))
        .route("/api/discord/reconnect", post(discord_reconnect))
        .route("/api/discord/roles", get(discord_roles))
        .route("/api/discord/user", get(discord_user))
        .route("/api/discord/members", get(discord_members))
        .route("/api/broadcast/start", post(broadcast_start))
        .route("/api/broadcast/stop", post(broadcast_stop))
        .route("/api/import", post(import_config))
        .route("/api/logs", get(logs))
        .route("/api/restart", post(restart))
        .layer(middleware::from_fn_with_state(st.clone(), require_login))
        .layer(middleware::from_fn_with_state(st.clone(), gate))
        .with_state(st)
}

pub async fn serve(listener: tokio::net::TcpListener, router: Router, mut stop: crate::util::StopRx) {
    let shutdown = async move {
        stop.sleep(Duration::from_secs(86_400 * 365)).await;
    };
    let svc = router.into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, svc).with_graceful_shutdown(shutdown).await {
        tracing::error!("admin panel stopped: {}", e);
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::logbuf::LogBuffer;
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct Client {
        app: Router,
        cookie: Option<String>,
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gtv-admin-{}", random_token(4)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn client(password: Option<&str>, peer: &str) -> (Client, Arc<App>, std::path::PathBuf) {
        let dir = tmpdir();
        let mut cfg = Config::default();
        cfg.data_dir = dir.join("data").to_string_lossy().into_owned();
        if let Some(p) = password {
            cfg.admin.password_hash = hash_password(p);
        }
        let path = dir.join("config.json");
        cfg.save(&path).unwrap();
        let app = App::new(path.clone(), cfg, LogBuffer::new(), None);
        let sock: SocketAddr = peer.parse().unwrap();
        (Client { app: router(app.clone()).layer(MockConnectInfo(sock)), cookie: None }, app, path)
    }

    impl Client {
        async fn call(&mut self, method: &str, uri: &str, body: Option<Value>, csrf: bool, host: &str) -> (StatusCode, Value, Option<String>) {
            let mut r = axum::http::Request::builder().method(method).uri(uri).header(header::HOST, host);
            if csrf { r = r.header("x-requested-with", "gtv-admin"); }
            if let Some(c) = &self.cookie { r = r.header(header::COOKIE, c.clone()); }
            let req = match body {
                Some(b) => r.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())).unwrap(),
                None => r.body(Body::empty()).unwrap(),
            };
            let resp = self.app.clone().oneshot(req).await.unwrap();
            let status = resp.status();
            let set = resp.headers().get(header::SET_COOKIE).and_then(|v| v.to_str().ok()).map(|s| s.split(';').next().unwrap().to_string());
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), set)
        }
        async fn go(&mut self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
            let (s, v, c) = self.call(method, uri, body, true, "127.0.0.1:8787").await;
            if let Some(c) = c { self.cookie = Some(c); }
            (s, v)
        }
        async fn raw(&mut self, method: &str, uri: &str, bytes: Vec<u8>) -> (StatusCode, Value) {
            let mut r = axum::http::Request::builder().method(method).uri(uri).header(header::HOST, "127.0.0.1:8787").header("x-requested-with", "gtv-admin");
            if let Some(c) = &self.cookie { r = r.header(header::COOKIE, c.clone()); }
            let resp = self.app.clone().oneshot(r.body(Body::from(bytes)).unwrap()).await.unwrap();
            let status = resp.status();
            let b = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
        }
        async fn login(&mut self, pw: &str) -> StatusCode {
            self.go("POST", "/api/login", Some(json!({"password": pw}))).await.0
        }
    }

    #[tokio::test]
    async fn everything_needs_a_session_except_login_and_hello() {
        let (mut c, _, _) = client(Some("correct horse"), "127.0.0.1:5000");
        for (m, u) in [("GET", "/api/state"), ("GET", "/api/config"), ("POST", "/api/restart"), ("GET", "/api/logs")] {
            assert_eq!(c.go(m, u, None).await.0, StatusCode::UNAUTHORIZED, "{} {}", m, u);
        }
        let (s, v) = c.go("GET", "/api/hello", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["configured"], true);
        assert_eq!(c.login("wrong").await, StatusCode::UNAUTHORIZED);
        assert_eq!(c.go("GET", "/api/state", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(c.login("correct horse").await, StatusCode::OK);
        assert_eq!(c.go("GET", "/api/state", None).await.0, StatusCode::OK);
        // the page itself is served without a session (it shows the login form)
        let (s, _, _) = c.call("GET", "/", None, false, "x").await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn changing_requests_need_the_csrf_header() {
        let (mut c, _, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        c.login("pw-pw-pw-1").await;
        let (s, v, _) = c.call("PUT", "/api/servers/a", Some(json!({"host": "h"})), false, "127.0.0.1").await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{}", v);
        let (s, _) = c.go("PUT", "/api/servers/a", Some(json!({"host": "h"}))).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn too_many_wrong_passwords_are_throttled() {
        let (mut c, _, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        for _ in 0..8 {
            assert_eq!(c.login("nope").await, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(c.login("pw-pw-pw-1").await, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn first_password_only_from_this_computer_and_only_once() {
        let (mut remote, _, _) = client(None, "192.168.1.9:5000");
        let (s, _) = remote.go("POST", "/api/setup", Some(json!({"password": "longenough1"}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (mut local, app, path) = client(None, "127.0.0.1:5000");
        // a web page reaching us through a rebinding hostname is refused
        let (s, _, _) = local.call("POST", "/api/setup", Some(json!({"password": "longenough1"})), true, "evil.example:8787").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(local.go("POST", "/api/setup", Some(json!({"password": "short"}))).await.0, StatusCode::BAD_REQUEST);
        let (s, _) = local.go("POST", "/api/setup", Some(json!({"password": "longenough1"}))).await;
        assert_eq!(s, StatusCode::OK);
        assert!(verify_password("longenough1", &Config::load(&path).unwrap().admin.password_hash));
        assert!(verify_password("longenough1", &app.cfg().admin.password_hash));
        // signed in by the setup call
        assert_eq!(local.go("GET", "/api/state", None).await.0, StatusCode::OK);
        // a second attempt is refused
        let (mut other, _, _) = (Client { app: local.app.clone(), cookie: None }, 0, 0);
        assert_eq!(other.go("POST", "/api/setup", Some(json!({"password": "anotherpass1"}))).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn servers_and_secrets_round_trip_without_leaking() {
        let (mut c, app, path) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        c.login("pw-pw-pw-1").await;
        let (s, _) = c.go("PUT", "/api/servers/main", Some(json!({"name": "Main", "host": "10.0.0.5", "port": 27960, "rcon_password": "sekrit-rcon"}))).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = c.go("PUT", "/api/servers/second", Some(json!({"host": "10.0.0.6"}))).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(app.cfg().servers.len(), 2);
        let (_, v) = c.go("GET", "/api/config", None).await;
        assert!(!v.to_string().contains("sekrit-rcon"), "{}", v);
        assert_eq!(v["config"]["servers"]["main"]["rcon_password"], MASK);
        assert_eq!(v["config"]["admin"]["password_hash"], "");
        // saving what the panel received must not destroy the secret or the admin password
        let (s, _) = c.go("PUT", "/api/config", Some(v.clone())).await;
        assert_eq!(s, StatusCode::OK);
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.servers["main"].rcon_password, "sekrit-rcon");
        assert!(verify_password("pw-pw-pw-1", &saved.admin.password_hash));
        // server order is preserved and a bad key / unknown field is refused with a message
        assert_eq!(saved.servers.keys().collect::<Vec<_>>(), vec!["main", "second"]);
        assert_eq!(c.go("PUT", "/api/servers/bad%20key", Some(json!({"host": "x"}))).await.0, StatusCode::BAD_REQUEST);
        let (s, v) = c.go("PUT", "/api/servers/k", Some(json!({"host": "x", "bogus": 1}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("bogus"));
        assert_eq!(c.go("DELETE", "/api/servers/second", None).await.0, StatusCode::OK);
        assert_eq!(Config::load(&path).unwrap().servers.len(), 1);
        assert_eq!(c.go("DELETE", "/api/servers/second", None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn server_test_talks_udp_and_reports_in_words() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((_, from)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(b"\xff\xff\xff\xffstatusResponse\n\\mapname\\ut4_turnpike\\g_gametype\\4\\sv_hostname\\Fake\n5 20 \"Alice\"\n", from).await;
            }
        });
        let (mut c, _, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        c.login("pw-pw-pw-1").await;
        let (s, v) = c.go("POST", "/api/servers/x/test", Some(json!({"host": "127.0.0.1", "port": port}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        let udp = v["checks"][0].clone();
        assert_eq!(udp["status"], "ok", "{}", v);
        assert!(udp["msg"].as_str().unwrap().contains("ut4_turnpike"));
        assert_eq!(c.go("POST", "/api/servers/missing/test", None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ssh_test_works_for_typed_and_saved_connections_and_explains_failures() {
        let (mut c, _, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        c.login("pw-pw-pw-1").await;
        // nothing typed and nothing saved under that name: a clear message, not a crash
        let (s, v) = c.go("POST", "/api/test/filesystem", Some(json!({"name": "ghost", "file": "/x"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{}", v);
        assert!(v["error"].as_str().unwrap().contains("host and user are required"), "{}", v);
        // a closed port is reported as not connected, in words (and does not ask for host-key approval)
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let typed = json!({"host": "127.0.0.1", "port": closed, "user": "u", "password": "p", "file": "/srv/games.log"});
        let (s, v) = c.go("POST", "/api/test/filesystem", Some(typed.clone())).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(v["connected"], false);
        assert!(v["needs_host_key_approval"].is_null() || v["needs_host_key_approval"] == false);
        // the same, saved first and tested by name only
        assert_eq!(c.go("PUT", "/api/filesystems/box", Some(json!({"type": "sftp", "host": "127.0.0.1", "port": closed, "user": "u", "password": "p"}))).await.0, StatusCode::OK);
        let (s, v) = c.go("POST", "/api/test/filesystem", Some(json!({"name": "box", "file": "/srv/games.log"}))).await;
        assert_eq!((s, &v["connected"]), (StatusCode::OK, &json!(false)), "{}", v);
    }

    #[tokio::test]
    async fn file_picker_lists_folders_and_explains_failures() {
        let (mut c, _, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        // needs a login like every other call
        let (s, _) = c.go("POST", "/api/browse", Some(json!({"local": true, "path": "/"}))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        c.login("pw-pw-pw-1").await;
        let dir = std::env::temp_dir().join(format!("gtv-browse-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(dir.join("q3ut4")).unwrap();
        std::fs::write(dir.join("games.log"), "x").unwrap();
        std::fs::write(dir.join("Quake3-UrT.x86_64"), "bin").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("Quake3-UrT.x86_64"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (s, v) = c.go("POST", "/api/browse", Some(json!({"local": true, "path": dir.to_string_lossy()}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(v["listed"], true, "{}", v);
        let names: Vec<&str> = v["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["q3ut4", "games.log", "Quake3-UrT.x86_64"], "folders first, then files by name");
        assert_eq!(v["entries"][0]["dir"], true);
        #[cfg(unix)]
        assert_eq!(v["entries"][2]["exec"], true);
        assert_eq!(v["entries"][1]["exec"], false);
        assert!(v["parent"].is_string());
        // a typo in the path is explained, not a crash
        let typo = dir.join("GamesServer");
        let (s, v) = c.go("POST", "/api/browse", Some(json!({"local": true, "path": typo.to_string_lossy()}))).await;
        assert_eq!((s, &v["listed"]), (StatusCode::OK, &json!(false)), "{}", v);
        assert!(v["error"].as_str().unwrap().contains("does not exist"), "{}", v);
        std::fs::create_dir_all(dir.join("GameServer")).unwrap();
        let (_, v) = c.go("POST", "/api/browse", Some(json!({"local": true, "path": dir.join("GamesServer").to_string_lossy()}))).await;
        assert_eq!(v["suggest"], json!(dir.join("GameServer").to_string_lossy()), "{}", v);
        // an SSH browse without a host or a saved login is a clear message
        let (s, v) = c.go("POST", "/api/browse", Some(json!({"name": "ghost", "path": "/"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{}", v);
        // an unreachable SSH host is reported as not listed
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (s, v) = c.go("POST", "/api/browse", Some(json!({"host": "127.0.0.1", "port": closed, "user": "u", "password": "p", "path": "/"}))).await;
        assert_eq!((s, &v["listed"]), (StatusCode::OK, &json!(false)), "{}", v);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gpu_detection_endpoint_reports_and_needs_login() {
        let (mut c, _, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        assert_eq!(c.go("POST", "/api/gpu/detect", Some(json!({}))).await.0, StatusCode::UNAUTHORIZED);
        c.login("pw-pw-pw-1").await;
        let (s, v) = c.go("POST", "/api/gpu/detect", Some(json!({"ffmpeg_binary": "/no/such/ffmpeg"}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(v["report"]["ffmpeg_found"], false);
        assert_eq!(v["report"]["recommended"], "cpu");
        assert!(v["text"].as_str().unwrap().contains("could not be run"));
    }

    #[tokio::test]
    async fn password_change_requires_the_current_one() {
        let (mut c, app, _) = client(Some("old-password-1"), "127.0.0.1:5000");
        c.login("old-password-1").await;
        assert_eq!(c.go("POST", "/api/password", Some(json!({"current": "wrong", "new": "brand-new-pass"}))).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(c.go("POST", "/api/password", Some(json!({"current": "old-password-1", "new": "short"}))).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(c.go("POST", "/api/password", Some(json!({"current": "old-password-1", "new": "brand-new-pass"}))).await.0, StatusCode::OK);
        assert!(verify_password("brand-new-pass", &app.cfg().admin.password_hash));
    }

    #[tokio::test]
    async fn python_config_can_be_imported() {
        let (mut c, app, _) = client(Some("pw-pw-pw-1"), "127.0.0.1:5000");
        c.login("pw-pw-pw-1").await;
        let text = "# from the python bot\n{\"discord\": {\"token\": \"a.b.c\"}, \"servers\": {\"one\": {\"host\": \"1.2.3.4\"}}}";
        let (s, v) = c.go("POST", "/api/import", Some(json!({"text": text}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(v["imported_servers"], 1);
        assert_eq!(app.cfg().discord.token, "a.b.c");
        assert!(app.cfg().servers.contains_key("one"));
        assert_eq!(c.go("POST", "/api/import", Some(json!({"text": "not json"}))).await.0, StatusCode::BAD_REQUEST);
    }

    fn at(app: &Arc<App>, peer: &str) -> Client {
        Client { app: router(app.clone()).layer(MockConnectInfo(peer.parse::<SocketAddr>().unwrap())), cookie: None }
    }

    fn edit_cfg(app: &Arc<App>, f: impl FnOnce(&mut Config)) {
        let mut c = (*app.cfg()).clone();
        f(&mut c);
        app.set_cfg(c);
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

    #[tokio::test]
    async fn only_this_computer_can_open_the_panel_unless_remote_access_is_on() {
        let (_, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        let mut far = at(&app, "203.0.113.9:5000");
        for (m, u) in [("GET", "/"), ("GET", "/api/hello"), ("POST", "/api/login"), ("POST", "/api/setup")] {
            let (s, _, _) = far.call(m, u, Some(json!({"password": "correct horse"})), true, "127.0.0.1").await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{} {}", m, u);
        }
        assert_eq!(at(&app, "127.0.0.1:1").login("correct horse").await, StatusCode::OK);
        assert_eq!(at(&app, "[::1]:1").login("correct horse").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn remote_access_can_be_limited_to_listed_addresses() {
        let (_, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        edit_cfg(&app, |c| {
            c.admin.host = "0.0.0.0".into();
            c.admin.allowed_ips = vec!["203.0.113.0/24".into(), "2001:db8::/32".into(), "198.51.100.7".into()];
        });
        for (peer, ok_) in [("203.0.113.9:1", true), ("198.51.100.7:1", true), ("198.51.100.8:1", false), ("203.0.114.1:1", false),
                            ("[2001:db8::1]:1", true), ("[2001:db9::1]:1", false), ("[::ffff:203.0.113.5]:1", true), ("127.0.0.1:1", true)] {
            let (s, _, _) = at(&app, peer).call("GET", "/api/hello", None, true, "x").await;
            assert_eq!(s == StatusCode::OK, ok_, "{} -> {}", peer, s);
        }
        // no list: any address that knows the password
        edit_cfg(&app, |c| c.admin.allowed_ips.clear());
        assert_eq!(at(&app, "198.51.100.8:1").call("GET", "/api/hello", None, true, "x").await.0, StatusCode::OK);
        // and switching it off in the settings closes the door at once, before any restart
        edit_cfg(&app, |c| c.admin.host = "127.0.0.1".into());
        assert_eq!(at(&app, "198.51.100.8:1").call("GET", "/api/hello", None, true, "x").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn switching_remote_access_on_needs_the_password_again_and_a_long_new_one() {
        let (mut c, app, path) = client(Some("correct horse"), "127.0.0.1:5000");
        c.login("correct horse").await;
        let st = c.go("GET", "/api/state", None).await.1;
        assert_eq!(st["access"]["remote"], false);
        let on = |pw: &str, np: Option<&str>| json!({"remote": true, "password": pw, "new_password": np});
        assert_eq!(c.go("POST", "/api/access", Some(on("wrong", Some("a very long new password")))).await.0, StatusCode::UNAUTHORIZED);
        for np in [None, Some(""), Some("short-11-ch")] {
            let (s, v) = c.go("POST", "/api/access", Some(on("correct horse", np))).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{:?} {}", np, v);
            assert!(v["error"].as_str().unwrap().contains("12"), "{}", v);
        }
        assert!(!app.cfg().admin.remote(), "nothing changed by the refusals");
        // a bad address list is refused with the entry named
        let (s, v) = c.go("POST", "/api/access", Some(json!({"remote": true, "password": "correct horse", "new_password": "a very long new password", "allowed_ips": ["10.0.0.0/8", "not-an-ip"]}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("not-an-ip"));
        // good
        let (s, v) = c.go("POST", "/api/access", Some(json!({"remote": true, "password": "correct horse", "new_password": "a very long new password", "allowed_ips": ["10.0.0.0/8"]}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!((v["remote"].clone(), v["needs_restart"].clone(), v["signed_in_again"].clone()), (json!(true), json!(true), json!(true)));
        let saved = Config::load(&path).unwrap();
        assert_eq!((saved.admin.host.as_str(), saved.admin.allowed_ips.clone()), ("0.0.0.0", vec!["10.0.0.0/8".to_string()]));
        assert!(verify_password("a very long new password", &saved.admin.password_hash) && !verify_password("correct horse", &saved.admin.password_hash));
        // the session was replaced and still works; the old password does not
        assert_eq!(c.go("GET", "/api/state", None).await.1["access"]["remote"], true);
        assert_eq!(at(&app, "127.0.0.1:2").login("correct horse").await, StatusCode::UNAUTHORIZED);
        assert_eq!(at(&app, "10.1.2.3:2").login("a very long new password").await, StatusCode::OK);
        assert_eq!(at(&app, "192.168.1.5:2").login("a very long new password").await, StatusCode::FORBIDDEN);
        // while open to the network a short password cannot be chosen
        let (s, _) = c.go("POST", "/api/password", Some(json!({"current": "a very long new password", "new": "eightchr"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // turning it off again needs the password too, and closes the door at once
        assert_eq!(c.go("POST", "/api/access", Some(json!({"remote": false, "password": "nope"}))).await.0, StatusCode::UNAUTHORIZED);
        let (s, v) = c.go("POST", "/api/access", Some(json!({"remote": false, "password": "a very long new password"}))).await;
        assert_eq!((s, v["remote"].clone()), (StatusCode::OK, json!(false)));
        assert_eq!(Config::load(&path).unwrap().admin.host, "127.0.0.1");
        assert_eq!(at(&app, "10.1.2.3:2").call("GET", "/api/hello", None, true, "x").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn you_cannot_lock_yourself_out_with_the_address_list() {
        let (_, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        edit_cfg(&app, |c| c.admin.host = "0.0.0.0".into());
        let mut far = at(&app, "203.0.113.9:5000");
        assert_eq!(far.login("correct horse").await, StatusCode::OK);
        let (s, v) = far.go("POST", "/api/access", Some(json!({"remote": true, "password": "correct horse", "allowed_ips": ["10.0.0.0/8"]}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("203.0.113.9") && v["error"].as_str().unwrap().contains("lock yourself out"), "{}", v);
        assert!(app.cfg().admin.allowed_ips.is_empty());
        let (s, _) = far.go("POST", "/api/access", Some(json!({"remote": true, "password": "correct horse", "allowed_ips": ["203.0.113.0/24"]}))).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_settings_page_cannot_open_the_panel_to_the_network() {
        let (mut c, app, path) = client(Some("correct horse"), "127.0.0.1:5000");
        c.login("correct horse").await;
        let mut cfg = c.go("GET", "/api/config", None).await.1["config"].clone();
        cfg["admin"]["host"] = json!("0.0.0.0");
        cfg["admin"]["allowed_ips"] = json!(["1.2.3.4"]);
        cfg["admin"]["port"] = json!(9999);
        let (s, _) = c.go("PUT", "/api/config", Some(json!({"config": cfg}))).await;
        assert_eq!(s, StatusCode::OK);
        let saved = Config::load(&path).unwrap();
        assert_eq!((saved.admin.host.as_str(), saved.admin.allowed_ips.len(), saved.admin.port), ("127.0.0.1", 0, 9999));
        assert!(!app.cfg().admin.remote());
    }

    #[tokio::test]
    async fn clan_pictures_are_checked_stored_replaced_and_cleaned_up() {
        let (mut c, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        assert_eq!(c.raw("PUT", "/api/branding/logo", PNG.to_vec()).await.0, StatusCode::UNAUTHORIZED);
        c.login("correct horse").await;
        let dir = crate::brand::dir(&app.cfg());
        // wrong kinds of file are refused whatever they are called
        for bad in [b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec(), b"MZ\x90\0 an exe".to_vec(), vec![]] {
            let (s, v) = c.raw("PUT", "/api/branding/logo", bad).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{}", v);
        }
        assert_eq!(c.raw("PUT", "/api/branding/banner", PNG.to_vec()).await.0, StatusCode::NOT_FOUND);
        let mut huge = PNG.to_vec();
        huge.resize(crate::brand::MAX_IMAGE_BYTES + 10_000, 0);
        assert_eq!(c.raw("PUT", "/api/branding/logo", huge).await.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(app.cfg().branding.logo, "");
        // a good one
        let (s, v) = c.raw("PUT", "/api/branding/logo", PNG.to_vec()).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        let first = v["file"].as_str().unwrap().to_string();
        assert!(crate::brand::valid_asset_name(&first) && dir.join(&first).is_file());
        assert_eq!(app.cfg().branding.logo, first);
        assert_eq!(app.live.read().unwrap().logo_url, "", "no public address yet, so Discord cannot be given a link");
        let st = c.go("GET", "/api/state", None).await.1;
        assert_eq!((st["branding"]["logo_visible_in_discord"].clone(), st["branding"]["public_address_set"].clone()), (json!(false), json!(false)));
        // preview inside the panel (needs the session)
        let r = c.app.clone().oneshot(axum::http::Request::builder().uri(format!("/api/picture/{}", first)).header(header::COOKIE, c.cookie.clone().unwrap()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!((r.status(), r.headers()[header::CONTENT_TYPE].to_str().unwrap().to_string()), (StatusCode::OK, "image/png".to_string()));
        let r = c.app.clone().oneshot(axum::http::Request::builder().uri("/api/picture/..%2Fconfig.json").header(header::COOKIE, c.cookie.clone().unwrap()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        // the same picture as the icon shares the file; replacing the logo keeps it for the icon
        let (_, v) = c.raw("PUT", "/api/branding/icon", PNG.to_vec()).await;
        assert_eq!(v["file"].as_str().unwrap(), first);
        let mut other = PNG.to_vec();
        other.extend_from_slice(b"different");
        let (_, v) = c.raw("PUT", "/api/branding/logo", other).await;
        let second = v["file"].as_str().unwrap().to_string();
        assert_ne!(first, second);
        assert!(dir.join(&first).is_file() && dir.join(&second).is_file());
        // clearing the icon leaves nothing using the first file: it is deleted
        let body = json!({"name": "The Forge", "color": "#e67e22", "logo": second, "icon": ""});
        let (s, v) = c.go("PUT", "/api/branding", Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert!(!dir.join(&first).exists() && dir.join(&second).is_file());
        let live = app.live.read().unwrap().clone();
        assert_eq!((live.name.as_str(), live.color), ("The Forge", Some(0xE67E22)), "the running bot picks the change up without a restart");
    }

    #[tokio::test]
    async fn branding_settings_are_validated() {
        let (mut c, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        c.login("correct horse").await;
        let put = |name: &str, color: &str, logo: &str, icon: &str| json!({"name": name, "color": color, "logo": logo, "icon": icon});
        for (body, what) in [
            (put(&"x".repeat(41), "", "", ""), "40"),
            (put("two\nlines", "", "", ""), "one line"),
            (put("ok", "orange", "", ""), "colour"),
            (put("ok", "#12345", "", ""), "colour"),
            (put("ok", "", "../../etc/passwd", ""), "logo"),
            (put("ok", "", "", "javascript:alert(1)"), "icon"),
            (put("ok", "", "0123456789abcdef.png", ""), "missing"),
        ] {
            let (s, v) = c.go("PUT", "/api/branding", Some(body)).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{}", v);
            assert!(v["error"].as_str().unwrap().contains(what), "{} / {}", what, v);
        }
        assert_eq!(app.cfg().branding, crate::config::BrandingConfig::default());
        let (s, _) = c.go("PUT", "/api/branding", Some(put("  Clan  ", "#00FF00", "https://x.example/l.png", "https://x.example/i.png"))).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(app.cfg().branding.name, "Clan");
        assert_eq!(app.live.read().unwrap().logo_url, "https://x.example/l.png");
    }

    #[tokio::test]
    async fn the_panel_saves_the_rcon_switch_and_role_and_never_shows_the_password() {
        let (mut c, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        assert_eq!(c.go("GET", "/api/discord/roles", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(c.login("correct horse").await, StatusCode::OK);
        // the switch and the password go in together; the password stays out of everything the panel can read back
        let (s, v) = c.go("PUT", "/api/servers/a", Some(json!({"host": "h", "rcon_password": "sekrit-rcon", "rcon_commands": true}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert!(app.cfg().servers["a"].rcon_commands && app.cfg().servers["a"].rcon_password == "sekrit-rcon");
        let (_, v) = c.go("GET", "/api/config", None).await;
        assert_eq!(v["config"]["servers"]["a"]["rcon_commands"], true);
        assert!(!v.to_string().contains("sekrit-rcon"), "the rcon password leaked into /api/config");
        // the admin role is saved with the rest of the config, as ids
        let mut cfg = v["config"].clone(); // exactly what the panel was shown: the password is masked
        assert_ne!(cfg["servers"]["a"]["rcon_password"], "sekrit-rcon");
        cfg["discord"]["rcon_roles"] = json!(["123456789012345678"]);
        let (s, v) = c.go("PUT", "/api/config", Some(json!({"config": cfg}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(app.cfg().discord.rcon_roles, vec!["123456789012345678"]);
        assert!(app.cfg().servers["a"].rcon_password == "sekrit-rcon", "saving the config with the masked password must keep the real one");
        // no bot connected: the role list says so in words, and the id can still be typed by hand
        let (s, v) = c.go("GET", "/api/discord/roles", None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v.to_string().contains("not connected") && v.to_string().contains("by hand"), "{}", v);
        // the configuration check follows the switch
        let (_, st) = c.go("GET", "/api/state", None).await;
        assert!(!st.to_string().contains("rcon_commands needs"), "{}", st["problems"]);
    }

    #[tokio::test]
    async fn the_panel_saves_the_chosen_people_and_finds_them_by_id_or_name() {
        let (mut c, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        assert_eq!(c.go("GET", "/api/discord/user?id=5", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(c.go("GET", "/api/discord/members?q=bob", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(c.login("correct horse").await, StatusCode::OK);
        // the chosen people are saved with the rest of the config, as ids
        let (_, v) = c.go("GET", "/api/config", None).await;
        let mut cfg = v["config"].clone();
        cfg["discord"]["rcon_users"] = json!(["123456789012345678", 987654321]);
        let (s, v) = c.go("PUT", "/api/config", Some(json!({"config": cfg}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(app.cfg().discord.rcon_users, vec!["123456789012345678", "987654321"]);
        let (_, v) = c.go("GET", "/api/config", None).await;
        assert_eq!(v["config"]["discord"]["rcon_users"], json!(["123456789012345678", "987654321"]));
        // a typo is refused in words, before Discord is even asked
        for bad in ["/api/discord/user?id=bob", "/api/discord/user?id=0", "/api/discord/user", "/api/discord/user?id=-5"] {
            let (s, v) = c.go("GET", bad, None).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{}", bad);
            assert!(v["error"].as_str().unwrap().contains("long number"), "{} {}", bad, v);
        }
        // no bot connected: both lookups say so and point to the id
        let (s, v) = c.go("GET", "/api/discord/user?id=123456789012345678", None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("not connected") && v["error"].as_str().unwrap().contains("can still be added"), "{}", v);
        let (s, v) = c.go("GET", "/api/discord/members?q=bob", None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("not connected") && v["error"].as_str().unwrap().contains("user id"), "{}", v);
        let (s, v) = c.go("GET", "/api/discord/members?q=%20", None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("part of a name"), "{}", v);
        // a name that is not an id is refused by the configuration check (and the person is named in it)
        let mut cfg = c.go("GET", "/api/config", None).await.1["config"].clone();
        cfg["discord"]["rcon_users"] = json!(["Bob"]);
        c.go("PUT", "/api/config", Some(json!({"config": cfg}))).await;
        let (_, st) = c.go("GET", "/api/state", None).await;
        assert!(st.to_string().contains("'Bob' is not a user id"), "{}", st["problems"]);
    }

    #[tokio::test]
    async fn the_panel_can_test_whether_a_map_picture_is_found() {
        use crate::levelshot::tests::{sample, tmp, write_pk3};
        let (mut c, app, _) = client(Some("correct horse"), "127.0.0.1:5000");
        assert_eq!(c.go("GET", "/api/levelshot-find?server=a&map=ut4_x", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(c.login("correct horse").await, StatusCode::OK);
        let games = tmp("panel");
        write_pk3(&games.join("maps.pk3"), &[("levelshots/ut4_turnpike.jpg", sample(256, 192, "jpg"))]);
        let (s, v) = c.go("PUT", "/api/servers/a", Some(json!({"host": "h", "levelshot_dir": games.to_string_lossy()}))).await;
        assert_eq!(s, StatusCode::OK, "{}", v);
        assert_eq!(app.cfg().servers["a"].levelshot_dir, games.to_string_lossy());

        let (s, v) = c.go("GET", "/api/levelshot-find?server=a&map=UT4_Turnpike", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["found"], true, "{}", v);
        let file = v["file"].as_str().unwrap().to_string();
        assert!(crate::levelshot::valid_file_name(&file) && v["size"] == 320 && v["preview"] == format!("/api/levelshot/{}", file));
        assert_eq!(v["shown_in_discord"], false, "no public address yet");
        assert_eq!(v["discord_url"], Value::Null);
        // the picture itself, inside the panel only
        let r = c.app.clone().oneshot(axum::http::Request::builder().uri(format!("/api/levelshot/{}", file)).header(header::COOKIE, c.cookie.clone().unwrap()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!((r.status(), r.headers()[header::CONTENT_TYPE].to_str().unwrap().to_string()), (StatusCode::OK, "image/jpeg".to_string()));
        let r = c.app.clone().oneshot(axum::http::Request::builder().uri(format!("/api/levelshot/{}", file)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = c.app.clone().oneshot(axum::http::Request::builder().uri("/api/levelshot/..%2Fconfig.json").header(header::COOKIE, c.cookie.clone().unwrap()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // misses are explained in words, with where it looked
        let (_, v) = c.go("GET", "/api/levelshot-find?server=a&map=ut4_nowhere", None).await;
        assert_eq!(v["found"], false);
        assert!(v["message"].as_str().unwrap().contains("No picture for ut4_nowhere") && v["searched"][0] == games.to_string_lossy().as_ref(), "{}", v);
        let (_, v) = c.go("GET", "/api/levelshot-find?server=a&map=%2E%2E%2Fx", None).await;
        assert!(v["found"] == false && v["message"].as_str().unwrap().contains("not a map name"), "{}", v);
        assert_eq!(c.go("GET", "/api/levelshot-find?server=zzz&map=ut4_x", None).await.0, StatusCode::NOT_FOUND);

        // with pictures on and a public address, the panel reports the address Discord will fetch
        let mut cfg = (*app.cfg()).clone();
        cfg.web = Some(crate::config::WebConfig { public_url: "http://play.example:8080".into(), ..Default::default() });
        app.set_cfg(cfg);
        let (_, v) = c.go("GET", "/api/levelshot-find?server=a&map=ut4_turnpike", None).await;
        assert_eq!(v["shown_in_discord"], true);
        assert_eq!(v["discord_url"], format!("http://play.example:8080/levelshot/{}", file));
        let st = c.go("GET", "/api/state", None).await.1;
        assert_eq!((st["branding"]["levelshots"]["enabled"].clone(), st["branding"]["levelshots"]["size"].clone()), (json!(true), json!(320)));
        let _ = std::fs::remove_dir_all(games);
    }
}
