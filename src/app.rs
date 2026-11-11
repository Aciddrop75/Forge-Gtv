// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Application state shared by the Discord layer, the web player and the admin panel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde::Serialize;

use crate::config::Config;
use crate::embed::{Chat, Embed};
use crate::fs::FileSystems;
use crate::logbuf::LogBuffer;
use crate::spectator::Streamer;
use crate::urt::{esc, now};
use crate::util::{Stop, utc_iso};
use crate::version::{APP_NAME, OWNER, RELEASE_DATE, VERSION};
use crate::brand::Live;
use crate::viewers::Viewers;
use crate::watch::{Watch, WatchDeps};

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "state", content = "detail")]
pub enum DiscordStatus {
    /// No token configured yet.
    NoToken,
    Connecting,
    Ready(String),
    /// The token was refused / the connection failed.
    Failed(String),
    Stopped,
}

/// One broadcast = one game server shown in one Discord channel. A channel can carry several (one per server), and a
/// server can be shown in several channels.
pub type WatchId = (u64, String);

/// A server's own web-player port (`spectator.web_port`), open while that server's video runs.
struct PortServer {
    stop: Stop,
    task: tokio::task::JoinHandle<()>,
    port: u16,
}

/// What `/gtv list` and the admin panel show about a running broadcast.
#[derive(Clone, Debug, Serialize)]
pub struct BroadcastInfo {
    pub channel_id: String,
    pub server: String,
    pub name: String,
    pub delay: u32,
    pub video: bool,
    pub watch_url: Option<String>,
    pub viewers: u32,
    pub viewers_open: u32,
    /// The server's own player port; 0 = the shared one.
    pub web_port: u16,
}

pub struct App {
    pub config_path: PathBuf,
    cfg: RwLock<Arc<Config>>,
    chat: RwLock<Option<Arc<dyn Chat>>>,
    pub discord: RwLock<DiscordStatus>,
    pub watches: Mutex<HashMap<WatchId, Arc<Watch>>>,
    ports: Mutex<HashMap<String, PortServer>>,
    pub streamers: Mutex<HashMap<String, Arc<Streamer>>>,
    streamer_users: Mutex<HashMap<String, u32>>,
    pub started: Instant,
    pub previous_version: Option<String>,
    pub logs: LogBuffer,
    pub shutdown: Stop,
    /// Settings that only take effect after a restart, as they were when the process started.
    running_fingerprint: String,
    pub discord_ctl: tokio::sync::Mutex<Option<crate::discord::DiscordHandle>>,
    /// A plain-words notice about the Discord connection that is not an error (for example: connected, but chat relay is off).
    pub discord_note: RwLock<String>,
    /// The connected bot's HTTP client (to list roles for the panel and to refresh the slash commands).
    discord_http: RwLock<Option<Arc<serenity::http::Http>>>,
    /// Clan branding and post options, shared with the running broadcasts.
    pub live: Arc<RwLock<Arc<Live>>>,
    /// People with the web player open.
    pub viewers: Arc<Viewers>,
    /// The map pictures (found in the game files, resized, stored under `data/levelshots`).
    pub levels: Arc<crate::levelshot::Levelshots>,
}

pub fn fingerprint(c: &Config) -> String {
    format!("{}|{:?}|{}:{}|{}", c.discord_token().len(), c.web.as_ref().map(|w| (&w.host, w.port)), c.admin.host, c.admin.port, c.admin.enabled)
}

impl App {
    pub fn new(config_path: PathBuf, cfg: Config, logs: LogBuffer, previous_version: Option<String>) -> Arc<App> {
        let fp = fingerprint(&cfg);
        let cfg_copy = cfg.clone();
        Arc::new(App {
            config_path,
            cfg: RwLock::new(Arc::new(cfg)),
            chat: RwLock::new(None),
            discord: RwLock::new(DiscordStatus::Stopped),
            watches: Mutex::new(HashMap::new()),
            ports: Mutex::new(HashMap::new()),
            streamers: Mutex::new(HashMap::new()),
            streamer_users: Mutex::new(HashMap::new()),
            started: Instant::now(),
            previous_version,
            logs,
            shutdown: Stop::new(),
            running_fingerprint: fp,
            discord_ctl: tokio::sync::Mutex::new(None),
            discord_note: RwLock::new(String::new()),
            discord_http: RwLock::new(None),
            live: Arc::new(RwLock::new(Arc::new(Live::from_config(&cfg_copy)))),
            viewers: Arc::new(Viewers::default()),
            levels: crate::levelshot::Levelshots::new(crate::levelshot::dir(&cfg_copy)),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    pub fn set_cfg(&self, c: Config) {
        *self.live.write().unwrap() = Arc::new(Live::from_config(&c));
        let offered_before = crate::discord::rcon_choices(&self.cfg());
        let c = Arc::new(c);
        *self.cfg.write().unwrap() = c.clone();
        // the `/gtv rcon` command exists in Discord only while it is switched on: add or remove it as soon as that changes
        if offered_before != crate::discord::rcon_choices(&c) {
            if let (Some(http), Ok(rt)) = (self.discord_http(), tokio::runtime::Handle::try_current()) {
                rt.spawn(async move { crate::discord::refresh_commands(&http, &c).await });
            }
        }
    }

    pub fn discord_http(&self) -> Option<Arc<serenity::http::Http>> {
        self.discord_http.read().unwrap().clone()
    }

    pub fn set_discord_http(&self, h: Option<Arc<serenity::http::Http>>) {
        *self.discord_http.write().unwrap() = h;
    }

    pub fn needs_restart(&self) -> bool {
        fingerprint(&self.cfg()) != self.running_fingerprint
    }

    pub fn chat(&self) -> Option<Arc<dyn Chat>> {
        self.chat.read().unwrap().clone()
    }

    pub fn set_chat(&self, c: Option<Arc<dyn Chat>>) {
        if c.is_none() {
            self.set_discord_http(None);
        }
        *self.chat.write().unwrap() = c;
    }

    pub fn streamer(&self, key: &str) -> Option<Arc<Streamer>> {
        self.streamers.lock().unwrap().get(key).cloned()
    }

    // ---- shared spectator streamers (one per server) ----

    /// Open the web-player port of one server (when it has its own). Fails if the port cannot be used.
    async fn open_stream_port(self: &Arc<Self>, key: &str, port: u16) -> Result<(), String> {
        if self.ports.lock().unwrap().get(key).is_some_and(|p| p.port == port) {
            return Ok(());
        }
        self.close_stream_port(key).await;
        let host = self.cfg().web.as_ref().map(|w| w.host.clone()).ok_or("Per-server player ports need the `web` section (Settings tab).")?;
        let listener = crate::web::bind(&host, port).await.map_err(|e| {
            format!("The player port {} for {} cannot be used ({}). Pick another `web_port` for this server, or 0 to share the main one.", port, key, e)
        })?;
        let stop = Stop::new();
        let task = tokio::spawn(crate::web::serve(listener, crate::web::router_for(self.clone(), key), stop.rx()));
        tracing::info!("[{}] its own web player is open on {}:{}", key, host, port);
        self.ports.lock().unwrap().insert(key.to_string(), PortServer { stop, task, port });
        Ok(())
    }

    async fn close_stream_port(&self, key: &str) {
        let p = self.ports.lock().unwrap().remove(key);
        if let Some(mut p) = p {
            p.stop.stop();
            // wait for the socket to be released so the same port can be opened again at once
            if tokio::time::timeout(std::time::Duration::from_secs(3), &mut p.task).await.is_err() {
                p.task.abort();
            }
            tracing::info!("[{}] its own web player (port {}) is closed", key, p.port);
        }
    }

    /// The servers that have a player port of their own open right now: (server key, port).
    pub fn stream_ports(&self) -> Vec<(String, u16)> {
        let mut v: Vec<_> = self.ports.lock().unwrap().iter().map(|(k, p)| (k.clone(), p.port)).collect();
        v.sort();
        v
    }

    // ---- shared spectator streamers (one per server) ----

    async fn acquire_streamer(self: &Arc<Self>, key: &str, delay: u32, fss: &FileSystems) -> Result<Arc<Streamer>, String> {
        let cfg = self.cfg();
        let sc = cfg.servers.get(key).ok_or("Unknown server.")?;
        let sp = sc.spectator.as_ref().ok_or("That server has no `spectator` section in the config.")?;
        let existing = self.streamers.lock().unwrap().get(key).cloned();
        let s = match existing {
            Some(s) => s,
            None => {
                let index = cfg.servers.get_index_of(key).unwrap_or(0) as u32;
                let target = if sp.publish_to.is_empty() { None } else { Some(fss.resolve(&sp.publish_to).map_err(|e| e.to_string())?) };
                // the server's own port first: if it is taken there is no point starting the video
                let own_port = sp.web_port != 0 && (sp.output == "hls" || sp.output == "both");
                if own_port {
                    self.open_stream_port(key, sp.web_port).await?;
                }
                let started = async {
                    let s = Streamer::new(key, &sc.host, sc.port, sp, sp.display_base + index, delay, target).map_err(|e| e.to_string())?;
                    s.start().await.map_err(|e| format!("could not start the video pipeline: {:#}", e))?;
                    Ok::<_, String>(s)
                }.await;
                let s = match started {
                    Ok(s) => s,
                    Err(e) => {
                        self.close_stream_port(key).await;
                        return Err(e);
                    }
                };
                self.streamers.lock().unwrap().insert(key.to_string(), s.clone());
                s
            }
        };
        *self.streamer_users.lock().unwrap().entry(key.to_string()).or_insert(0) += 1;
        Ok(s)
    }

    async fn release_streamer(&self, key: &str) {
        let s = {
            let mut users = self.streamer_users.lock().unwrap();
            let n = users.get(key).copied().unwrap_or(0).saturating_sub(1);
            if n > 0 {
                users.insert(key.to_string(), n);
                return;
            }
            users.remove(key);
            self.streamers.lock().unwrap().remove(key)
        };
        if let Some(s) = s {
            s.stop().await;
        }
        self.close_stream_port(key).await;
    }

    // ---- commands (shared by Discord slash commands and the admin panel) ----

    pub async fn start_watch(self: &Arc<Self>, channel: u64, server: &str, delay: u32, stream: bool) -> Result<String, String> {
        let chat = self.chat().ok_or("The bot is not connected to Discord yet.")?;
        let cfg = self.cfg();
        let sc = cfg.servers.get(server).ok_or("Unknown server.")?.clone();
        if self.watch(channel, server).is_some() {
            return Err(format!("**{}** is already broadcasting in this channel. Use `/gtv stop server:{}` first, or pick another server.", sc.display_name(), server));
        }
        let fss = Arc::new(FileSystems::new(&cfg.filesystems).map_err(|e| e.to_string())?);
        if stream && sc.spectator.is_none() {
            return Err("That server has no `spectator` section in the config.".into());
        }
        let streamer = if stream { Some(self.acquire_streamer(server, delay, &fss).await?) } else { None };
        let deps = WatchDeps { chat, fss, web: WatchDeps::from_web(&cfg.web, cfg.web_token()), live: self.live.clone(), viewers: self.viewers.clone(), levels: self.levels.clone() };
        let watch = Watch::new(deps, channel, sc.clone(), delay, streamer).map_err(|e| e.to_string())?;
        if let Err(e) = watch.start().await {
            if stream {
                self.release_streamer(server).await;
            }
            return Err(format!("I can't post in this channel ({}). I need Send Messages + Embed Links.", e));
        }
        self.watches.lock().unwrap().insert((channel, server.to_string()), watch);
        let mode = if sc.log_path.is_empty() { "query-only mode (scoreboard + score changes)" } else { "with games.log (kill feed, stats)" };
        let extra = if stream { "\n📹 Spectator client starting - video appears in ~20-60 s (longer if it must download the map)." } else { "" };
        let others = self.watches_in(channel).len() - 1;
        let also = if others > 0 { format!("\n(This channel now carries {} broadcasts - add `server:` to `/gtv stop`, `delay`, `stream` and `cam` to pick one.)", others + 1) } else { String::new() };
        Ok(format!("📡 Broadcasting **{}** - {}.{}{}", sc.display_name(), mode, extra, also))
    }

    /// Stop one broadcast: the server named, or the only one in the channel.
    pub async fn stop_watch(&self, channel: u64, server: Option<&str>) -> Result<String, String> {
        let w = self.pick(channel, server)?;
        let key = w.cfg.key.clone();
        self.watches.lock().unwrap().remove(&(channel, key.clone()));
        w.stop().await;
        if w.streamer().is_some() {
            self.release_streamer(&key).await;
        }
        Ok(format!("Stopped **{}**.", w.cfg.display_name()))
    }

    /// Stop every broadcast in a channel.
    pub async fn stop_channel(&self, channel: u64) -> Result<String, String> {
        let keys: Vec<String> = self.watches_in(channel).iter().map(|w| w.cfg.key.clone()).collect();
        if keys.is_empty() {
            return Err("Nothing running here.".into());
        }
        for k in &keys {
            let _ = self.stop_watch(channel, Some(k)).await;
        }
        Ok(format!("Stopped {} broadcast{}.", keys.len(), if keys.len() == 1 { "" } else { "s" }))
    }

    pub async fn stop_all(&self) {
        let ids: Vec<WatchId> = self.watches.lock().unwrap().keys().cloned().collect();
        for (c, k) in ids {
            let _ = self.stop_watch(c, Some(&k)).await;
        }
        let rest: Vec<Arc<Streamer>> = self.streamers.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in rest {
            s.stop().await;
        }
        let ports: Vec<String> = self.ports.lock().unwrap().keys().cloned().collect();
        for k in ports {
            self.close_stream_port(&k).await;
        }
    }

    pub fn watch(&self, channel: u64, server: &str) -> Option<Arc<Watch>> {
        self.watches.lock().unwrap().get(&(channel, server.to_string())).cloned()
    }

    /// Every broadcast running in a channel, by server key.
    pub fn watches_in(&self, channel: u64) -> Vec<Arc<Watch>> {
        let mut v: Vec<Arc<Watch>> = self.watches.lock().unwrap().iter().filter(|((c, _), _)| *c == channel).map(|(_, w)| w.clone()).collect();
        v.sort_by(|a, b| a.cfg.key.cmp(&b.cfg.key));
        v
    }

    /// The broadcast a command means: the server named, or the only one in the channel.
    pub fn pick(&self, channel: u64, server: Option<&str>) -> Result<Arc<Watch>, String> {
        let mut all = self.watches_in(channel);
        match (server.map(str::trim).filter(|s| !s.is_empty()), all.len()) {
            (Some(s), _) => all.into_iter().find(|w| w.cfg.key == s).ok_or_else(|| format!("**{}** is not broadcasting in this channel.", s)),
            (None, 0) => Err("Nothing running here.".into()),
            (None, 1) => Ok(all.remove(0)),
            (None, _) => Err(format!(
                "Several servers are broadcasting in this channel ({}). Add the `server` option to say which.",
                all.iter().map(|w| format!("`{}`", w.cfg.key)).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// Everything that is broadcasting, in every channel.
    pub fn broadcasts(&self) -> Vec<BroadcastInfo> {
        let mut all: Vec<((u64, String), Arc<Watch>)> = self.watches.lock().unwrap().iter().map(|(k, w)| (k.clone(), w.clone())).collect();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all.into_iter().map(|((ch, _), w)| {
            let v = w.viewers();
            BroadcastInfo {
                channel_id: ch.to_string(), server: w.cfg.key.clone(), name: w.cfg.display_name().to_string(), delay: w.delay(),
                video: w.streamer().map(|s| s.is_running()).unwrap_or(false), watch_url: w.watch_url(),
                viewers: v.watching, viewers_open: v.open, web_port: w.cfg.spectator.as_ref().map(|s| s.web_port).unwrap_or(0),
            }
        }).collect()
    }

    pub fn cmd_delay(&self, channel: u64, server: Option<&str>, secs: u32) -> Result<String, String> {
        let w = self.pick(channel, server)?;
        w.set_delay(secs);
        Ok(format!("Delay for **{}** set to {}s.", w.cfg.display_name(), secs))
    }

    pub async fn cmd_stream(self: &Arc<Self>, channel: u64, server: Option<&str>, enable: bool) -> Result<String, String> {
        let w = self.pick(channel, server)?;
        if enable && w.cfg.spectator.is_none() {
            return Err("That server has no spectator section in the config.".into());
        }
        if enable && w.streamer().is_none() {
            let fss = Arc::new(FileSystems::new(&self.cfg().filesystems).map_err(|e| e.to_string())?);
            let s = self.acquire_streamer(&w.cfg.key, w.delay(), &fss).await?;
            w.attach_streamer(Some(s));
            Ok(format!("📹 Spectator client starting for **{}**.", w.cfg.display_name()))
        } else if !enable && w.streamer().is_some() {
            self.release_streamer(&w.cfg.key).await;
            w.attach_streamer(None);
            Ok(format!("Video stream of **{}** stopped.", w.cfg.display_name()))
        } else {
            Ok("Already in that state.".into())
        }
    }

    pub async fn cmd_cam(&self, channel: u64, server: Option<&str>, player: Option<String>, secs: u32) -> Result<String, String> {
        const NONE: &str = "No video stream running here (`/gtv stream enable:True`).";
        let w = self.pick(channel, server).map_err(|e| if e.starts_with("Nothing") { NONE.to_string() } else { e })?;
        let s = w.streamer().ok_or(NONE)?;
        let Some(player) = player else {
            s.unpin();
            return Ok("🎬 Auto director on.".into());
        };
        let Some(r) = s.ref_for(&player) else {
            return Err("Can't target that name (needs games.log for non-ASCII names).".into());
        };
        s.pin(r, secs, &player).await;
        Ok(format!("📹 Following **{}** for {}s.", esc(&player), secs))
    }

    pub fn about_embed(&self) -> Embed {
        let up = self.started.elapsed().as_secs();
        let cfg = self.cfg();
        let fs = FileSystems::new(&cfg.filesystems).map(|f| f.describe().join("\n")).unwrap_or_else(|e| e.to_string());
        let mut e = Embed::new(format!("{} v{}", APP_NAME, VERSION), 0x5865F2)
            .desc(format!("Owned and maintained by **{}**.", OWNER))
            .field("Version", format!("v{} ({})", VERSION, RELEASE_DATE), true)
            .field("Owner", OWNER, true)
            .field("Uptime", format!("{}h {:02}m", up / 3600, up % 3600 / 60), true)
            .field("Runtime", format!("Rust ({} {})", std::env::consts::OS, std::env::consts::ARCH), true)
            .field("Broadcasts", self.watches.lock().unwrap().len().to_string(), true)
            .field("File systems", fs.chars().take(900).collect::<String>(), false);
        if let Some(p) = &self.previous_version {
            e = e.field("Updated", format!("v{} → v{}", p, VERSION), false);
        }
        e.timestamp = true;
        let _ = utc_iso;
        let live = self.live.read().unwrap().clone();
        e.color = live.accent(e.color);
        live.brand(e)
    }

    /// Everything the admin panel's dashboard shows.
    pub fn runtime_state(&self) -> serde_json::Value {
        let watches: Vec<_> = self.broadcasts().into_iter().map(|b| serde_json::json!({
            "channel_id": b.channel_id, "server": b.server, "delay": b.delay, "video": b.video, "watch_url": b.watch_url,
            "viewers": b.viewers, "viewers_open": b.viewers_open, "web_port": b.web_port,
        })).collect();
        let streamers: Vec<_> = self.streamers.lock().unwrap().values().map(|s| {
            let (label, mode) = s.camera();
            serde_json::json!({"server": s.key, "running": s.is_running(), "camera": label, "camera_mode": mode})
        }).collect();
        serde_json::json!({
            "uptime_seconds": self.started.elapsed().as_secs(),
            "discord": *self.discord.read().unwrap(),
            "discord_note": self.discord_note.read().unwrap().clone(),
            "watches": watches,
            "streamers": streamers,
            "player_ports": self.stream_ports().into_iter().map(|(k, p)| serde_json::json!({"server": k, "port": p})).collect::<Vec<_>>(),
            "needs_restart": self.needs_restart(),
            "now": now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerConfig, SpectatorConfig, WebConfig};
    use crate::spectator::Platform;
    use crate::watch::tests::Recorder;

    fn server(key: &str, spectator: Option<SpectatorConfig>) -> ServerConfig {
        // nothing listens on UDP port 1: the status polls just fail quietly, which is all these tests need
        ServerConfig { key: key.into(), name: format!("Server {}", key), host: "127.0.0.1".into(), port: 1, spectator, ..Default::default() }
    }

    fn app_with(cfg: Config) -> (Arc<App>, Arc<Recorder>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("gtv-app-{}", crate::config::random_token(4)));
        let app = App::new(dir.join("config.json"), cfg, LogBuffer::new(), None);
        let rec = Arc::new(Recorder::default());
        app.set_chat(Some(rec.clone()));
        (app, rec, dir)
    }

    fn plain(keys: &[&str]) -> (Arc<App>, Arc<Recorder>) {
        let mut cfg = Config::default();
        for k in keys {
            cfg.servers.insert(k.to_string(), server(k, None));
        }
        let (a, r, _) = app_with(cfg);
        (a, r)
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn a_channel_can_carry_several_servers_and_commands_pick_one() {
        let (app, rec) = plain(&["a", "b"]);
        let first = app.start_watch(7, "a", 0, false).await.unwrap();
        assert!(!first.contains("broadcasts"), "{}", first);
        let second = app.start_watch(7, "b", 0, false).await.unwrap();
        assert!(second.contains("2 broadcasts"), "{}", second);
        assert!(app.start_watch(7, "a", 0, false).await.unwrap_err().contains("already broadcasting"));
        assert_eq!(app.watches_in(7).len(), 2);

        // each server posted its own scoreboard and feed in the same channel
        let sent = rec.sent.lock().unwrap().clone();
        assert_eq!(sent.iter().filter(|(c, ..)| *c == 7).count(), 4);
        let feeds: Vec<String> = sent.iter().map(|(_, e, _)| e.title.clone()).filter(|t| t.contains("Live feed")).collect();
        assert!(feeds.iter().any(|t| t.contains("Server a")) && feeds.iter().any(|t| t.contains("Server b")), "{:?}", feeds);

        // a command that does not name a server is refused while it would be a guess
        let e = app.pick(7, None).err().unwrap();
        assert!(e.contains("Several") && e.contains("`a`") && e.contains("`b`"), "{}", e);
        assert!(app.cmd_delay(7, None, 5).unwrap_err().contains("Several"));
        assert!(app.pick(7, Some("zzz")).err().unwrap().contains("not broadcasting"));
        app.cmd_delay(7, Some("a"), 5).unwrap();
        assert_eq!(app.watch(7, "a").unwrap().delay(), 5);
        assert_eq!(app.watch(7, "b").unwrap().delay(), 0, "the other server is untouched");

        // stopping one leaves the other running
        assert!(app.stop_watch(7, None).await.unwrap_err().contains("Several"));
        assert!(app.stop_watch(7, Some("a")).await.unwrap().contains("Server a"));
        assert_eq!(app.watches_in(7).len(), 1);
        assert_eq!(app.pick(7, None).unwrap().cfg.key, "b", "with one left, no name is needed");
        assert_eq!(app.stop_channel(7).await.unwrap(), "Stopped 1 broadcast.");
        assert!(app.stop_channel(7).await.is_err());
        assert!(app.watches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_server_can_be_shown_in_two_channels_and_stop_all_clears_everything() {
        let (app, _) = plain(&["a", "b"]);
        app.start_watch(1, "a", 0, false).await.unwrap();
        app.start_watch(2, "a", 0, false).await.unwrap();
        app.start_watch(2, "b", 0, false).await.unwrap();
        let list = app.broadcasts();
        assert_eq!(list.len(), 3);
        assert_eq!(list.iter().map(|b| (b.channel_id.as_str(), b.server.as_str())).collect::<Vec<_>>(), [("1", "a"), ("2", "a"), ("2", "b")]);
        let state = app.runtime_state();
        assert_eq!(state["watches"].as_array().unwrap().len(), 3);
        assert_eq!(state["watches"][0]["web_port"], 0);
        app.stop_watch(1, None).await.unwrap();
        assert!(app.watch(2, "a").is_some() && app.watch(2, "b").is_some());
        app.stop_all().await;
        assert!(app.watches.lock().unwrap().is_empty());
    }

    /// One raw HTTP request; (status, headers, body).
    async fn get(port: u16, path: &str) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", path).as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (head.split_whitespace().nth(1).unwrap().parse().unwrap(), head.to_string(), body.to_string())
    }

    fn with_test_streamer(app: &App, key: &str, work: &std::path::Path) {
        let sp = SpectatorConfig { work_dir: work.to_string_lossy().into_owned(), ..Default::default() };
        let s = Streamer::with_platform(key, "h", 1, &sp, 90, 0, None, Platform::Linux).unwrap();
        s.mark_running_for_test();
        app.streamers.lock().unwrap().insert(key.into(), s);
    }

    #[tokio::test]
    async fn every_server_streams_on_its_own_port_and_only_shows_its_own_video() {
        let (pa, pb) = (free_port(), free_port());
        let mut cfg = Config::default();
        cfg.web = Some(WebConfig { host: "127.0.0.1".into(), port: free_port(), ..Default::default() });
        for (k, p) in [("a", pa), ("b", pb)] {
            cfg.servers.insert(k.into(), server(k, Some(SpectatorConfig { web_port: p, ..Default::default() })));
        }
        let (app, _, dir) = app_with(cfg);
        with_test_streamer(&app, "a", &dir);
        with_test_streamer(&app, "b", &dir);
        app.open_stream_port("a", pa).await.unwrap();
        app.open_stream_port("b", pb).await.unwrap();
        assert_eq!(app.stream_ports(), vec![("a".to_string(), pa), ("b".to_string(), pb)]);

        // port A: its own player, its own status; nothing of B
        assert_eq!(get(pa, "/watch/a").await.0, 200);
        assert_eq!(get(pa, "/status/a").await.0, 200);
        assert_eq!(get(pa, "/healthz").await.0, 200);
        for path in ["/watch/b", "/status/b", "/hls/b/live.m3u8"] {
            assert_eq!(get(pa, path).await.0, 404, "port A must not serve {}", path);
        }
        // the bare address goes straight to this port's own server (the shared player would list both)
        let (code, head, _) = get(pa, "/").await;
        assert!(code == 302 && head.to_lowercase().contains("location: /watch/a"), "{}", head);
        let (code, head, _) = get(pb, "/").await;
        assert!(code == 302 && head.to_lowercase().contains("location: /watch/b"), "{}", head);
        assert_eq!(get(pb, "/watch/b").await.0, 200);
        assert_eq!(get(pb, "/watch/a").await.0, 404);

        // closing a server's port frees it at once, and it can be opened again
        app.close_stream_port("a").await;
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", pa)).await.is_err(), "port A should be closed");
        assert_eq!(get(pb, "/watch/b").await.0, 200, "closing A does not touch B");
        app.open_stream_port("a", pa).await.unwrap();
        assert_eq!(get(pa, "/watch/a").await.0, 200);
        app.stop_all().await;
        assert!(app.stream_ports().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_port_that_is_taken_stops_the_video_before_anything_is_started() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap().port();
        let mut cfg = Config::default();
        cfg.web = Some(WebConfig { host: "127.0.0.1".into(), port: free_port(), ..Default::default() });
        // a client that does not exist: if the pipeline were started anyway, that would be the error instead
        let sp = SpectatorConfig { web_port: taken, client_binary: "/nonexistent/client".into(), ..Default::default() };
        cfg.servers.insert("a".into(), server("a", Some(sp)));
        let (app, _, _) = app_with(cfg);
        let e = app.start_watch(9, "a", 0, true).await.unwrap_err();
        assert!(e.contains(&taken.to_string()) && e.contains("cannot be used"), "{}", e);
        assert!(app.streamers.lock().unwrap().is_empty() && app.watches.lock().unwrap().is_empty());
        assert!(app.stream_ports().is_empty());

        // no web section: a clear message instead of a silent failure
        let mut cfg = Config::default();
        cfg.servers.insert("a".into(), server("a", Some(SpectatorConfig { web_port: 9999, ..Default::default() })));
        let (app, _, _) = app_with(cfg);
        assert!(app.open_stream_port("a", 9999).await.unwrap_err().contains("`web` section"));
    }

    #[cfg(unix)]
    fn fake(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{}", body)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// The whole path a Discord admin takes - start with video, switch it off and on, stop - for two servers at the same
    /// time in one channel, with stand-in game client / ffmpeg programs (so no game, display or Discord is needed).
    #[cfg(unix)]
    #[tokio::test]
    async fn two_servers_stream_at_once_each_on_its_own_port() {
        let t = std::env::temp_dir().join(format!("gtv-two-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&t).unwrap();
        let (pa, pb) = (free_port(), free_port());
        let mut cfg = Config::default();
        cfg.web = Some(WebConfig { host: "127.0.0.1".into(), port: free_port(), public_url: "http://play.example:8080".into(), token: "tok".into() });
        for (k, p) in [("a", pa), ("b", pb)] {
            let sp = SpectatorConfig {
                client_binary: fake(&t, &format!("client_{}", k), "exec sleep 300\n"),
                ffmpeg_binary: fake(&t, &format!("ffmpeg_{}", k), "exec sleep 300\n"),
                xdotool_binary: fake(&t, &format!("xdotool_{}", k), "exit 0\n"),
                base_path: t.to_string_lossy().into_owned(), work_dir: t.join("work").to_string_lossy().into_owned(),
                manage_display: false, display: ":99".into(), web_port: p, ..Default::default()
            };
            cfg.servers.insert(k.into(), server(k, Some(sp)));
        }
        let (app, _, _) = app_with(cfg);
        let ch = 5;

        app.start_watch(ch, "a", 0, true).await.unwrap();
        app.start_watch(ch, "b", 0, true).await.unwrap();
        assert_eq!(app.stream_ports(), vec![("a".to_string(), pa), ("b".to_string(), pb)]);
        // each broadcast links to its own server's port
        assert_eq!(app.watch(ch, "a").unwrap().watch_url().unwrap(), format!("http://play.example:{}/watch/a?t=tok", pa));
        assert_eq!(app.watch(ch, "b").unwrap().watch_url().unwrap(), format!("http://play.example:{}/watch/b?t=tok", pb));
        // two separate pipelines, each with its own game folder
        let (sa, sb) = (app.streamer("a").unwrap(), app.streamer("b").unwrap());
        assert!(sa.is_running() && sb.is_running() && sa.home_dir() != sb.home_dir());
        assert_eq!(get(pa, "/watch/a?t=tok").await.0, 200, "port A serves A (200) ...");
        assert_eq!(get(pa, "/watch/b?t=tok").await.0, 404, "... and not B");
        assert_eq!(get(pb, "/watch/b?t=tok").await.0, 200);

        // the video of one server can be switched off and on without touching the other
        assert!(app.cmd_stream(ch, None, false).await.unwrap_err().contains("Several"));
        app.cmd_stream(ch, Some("a"), false).await.unwrap();
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", pa)).await.is_err(), "A's port is closed with its video");
        assert!(app.streamer("a").is_none() && app.streamer("b").unwrap().is_running());
        assert_eq!(get(pb, "/watch/b?t=tok").await.0, 200);
        app.cmd_stream(ch, Some("a"), true).await.unwrap();
        assert_eq!(get(pa, "/watch/a?t=tok").await.0, 200, "the same port opens again at once");

        // stopping one broadcast stops that server's video and port only
        app.stop_watch(ch, Some("b")).await.unwrap();
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", pb)).await.is_err());
        assert_eq!(get(pa, "/watch/a?t=tok").await.0, 200);
        assert!(app.watch(ch, "a").is_some());
        app.stop_all().await;
        assert!(app.stream_ports().is_empty() && app.streamers.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(t);
    }
}
