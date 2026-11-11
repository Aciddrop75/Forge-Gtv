// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! The broadcast engine: one `Watch` = one Discord channel following one UrT server.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::task::JoinHandle;

use crate::brand::Live;
use crate::config::{ServerConfig, WebConfig};
use crate::embed::{Chat, ChatError, Components, Embed};
use crate::fs::{Fs, FileSystems, tail_file};
use crate::spectator::Streamer;
use crate::urt::{self, Event, Kind, LogParser, PlayerStats, Snapshot, esc, gametype_name, strip_colors};
use crate::util::{Stop, StopRx, utc_iso, utc_stamp};
use crate::version::{OWNER, VERSION};
use crate::viewers::Viewers;
use std::sync::RwLock;

pub const FEED_LINES: usize = 14;
pub const BOARD_EVERY: f64 = 5.0;
pub const FEED_EVERY: f64 = 3.0;

#[derive(Clone)]
pub struct WatchDeps {
    pub chat: Arc<dyn Chat>,
    pub fss: Arc<FileSystems>,
    /// (public_url, token) of the web player, if one is configured.
    pub web: Option<(String, String)>,
    /// Clan branding and post options, read at every redraw so panel changes apply without a restart.
    pub live: Arc<RwLock<Arc<Live>>>,
    /// Who has the web player open.
    pub viewers: Arc<Viewers>,
    /// The map pictures shown on the scoreboard.
    pub levels: Arc<crate::levelshot::Levelshots>,
}

impl WatchDeps {
    pub fn from_web(web: &Option<WebConfig>, token: String) -> Option<(String, String)> {
        web.as_ref().map(|w| (crate::config::fix_public_url(&w.public_url).unwrap_or_else(|_| w.public_url.clone()), token))
    }

    #[cfg(test)]
    pub fn plain(chat: Arc<dyn Chat>, fss: Arc<FileSystems>) -> WatchDeps {
        WatchDeps { chat, fss, web: None, live: Arc::new(RwLock::new(Arc::new(Live { each_map: true, ..Default::default() }))), viewers: Arc::new(Viewers::default()),
            levels: crate::levelshot::Levelshots::new(std::env::temp_dir().join("gtv-test-levelshots")) }
    }
}

struct State {
    parser: Option<LogParser>,
    pending: VecDeque<Event>,
    snaps: VecDeque<Snapshot>,
    feed: VecDeque<String>,
    stats: HashMap<String, PlayerStats>,
    board_id: Option<u64>,
    feed_id: Option<u64>,
    feed_dirty: bool,
    names: Vec<String>,
    last_relay: HashMap<u64, f64>,
    /// The live scoreboard as last drawn - what the post is closed with when the map changes.
    last_board: Option<Embed>,
    /// A new map started: the old posts are closed and fresh ones made (see `rotate`).
    retired: Option<Retired>,
    posted_at: f64,
    peak_viewers: u32,
    /// The map name of the match in progress (for "finished" titles).
    map_now: String,
}

/// What the previous map's posts looked like at the end, kept until the new posts are made.
struct Retired {
    board: Option<Embed>,
    feed: Vec<String>,
    map: String,
}

pub struct Watch {
    pub channel_id: u64,
    pub cfg: ServerConfig,
    deps: WatchDeps,
    delay: AtomicU32,
    st: Mutex<State>,
    streamer: Mutex<Option<Arc<Streamer>>>,
    stop: Stop,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    hide: String,
    archive: Option<(Arc<dyn Fs>, String)>,
}

fn footer() -> String {
    crate::version::credit()
}

impl Watch {
    pub fn new(deps: WatchDeps, channel_id: u64, cfg: ServerConfig, delay: u32, streamer: Option<Arc<Streamer>>) -> Result<Arc<Watch>, anyhow::Error> {
        let hide = cfg.spectator.as_ref().map(|s| strip_colors(&s.client_name).to_lowercase()).unwrap_or_default();
        let archive = if cfg.archive_dir.is_empty() { None } else { Some(deps.fss.resolve(&cfg.archive_dir)?) };
        let parser = if cfg.log_path.is_empty() { None } else { Some(LogParser::new(cfg.show_joins)) };
        let w = Arc::new(Watch {
            channel_id,
            cfg,
            deps,
            delay: AtomicU32::new(delay),
            st: Mutex::new(State {
                parser,
                pending: VecDeque::new(),
                snaps: VecDeque::with_capacity(500),
                feed: VecDeque::new(),
                stats: HashMap::new(),
                board_id: None,
                feed_id: None,
                feed_dirty: true,
                names: vec![],
                last_relay: HashMap::new(),
                last_board: None,
                retired: None,
                posted_at: urt::now(),
                peak_viewers: 0,
                map_now: String::new(),
            }),
            streamer: Mutex::new(None),
            stop: Stop::new(),
            tasks: Mutex::new(vec![]),
            hide,
            archive,
        });
        w.attach_streamer(streamer);
        Ok(w)
    }

    pub fn delay(&self) -> u32 {
        self.delay.load(Ordering::Relaxed)
    }

    /// Clan branding and post options as they are right now.
    fn live(&self) -> Arc<Live> {
        self.deps.live.read().unwrap().clone()
    }

    /// How many people have this broadcast's web player open.
    pub fn viewers(&self) -> crate::viewers::Counts {
        self.deps.viewers.counts(&self.cfg.key)
    }

    pub fn set_delay(&self, d: u32) {
        self.delay.store(d, Ordering::Relaxed);
        if let Some(s) = self.streamer() {
            s.set_delay(d);
        }
    }

    pub fn streamer(&self) -> Option<Arc<Streamer>> {
        self.streamer.lock().unwrap().clone()
    }

    pub fn attach_streamer(self: &Arc<Self>, s: Option<Arc<Streamer>>) {
        if let Some(st) = &s {
            let (a, b) = (Arc::downgrade(self), Arc::downgrade(self));
            st.set_lookups(
                Box::new(move |n| a.upgrade().and_then(|w| w.slot_for(n))),
                Box::new(move |slot| b.upgrade().and_then(|w| w.name_of(slot))),
            );
        }
        *self.streamer.lock().unwrap() = s;
    }

    pub fn slot_for(&self, name: &str) -> Option<i32> {
        self.st.lock().unwrap().parser.as_ref().and_then(|p| p.slot_for(name))
    }

    pub fn name_of(&self, slot: i32) -> Option<String> {
        self.st.lock().unwrap().parser.as_ref().and_then(|p| p.slots.get(&slot).cloned())
    }

    pub fn player_names(&self) -> Vec<String> {
        self.st.lock().unwrap().names.clone()
    }

    pub fn watch_url(&self) -> Option<String> {
        let s = self.streamer()?;
        if !s.is_running() {
            return None;
        }
        if s.wants_web() {
            if let Some((public, token)) = &self.deps.web {
                // a server with its own player port (or address) gets a link to that
                let base = match &self.cfg.spectator {
                    Some(sp) => crate::config::stream_base(public, sp),
                    None => public.clone(),
                };
                if !base.is_empty() {
                    let url = format!("{}/watch/{}", base.trim_end_matches('/'), self.cfg.key);
                    return Some(if token.is_empty() { url } else { format!("{}?t={}", url, token) });
                }
            }
        }
        let w = s.watch_url_override();
        if w.is_empty() { None } else { Some(w) }
    }

    // ---- lifecycle ----

    pub async fn start(self: &Arc<Self>) -> Result<(), ChatError> {
        self.post_pair().await?;
        let mut tasks = self.tasks.lock().unwrap();
        tasks.push(tokio::spawn(self.clone().poll_loop(self.stop.rx())));
        tasks.push(tokio::spawn(self.clone().display_loop(self.stop.rx())));
        if !self.cfg.log_path.is_empty() {
            match self.deps.fss.resolve(&self.cfg.log_path) {
                Ok((fs, path)) => {
                    let me = self.clone();
                    tasks.push(tokio::spawn(tail_file(fs, path, move |l| me.on_log_line(&l), self.stop.rx())));
                }
                Err(e) => tracing::error!("[{}] cannot follow games.log: {}", self.cfg.key, e),
            }
        }
        Ok(())
    }

    /// Post a fresh scoreboard and live feed at the bottom of the channel and remember them as the current ones.
    async fn post_pair(&self) -> Result<(), ChatError> {
        let (board, _) = self.build_board(urt::now());
        let bid = self.deps.chat.send(self.channel_id, board, None).await?;
        let fid = self.deps.chat.send(self.channel_id, self.build_feed(), None).await?;
        let mut st = self.st.lock().unwrap();
        st.board_id = Some(bid);
        st.feed_id = Some(fid);
        st.feed_dirty = false;
        st.posted_at = urt::now();
        st.peak_viewers = 0;
        Ok(())
    }

    /// The map changed: close the old scoreboard and feed with their final state (or remove them) and start new ones,
    /// so each map has its own clean post at the bottom of the channel.
    async fn rotate(&self, old: Retired) {
        let (old_board, old_feed) = { let st = self.st.lock().unwrap(); (st.board_id, st.feed_id) };
        if let Err(e) = self.post_pair().await {
            // can't post: keep using the old ones rather than lose the broadcast
            tracing::warn!("[{}] could not start the post for the new map: {}", self.cfg.key, e);
            return;
        }
        let live = self.live();
        if live.delete_old {
            for id in [old_board, old_feed].into_iter().flatten() {
                match self.deps.chat.delete(self.channel_id, id).await {
                    Ok(()) | Err(ChatError::NotFound) => {}
                    Err(e) => tracing::warn!("[{}] could not remove the old post: {}", self.cfg.key, e),
                }
            }
            return;
        }
        if let (Some(id), Some(mut e)) = (old_board, old.board) {
            if e.title.starts_with("📡 LIVE") {
                e.title = e.title.replacen("📡 LIVE", "🏁 FINISHED", 1);
                e.timestamp = false;
                e.description.push_str("\n\n*That map is over - the live board is the newest post below.*");
            }
            let _ = self.deps.chat.edit(self.channel_id, id, e, Some(Components::default())).await;
        }
        if let Some(id) = old_feed {
            let name = esc(self.cfg.display_name());
            let title = if old.map.is_empty() { format!("📺 Feed · {} · map finished", name) } else { format!("📺 Feed · {} · {} (finished)", name, esc(&old.map)) };
            let text = if old.feed.is_empty() { "*Nothing happened.*".to_string() } else { old.feed.join("\n") };
            let e = live.brand(Embed::new(title, 0x95A5A6).desc(text));
            let _ = self.deps.chat.edit(self.channel_id, id, e, Some(Components::default())).await;
        }
    }

    pub async fn stop(&self) {
        self.stop.stop();
        let tasks: Vec<_> = self.tasks.lock().unwrap().drain(..).collect();
        for t in tasks {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), t).await;
        }
        self.deps.viewers.clear(&self.cfg.key);
        let bid = self.st.lock().unwrap().board_id;
        if let Some(bid) = bid {
            let e = self.live().brand(Embed::new("⏹️ Broadcast ended", 0x95A5A6));
            if let Err(err) = self.deps.chat.edit(self.channel_id, bid, e, Some(Components::default())).await {
                tracing::warn!("could not mark broadcast ended: {}", err);
            }
        }
    }

    // ---- data collection ----

    pub fn on_log_line(&self, line: &str) {
        if line.contains("InitGame:") {
            if let Some(s) = self.streamer() {
                s.map_loaded();
            }
        }
        let events = {
            let mut st = self.st.lock().unwrap();
            match st.parser.as_mut() {
                Some(p) => p.feed(line),
                None => return,
            }
        };
        let streamer = self.streamer();
        let mut st = self.st.lock().unwrap();
        for ev in events {
            if !self.hide.is_empty() && ev.data.name.as_deref().map(|n| n.to_lowercase()) == Some(self.hide.clone()) {
                continue;
            }
            if let Some(s) = &streamer {
                s.director_event(&ev);
            }
            st.pending.push_back(ev);
        }
    }

    async fn poll_loop(self: Arc<Self>, mut stop: StopRx) {
        let mut prev: Option<Snapshot> = None;
        while !stop.is_stopped() {
            let snap = urt::query_status(&self.cfg.host, self.cfg.port).await;
            if let Some(s) = self.streamer() {
                s.observe(&snap);
                if !self.hide.is_empty() {
                    let team = self.st.lock().unwrap().parser.as_ref().and_then(|p| p.teams.get(&self.hide).cloned());
                    s.set_own_team(team);
                }
            }
            {
                let mut st = self.st.lock().unwrap();
                if st.snaps.len() >= 500 {
                    st.snaps.pop_front();
                }
                st.snaps.push_back(snap.clone());
                if st.parser.is_none() && snap.online {
                    if let Some(p) = prev.as_ref().filter(|p| p.online) {
                        if p.mapname != snap.mapname {
                            st.pending.push_back(Event::new(snap.ts, Kind::Map, format!("🗺️ New map: **{}**", esc(&snap.mapname))));
                        } else {
                            let before: HashMap<&str, i64> = p.players.iter().map(|x| (x.name.as_str(), x.score)).collect();
                            for pl in &snap.players {
                                if let Some(b) = before.get(pl.name.as_str()) {
                                    if pl.score > *b {
                                        st.pending.push_back(Event::new(snap.ts, Kind::Score, format!("➕ **{}** scores ({} → {})", esc(&pl.name), b, pl.score)));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            prev = Some(snap);
            if stop.sleep_secs(self.cfg.poll_interval).await {
                return;
            }
        }
    }

    fn snapshot_at(st: &State, t: f64) -> Option<&Snapshot> {
        st.snaps.iter().take_while(|s| s.ts <= t).last()
    }

    fn apply(&self, st: &mut State, ev: &Event) -> bool {
        match ev.kind {
            Kind::Map => {
                st.stats.clear();
                // nothing has happened on the current posts yet (the bot just joined) -> keep using them
                let used = !st.feed.is_empty() || urt::now() - st.posted_at > 60.0;
                if self.live().each_map && used && st.retired.is_none() {
                    st.retired = Some(Retired { board: st.last_board.clone(), feed: st.feed.iter().cloned().collect(), map: st.map_now.clone() });
                    st.feed.clear();
                }
            }
            Kind::Kill => {
                if let Some(v) = &ev.data.victim {
                    let s = st.stats.entry(v.to_lowercase()).or_insert_with(|| PlayerStats::new(v));
                    s.deaths += 1;
                    s.streak = 0;
                }
                if let Some(k) = &ev.data.killer {
                    let s = st.stats.entry(k.to_lowercase()).or_insert_with(|| PlayerStats::new(k));
                    s.kills += 1;
                    s.streak += 1;
                    s.best_streak = s.best_streak.max(s.streak);
                    if let Some(w) = &ev.data.weapon {
                        *s.weapons.entry(w.clone()).or_insert(0) += 1;
                    }
                    if ev.data.hs {
                        s.headshots += 1;
                    }
                }
            }
            _ => {}
        }
        if st.feed.len() >= FEED_LINES {
            st.feed.pop_front();
        }
        st.feed.push_back(ev.text.clone());
        st.feed_dirty = true;
        ev.kind == Kind::Exit
    }

    fn top_fraggers(st: &State) -> Vec<PlayerStats> {
        let mut top: Vec<PlayerStats> = st.stats.values().cloned().collect();
        top.sort_by(|a, b| b.kills.cmp(&a.kills).then(a.deaths.cmp(&b.deaths)).then(a.name.cmp(&b.name)));
        top
    }

    pub fn summary_embed(top: &[PlayerStats], peak_viewers: u32, server: &str) -> Option<Embed> {
        if top.is_empty() {
            return None;
        }
        let medals = ["🥇", "🥈", "🥉"];
        let lines: Vec<String> = top.iter().take(3).enumerate().map(|(i, s)| {
            format!("{} **{}** — {} kills / {} deaths · {} HS · best streak {}", medals[i], esc(&s.name), s.kills, s.deaths, s.headshots, s.best_streak)
        }).collect();
        let mut e = Embed::new("🏁 Match finished — top fraggers", 0xF1C40F).desc(lines.join("\n")).field("Server", esc(server), true);
        if peak_viewers > 0 {
            e = e.field("👁 Peak viewers", peak_viewers.to_string(), true);
        }
        Some(e)
    }

    async fn match_finished(self: &Arc<Self>) {
        let (top, peak) = { let st = self.st.lock().unwrap(); (Self::top_fraggers(&st), st.peak_viewers) };
        let Some(embed) = Self::summary_embed(&top, peak, self.cfg.display_name()) else { return };
        let embed = self.live().brand(embed);
        if let Err(e) = self.deps.chat.send(self.channel_id, embed, None).await {
            tracing::warn!("summary post failed: {}", e);
        }
        if let Some((fs, base)) = self.archive.clone() {
            let map = { Self::snapshot_at(&self.st.lock().unwrap(), urt::now()).map(|s| s.mapname.clone()) };
            let doc = json!({
                "bot_version": VERSION, "owner": OWNER, "server": self.cfg.key, "written": utc_iso(), "map": map, "peak_viewers": peak,
                "players": top.iter().map(|s| json!({
                    "name": s.name, "kills": s.kills, "deaths": s.deaths, "headshots": s.headshots,
                    "best_streak": s.best_streak, "weapons": s.weapons,
                })).collect::<Vec<_>>(),
            });
            let name = format!("{}-{}-{}.json", self.cfg.key, utc_stamp(), map.unwrap_or_else(|| "map".into()));
            let path = fs.join(&base, &name);
            match fs.write_bytes(&path, serde_json::to_string_pretty(&doc).unwrap_or_default().as_bytes()).await {
                Ok(()) => tracing::info!("archived match to {} ({})", path, fs.label()),
                Err(e) => tracing::warn!("archive failed: {}", e),
            }
        }
    }

    // ---- rendering ----

    pub fn build_board(&self, now: f64) -> (Embed, Vec<String>) {
        let (e, names) = self.board_inner(now);
        (self.live().brand(e), names)
    }

    fn board_inner(&self, now: f64) -> (Embed, Vec<String>) {
        let live = self.live();
        let st = self.st.lock().unwrap();
        let delay = self.delay();
        let delay_txt = if delay > 0 { format!(" · ⏱ {}s delay", delay) } else { String::new() };
        let snap = Self::snapshot_at(&st, now - delay as f64);
        let Some(snap) = snap else {
            return (Embed::new("⏳ Connecting…", 0x95A5A6).footer(footer())
                .desc(format!("Waiting for **{}**{}", esc(self.cfg.display_name()), delay_txt)), vec![]);
        };
        if !snap.online {
            return (Embed::new(format!("🔌 {} is not responding", self.cfg.display_name()), 0x95A5A6).desc("Retrying…").footer(footer()), vec![]);
        }
        let players: Vec<_> = snap.players.iter().filter(|p| p.name.to_lowercase() != self.hide).collect();
        let mut desc = format!("🗺️ **{}** · {} · 👥 {}/{}{}", esc(&snap.mapname), gametype_name(snap.gametype), players.len(), snap.max_clients, delay_txt);
        if let Some(s) = self.streamer() {
            let (label, mode) = s.camera();
            if s.is_running() && !label.is_empty() {
                desc.push_str(&format!("\n📹 Camera: **{}** ({})", esc(&label), mode));
            }
            if s.is_running() {
                let n = self.viewers().watching;
                desc.push_str(&format!("\n👁 **{}** watching the stream", n));
            }
        }
        let mut color = live.accent(0x2ECC71);
        if let (Some(r), Some(b)) = (snap.red, snap.blue) {
            desc.push_str(&format!("\n🔴 **{}**  –  **{}** 🔵", r, b));
            color = if r > b { 0xE74C3C } else if b > r { 0x3498DB } else { 0x95A5A6 };
        }
        let empty = HashMap::new();
        let teams = st.parser.as_ref().map(|p| &p.teams).unwrap_or(&empty);
        let mut groups: HashMap<&str, Vec<&urt::Player>> = HashMap::new();
        let mut sorted = players.clone();
        sorted.sort_by(|a, b| b.score.cmp(&a.score));
        for p in sorted {
            let t = teams.get(&p.name.to_lowercase()).map(String::as_str).unwrap_or("free");
            let key = match t { "red" => "red", "blue" => "blue", "spec" => "spec", _ => "free" };
            groups.entry(key).or_default().push(p);
        }
        let has_teams = ["red", "blue", "spec"].iter().any(|k| groups.get(k).is_some_and(|v| !v.is_empty()));
        let mut emb = Embed::new(format!("📡 LIVE · {}", if snap.hostname.is_empty() { self.cfg.display_name() } else { &snap.hostname }), color);
        for key in ["red", "blue", "free", "spec"] {
            let Some(plist) = groups.get(key).filter(|v| !v.is_empty()) else { continue };
            let label = match key {
                "red" => "🔴 Red",
                "blue" => "🔵 Blue",
                "free" => if has_teams { "⚪ Free" } else { "Players" },
                _ => "👁️ Spectators",
            };
            if key == "spec" {
                let names: Vec<&str> = plist.iter().map(|p| p.name.as_str()).collect();
                emb = emb.field(label, esc(&names.join(", ")).chars().take(1000).collect::<String>(), false);
                continue;
            }
            let rows: Vec<String> = plist.iter().take(20).map(|p| {
                let kd = st.stats.get(&p.name.to_lowercase()).map(|s| format!("{}/{}", s.kills, s.deaths)).unwrap_or_else(|| "-".into());
                let name: String = p.name.chars().take(17).collect();
                format!("{:<17} {:>3} {:>6} {:>4}ms", name, p.score, kd, p.ping)
            }).collect();
            let header = format!("{:<17} {:>3} {:>6} {:>6}", "Player", "Pts", "K/D", "Ping");
            emb = emb.field(label, format!("```\n{}\n{}\n```", header, rows.join("\n")), false);
        }
        emb.description = desc;
        emb.timestamp = true;
        if let Some(url) = self.levelshot_url(&live, &snap.mapname) {
            emb.image = url;
        }
        let mut names: Vec<String> = Vec::new();
        for p in &players {
            if !p.name.is_empty() && !names.contains(&p.name) {
                names.push(p.name.clone());
            }
        }
        (emb, names)
    }

    /// The address of the current map's picture, once it has been found and made (the first redraw after a map change
    /// starts the search; the next one shows it). None when pictures are off, there is no public address, or no picture.
    fn levelshot_url(&self, live: &Live, map: &str) -> Option<String> {
        if !live.level_on || map.is_empty() {
            return None;
        }
        let name = self.deps.levels.lookup(&self.cfg.levelshot_sources(), map, live.level_size, live.level_fit)?;
        Some(format!("{}/levelshot/{}", live.public_url.trim_end_matches('/'), name))
    }

    pub fn build_feed(&self) -> Embed {
        let live = self.live();
        let st = self.st.lock().unwrap();
        let text = st.feed.iter().cloned().collect::<Vec<_>>().join("\n");
        live.brand(Embed::new(format!("📺 Live feed · {}", esc(self.cfg.display_name())), live.accent(0x5865F2)).desc(if text.is_empty() { "*Waiting for action…*".to_string() } else { text }))
    }

    pub fn components(&self, names: &[String]) -> Components {
        Components {
            player_options: names.iter().take(25).map(|n| n.chars().take(100).collect()).collect(),
            watch_url: self.watch_url(),
            server: self.cfg.key.clone(),
        }
    }

    pub fn player_card(&self, name: &str) -> Embed {
        self.live().brand(self.player_card_inner(name))
    }

    fn player_card_inner(&self, name: &str) -> Embed {
        let accent = self.live().accent(0x5865F2);
        let st = self.st.lock().unwrap();
        let mut emb = Embed::new(format!("🎯 {}", name), accent);
        if st.parser.is_none() {
            emb.description = "Detailed stats need `log_path` (games.log) in the bot config.".into();
            return emb;
        }
        let Some(s) = st.stats.get(&name.to_lowercase()) else {
            emb.description = "No frags recorded yet on this map.".into();
            return emb;
        };
        let kd = if s.deaths > 0 { s.kills as f64 / s.deaths as f64 } else { s.kills as f64 };
        for (label, val) in [("Kills", s.kills.to_string()), ("Deaths", s.deaths.to_string()), ("K/D", format!("{:.2}", kd)),
                             ("Headshots", s.headshots.to_string()), ("Streak", s.streak.to_string()), ("Best streak", s.best_streak.to_string())] {
            emb = emb.field(label, val, true);
        }
        let top = s.top_weapons(3);
        if !top.is_empty() {
            emb = emb.field("Top weapons", top.iter().map(|(w, n)| format!("{} ×{}", w, n)).collect::<Vec<_>>().join("\n"), false);
        }
        emb
    }

    async fn edit(&self, which_board: bool, embed: Embed, comps: Option<Components>) {
        let id = { let st = self.st.lock().unwrap(); if which_board { st.board_id } else { st.feed_id } };
        let Some(id) = id else { return };
        match self.deps.chat.edit(self.channel_id, id, embed.clone(), comps.clone()).await {
            Ok(()) => {}
            Err(ChatError::NotFound) => {
                // the message was deleted: post a fresh one
                if let Ok(new_id) = self.deps.chat.send(self.channel_id, embed, comps).await {
                    let mut st = self.st.lock().unwrap();
                    if which_board { st.board_id = Some(new_id) } else { st.feed_id = Some(new_id) }
                }
            }
            Err(e) => tracing::warn!("edit failed: {}", e),
        }
    }

    /// Apply every event that is older than the delay. Returns true if the match ended.
    pub fn flush_due(&self, now: f64) -> bool {
        let cutoff = now - self.delay() as f64;
        let mut st = self.st.lock().unwrap();
        let mut ended = false;
        while st.pending.front().is_some_and(|e| e.ts <= cutoff) {
            let ev = st.pending.pop_front().unwrap();
            ended |= self.apply(&mut st, &ev);
        }
        ended
    }

    async fn display_loop(self: Arc<Self>, mut stop: StopRx) {
        let (mut last_board, mut last_feed) = (0.0f64, 0.0f64);
        while !stop.sleep_secs(0.5).await {
            let now = urt::now();
            if self.flush_due(now) {
                self.match_finished().await;
            }
            let retired = self.st.lock().unwrap().retired.take();
            if let Some(r) = retired {
                self.rotate(r).await;
                last_board = 0.0; // draw the new post (with its Watch button) at once
                last_feed = 0.0;
            }
            if now - last_board >= BOARD_EVERY {
                last_board = now;
                let (emb, names) = self.build_board(now);
                {
                    let mut st = self.st.lock().unwrap();
                    st.names = names.clone();
                    if emb.title.starts_with("📡 LIVE") {
                        st.last_board = Some(emb.clone());
                    }
                    let delay = self.delay() as f64;
                    if let Some(m) = Self::snapshot_at(&st, now - delay).filter(|s| s.online).map(|s| s.mapname.clone()) {
                        st.map_now = m;
                    }
                    let watching = self.viewers().watching;
                    st.peak_viewers = st.peak_viewers.max(watching);
                }
                let comps = self.components(&names);
                self.edit(true, emb, Some(comps)).await;
            }
            let dirty = self.st.lock().unwrap().feed_dirty;
            if dirty && now - last_feed >= FEED_EVERY {
                last_feed = now;
                self.st.lock().unwrap().feed_dirty = false;
                self.edit(false, self.build_feed(), None).await;
            }
        }
    }

    // ---- Discord -> game chat relay ----

    pub fn relay(self: &Arc<Self>, author_id: u64, author_name: &str, content: &str) -> bool {
        let c = &self.cfg;
        if !(c.relay_chat && !c.rcon_password.is_empty() && !content.is_empty()) {
            return false;
        }
        let now = urt::now();
        {
            let mut st = self.st.lock().unwrap();
            if now - st.last_relay.get(&author_id).copied().unwrap_or(0.0) < 2.0 {
                return false;
            }
            st.last_relay.insert(author_id, now);
        }
        let clean = |s: &str| -> String { s.chars().filter(|c| (' '..='~').contains(c) && !";\"\\%$^~`".contains(*c)).collect() };
        let text: String = clean(content).chars().take(110).collect::<String>().trim().to_string();
        let mut author: String = clean(author_name).chars().take(20).collect();
        if author.is_empty() {
            author = "user".into();
        }
        if text.is_empty() {
            return false;
        }
        let (host, port, pw) = (c.host.clone(), c.port, c.rcon_password.clone());
        tokio::spawn(async move {
            urt::rcon(&host, port, &pw, &format!("say \"^5[Discord] ^7{}: {}\"", author, text)).await;
        });
        true
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::embed::Field;
    use async_trait::async_trait;
    use indexmap::IndexMap;
    use std::sync::atomic::AtomicU64;

    #[derive(Default)]
    pub struct Recorder {
        pub sent: Mutex<Vec<(u64, Embed, Option<Components>)>>,
        pub edits: Mutex<Vec<(u64, u64, Embed, Option<Components>)>>,
        pub next: AtomicU64,
        pub gone: Mutex<Vec<u64>>,
        pub deleted: Mutex<Vec<(u64, u64)>>,
    }

    #[async_trait]
    impl Chat for Recorder {
        async fn send(&self, channel: u64, embed: Embed, comps: Option<Components>) -> Result<u64, ChatError> {
            self.sent.lock().unwrap().push((channel, embed, comps));
            Ok(100 + self.next.fetch_add(1, Ordering::SeqCst))
        }
        async fn edit(&self, channel: u64, message: u64, embed: Embed, comps: Option<Components>) -> Result<(), ChatError> {
            if self.gone.lock().unwrap().contains(&message) {
                return Err(ChatError::NotFound);
            }
            self.edits.lock().unwrap().push((channel, message, embed, comps));
            Ok(())
        }
        async fn delete(&self, channel: u64, message: u64) -> Result<(), ChatError> {
            self.deleted.lock().unwrap().push((channel, message));
            Ok(())
        }
        async fn react(&self, _: u64, _: u64, _: &str) -> Result<(), ChatError> {
            Ok(())
        }
    }

    pub fn server() -> ServerConfig {
        ServerConfig { key: "main".into(), name: "Test".into(), host: "127.0.0.1".into(), log_path: "/nonexistent/games.log".into(), ..Default::default() }
    }

    pub fn watch_with(cfg: ServerConfig, delay: u32) -> (Arc<Watch>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        let deps = WatchDeps::plain(rec.clone(), Arc::new(FileSystems::new(&IndexMap::new()).unwrap()));
        (Watch::new(deps, 42, cfg, delay, None).unwrap(), rec)
    }

    fn snap(ts: f64, players: &[(&str, i64)], red: Option<i64>, blue: Option<i64>) -> Snapshot {
        Snapshot {
            ts, online: true, hostname: "My Server".into(), mapname: "ut4_turnpike".into(), gametype: 7, max_clients: 16, red, blue,
            players: players.iter().map(|(n, s)| urt::Player { name: n.to_string(), score: *s, ping: 30 }).collect(),
        }
    }

    fn feed(w: &Watch, line: &str, t: f64) {
        let evs = { w.st.lock().unwrap().parser.as_mut().unwrap().feed_at(line, t) };
        w.st.lock().unwrap().pending.extend(evs);
    }

    #[test]
    fn connecting_and_offline_boards() {
        let (w, _) = watch_with(server(), 0);
        assert_eq!(w.build_board(100.0).0.title, "⏳ Connecting…");
        w.st.lock().unwrap().snaps.push_back(Snapshot { ts: 99.0, online: false, ..Default::default() });
        assert!(w.build_board(100.0).0.title.contains("not responding"));
    }

    #[test]
    fn an_empty_server_still_gets_a_clean_board() {
        let mut sc = server();
        sc.spectator = Some(crate::config::SpectatorConfig { client_name: "^3GTV".into(), ..Default::default() });
        let (w, _) = watch_with(sc, 0);
        // nobody online, or only our own spectator client (which is hidden from the board)
        for players in [vec![], vec![("GTV", 0)]] {
            w.st.lock().unwrap().snaps.clear();
            w.st.lock().unwrap().snaps.push_back(snap(50.0, &players, None, None));
            let (e, names) = w.build_board(60.0);
            assert!(e.title.starts_with("📡 LIVE"), "{}", e.title);
            assert!(e.description.contains("0/16"), "{}", e.description);
            assert!(e.fields.is_empty() && names.is_empty());
            // no player menu (Discord refuses an empty one); the board itself is still valid
            assert!(w.components(&names).player_options.is_empty());
        }
    }

    #[test]
    fn board_groups_teams_and_renders_a_table() {
        let (w, _) = watch_with(server(), 0);
        for (slot, name, team) in [(0, "Alice", 1), (1, "Bob", 2), (2, "Cy", 3), (3, "Dee", 0)] {
            feed(&w, &format!("ClientUserinfoChanged: {} n\\{}\\t\\{}", slot, name, team), 1.0);
        }
        w.st.lock().unwrap().pending.clear();
        w.st.lock().unwrap().snaps.push_back(snap(50.0, &[("Alice", 7), ("Bob", 9), ("Cy", 0), ("Dee", 3)], Some(3), Some(5)));
        w.st.lock().unwrap().stats.insert("alice".into(), PlayerStats { name: "Alice".into(), kills: 7, deaths: 2, ..Default::default() });
        let (e, names) = w.build_board(60.0);
        assert_eq!(e.title, "📡 LIVE · My Server");
        assert!(e.description.contains("ut4\\_turnpike") && e.description.contains("CTF") && e.description.contains("4/16"));
        assert!(e.description.contains("🔴 **3**  –  **5** 🔵"));
        assert_eq!(e.color, 0x3498DB); // blue leads
        let labels: Vec<&str> = e.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(labels, vec!["🔴 Red", "🔵 Blue", "⚪ Free", "👁️ Spectators"]);
        assert!(e.fields[0].value.contains("Alice") && e.fields[0].value.contains("7/2"));
        assert!(e.fields[0].value.starts_with("```\nPlayer"));
        assert_eq!(names, vec!["Alice", "Bob", "Cy", "Dee"]);
    }

    #[test]
    fn spectator_client_is_hidden_from_the_board() {
        let mut cfg = server();
        cfg.spectator = Some(crate::config::SpectatorConfig { client_name: "^3GTV".into(), ..Default::default() });
        let (w, _) = watch_with(cfg, 0);
        w.st.lock().unwrap().snaps.push_back(snap(50.0, &[("GTV", 0), ("Alice", 1)], None, None));
        let (e, names) = w.build_board(60.0);
        assert_eq!(names, vec!["Alice"]);
        assert!(e.description.contains("1/16"));
        assert_eq!(e.fields[0].name, "Players");
    }

    #[test]
    fn delay_holds_back_both_the_scoreboard_and_the_feed() {
        let (w, _) = watch_with(server(), 30);
        w.st.lock().unwrap().snaps.push_back(snap(100.0, &[("Old", 1)], None, None));
        w.st.lock().unwrap().snaps.push_back(snap(125.0, &[("New", 9)], None, None));
        assert_eq!(w.build_board(140.0).1, vec!["Old"]);      // 140-30 = 110 -> the first snapshot
        assert!(w.build_board(140.0).0.description.contains("⏱ 30s delay"));
        assert_eq!(w.build_board(160.0).1, vec!["New"]);
        feed(&w, "Kill: 0 1 19: A killed B by UT_MOD_UMP45", 100.0);
        assert!(!w.flush_due(120.0));
        assert!(w.build_feed().description.contains("Waiting"));
        w.flush_due(131.0);
        assert!(w.build_feed().description.contains("UMP45"));
    }

    #[test]
    fn stats_player_cards_and_match_summary() {
        let (w, _) = watch_with(server(), 0);
        for (slot, name) in [(0, "Alice"), (1, "Bob")] {
            feed(&w, &format!("ClientUserinfoChanged: {} n\\{}\\t\\1", slot, name), 1.0);
        }
        w.st.lock().unwrap().pending.clear();
        feed(&w, "Hit: 1 0 0 5: Alice hit Bob in the Head", 100.0);
        feed(&w, "Kill: 0 1 19: Alice killed Bob by UT_MOD_DEAGLE", 100.1);
        feed(&w, "Kill: 0 1 19: Alice killed Bob by UT_MOD_DEAGLE", 101.0);
        feed(&w, "Kill: 1 0 19: Bob killed Alice by UT_MOD_UMP45", 102.0);
        feed(&w, "Exit: Timelimit hit.", 103.0);
        assert!(w.flush_due(200.0));
        let card = w.player_card("alice");
        let f = |n: &str| card.fields.iter().find(|x: &&Field| x.name == n).map(|x| x.value.clone());
        assert_eq!(f("Kills").unwrap(), "2");
        assert_eq!(f("Deaths").unwrap(), "1");
        assert_eq!(f("K/D").unwrap(), "2.00");
        assert_eq!(f("Headshots").unwrap(), "1");
        assert_eq!(f("Best streak").unwrap(), "2");
        assert!(f("Top weapons").unwrap().contains("Desert Eagle ×2"));
        assert!(w.player_card("nobody").description.contains("No frags"));
        let top = { Watch::top_fraggers(&w.st.lock().unwrap()) };
        let s = Watch::summary_embed(&top, 0, "ut4").unwrap();
        assert!(s.description.starts_with("🥇 **Alice** — 2 kills / 1 deaths"));
        assert!(s.description.contains("🥈 **Bob**"));
        assert!(Watch::summary_embed(&[], 0, "ut4").is_none());
        // a new map clears the stats
        feed(&w, "InitGame: \\mapname\\ut4_abbey", 300.0);
        w.flush_due(400.0);
        assert!(w.player_card("alice").description.contains("No frags"));
    }

    #[test]
    fn player_cards_need_the_log() {
        let mut cfg = server();
        cfg.log_path.clear();
        let (w, _) = watch_with(cfg, 0);
        assert!(w.player_card("x").description.contains("log_path"));
    }

    #[test]
    fn feed_keeps_only_the_latest_lines() {
        let (w, _) = watch_with(server(), 0);
        for i in 0..20 {
            feed(&w, &format!("Bomb was planted by {}", i), 1.0);
        }
        w.flush_due(10.0);
        let d = w.build_feed().description;
        assert_eq!(d.lines().count(), FEED_LINES);
        assert!(d.contains("**#19**") && !d.contains("**#2**") && !d.contains("**#5**"));
    }

    #[test]
    fn game_chat_never_reaches_the_discord_feed() {
        let (w, _) = watch_with(server(), 0);
        feed(&w, "say: 0 Al: secret tactics", 1.0);
        feed(&w, "sayteam: 0 Al: rush b", 1.0);
        feed(&w, "Bomb was planted by 7", 1.0);
        w.flush_due(10.0);
        let d = w.build_feed().description;
        assert!(d.contains("**#7**"));
        assert!(!d.contains("secret") && !d.contains("rush") && !d.contains("💬"), "{}", d);
    }

    #[test]
    fn components_cap_the_dropdown() {
        let (w, _) = watch_with(server(), 0);
        let names: Vec<String> = (0..40).map(|i| format!("p{}", i)).collect();
        assert_eq!(w.components(&names).player_options.len(), 25);
        assert_eq!(w.components(&names).watch_url, None);
    }

    #[test]
    fn a_server_with_its_own_port_links_to_it_and_the_feed_names_the_server() {
        use crate::config::SpectatorConfig;
        use crate::spectator::{Platform, Streamer};
        let rec = Arc::new(Recorder::default());
        let mut deps = WatchDeps::plain(rec, Arc::new(FileSystems::new(&IndexMap::new()).unwrap()));
        deps.web = Some(("http://example.org:8080".into(), "tok".into()));
        let work = std::env::temp_dir().join(format!("gtv-wu-{}", crate::config::random_token(4)));
        let mk = |port: u16, public: &str| {
            let sp = SpectatorConfig { web_port: port, public_url: public.into(), work_dir: work.to_string_lossy().into_owned(), ..Default::default() };
            let st = Streamer::with_platform("main", "h", 1, &sp, 90, 0, None, Platform::Linux).unwrap();
            st.mark_running_for_test();
            let mut cfg = server();
            cfg.spectator = Some(sp);
            Watch::new(deps.clone(), 42, cfg, 0, Some(st)).unwrap()
        };
        assert_eq!(mk(0, "").watch_url().unwrap(), "http://example.org:8080/watch/main?t=tok");
        let w = mk(8081, "");
        assert_eq!(w.watch_url().unwrap(), "http://example.org:8081/watch/main?t=tok");
        assert_eq!(mk(8081, "https://gtv2.example.com").watch_url().unwrap(), "https://gtv2.example.com/watch/main?t=tok");
        // the drop-down under the board remembers which server it belongs to (a channel can show several)
        assert_eq!(w.components(&["x".to_string()]).server, "main");
        assert!(w.build_feed().title.contains("Test"), "{}", w.build_feed().title);
    }

    #[tokio::test]
    async fn the_scoreboard_shows_the_current_maps_picture_at_one_fixed_size() {
        use crate::levelshot::tests::{sample, tmp};
        let t = tmp("board");
        std::fs::write(t.join("ut4_turnpike.jpg"), sample(512, 384, "jpg")).unwrap();
        let rec = Arc::new(Recorder::default());
        let mut deps = WatchDeps::plain(rec, Arc::new(FileSystems::new(&IndexMap::new()).unwrap()));
        deps.levels = crate::levelshot::Levelshots::new(t.join("out"));
        let live = |on: bool, size: u32| Arc::new(Live {
            level_on: on, level_size: size, level_fit: crate::levelshot::Fit::Crop, public_url: "http://example.org:8080/".into(), ..Default::default()
        });
        *deps.live.write().unwrap() = live(true, 320);
        let mut cfg = server();
        cfg.levelshot_dir = t.to_string_lossy().into_owned();
        let w = Watch::new(deps.clone(), 42, cfg, 0, None).unwrap();
        w.st.lock().unwrap().snaps.push_back(snap(50.0, &[("Alice", 3)], None, None));

        // the first redraw starts the search and shows no picture yet; a later one has it
        assert_eq!(w.build_board(60.0).0.image, "");
        let mut url = String::new();
        for _ in 0..200 {
            url = w.build_board(60.0).0.image;
            if !url.is_empty() { break }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(url.starts_with("http://example.org:8080/levelshot/ut4_turnpike-320c-") && url.ends_with(".jpg"), "{}", url);
        let file = url.rsplit('/').next().unwrap();
        assert!(crate::levelshot::valid_file_name(file));
        assert_eq!(image::open(t.join("out").join(file)).unwrap().to_rgb8().dimensions(), (320, 320));
        // the logo corner is untouched: the picture is the large image, not the thumbnail
        assert_eq!(w.build_board(60.0).0.thumbnail, "");

        // a map with no picture shows none (and does not break the board)
        w.st.lock().unwrap().snaps.push_back(Snapshot { mapname: "ut4_nopic".into(), ..snap(70.0, &[("Alice", 3)], None, None) });
        for _ in 0..3 {
            let e = w.build_board(80.0).0;
            assert_eq!(e.image, "");
            assert!(e.title.starts_with("📡 LIVE"));
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        // switched off, or no public address: no picture and no search
        *deps.live.write().unwrap() = live(false, 320);
        w.st.lock().unwrap().snaps.push_back(snap(90.0, &[("Alice", 3)], None, None));
        assert_eq!(w.build_board(100.0).0.image, "");
        let _ = std::fs::remove_dir_all(t);
    }

    /// Developer tool, not a test: write what four broadcasts in one channel would post (the real scoreboard and feed
    /// builders, the real picture pipeline) as JSON, for rendering a preview.
    ///   GTV_PREVIEW_OUT=/path/posts.json GTV_PREVIEW_MAPS=/folder/with/ut4_*.jpg GTV_PREVIEW_SIZE=320 \
    ///     cargo test dump_channel_preview -- --ignored
    #[ignore]
    #[tokio::test]
    async fn dump_channel_preview() {
        let out = std::path::PathBuf::from(std::env::var("GTV_PREVIEW_OUT").expect("GTV_PREVIEW_OUT"));
        let maps = std::path::PathBuf::from(std::env::var("GTV_PREVIEW_MAPS").expect("GTV_PREVIEW_MAPS"));
        let size: u32 = std::env::var("GTV_PREVIEW_SIZE").ok().and_then(|v| v.parse().ok()).unwrap_or(320);
        let pics = out.parent().unwrap().join("levelshots");
        let _ = std::fs::remove_dir_all(&pics);
        let rec = Arc::new(Recorder::default());
        let mut deps = WatchDeps::plain(rec, Arc::new(FileSystems::new(&IndexMap::new()).unwrap()));
        deps.levels = crate::levelshot::Levelshots::new(pics);
        *deps.live.write().unwrap() = Arc::new(Live {
            name: "The Forge".into(), color: Some(0xE67E22), logo_url: "logo.png".into(), icon_url: "icon.png".into(), each_map: true,
            level_on: true, level_size: size, level_fit: crate::levelshot::Fit::Crop, public_url: "http://example.org:8080".into(), ..Default::default()
        });
        // (key, name, map, gametype, red, blue, delay, players: (name, score, team 1=red 2=blue 3=spec 0=free))
        type P = (&'static str, i64, u8);
        let scenes: Vec<(&str, &str, &str, i32, Option<i64>, Option<i64>, u32, Vec<P>)> = vec![
            ("pub1", "TFG | Public #1", "ut4_turnpike", 7, Some(3), Some(5), 20, vec![("Viper", 18, 1), ("Nova", 14, 1), ("Rook", 9, 1), ("Echo", 4, 1), ("Blaze", 21, 2), ("Kestrel", 16, 2), ("Moth", 11, 2), ("Pike", 6, 2)]),
            ("bomb", "TFG | Bomb Server", "ut4_casa", 8, Some(6), Some(4), 20, vec![("Hex", 12, 1), ("Dune", 10, 1), ("Wren", 7, 1), ("Zed", 13, 2), ("Orca", 9, 2), ("Fable", 5, 2)]),
            ("war", "TFG | Clan War", "ut4_abbey", 4, Some(2), Some(1), 45, vec![("Aciddrop", 15, 1), ("Slate", 11, 1), ("Ember", 8, 1), ("Crow", 14, 2), ("Lynx", 10, 2), ("Tarn", 6, 2)]),
            ("tdm", "TFG | Team Deathmatch", "ut4_uptown", 3, Some(31), Some(27), 0, vec![("Jolt", 19, 1), ("Sable", 12, 1), ("Quill", 8, 1), ("Rune", 17, 2), ("Dax", 14, 2), ("Mira", 9, 2), ("Onyx", 0, 3)]),
        ];
        let mut posts = Vec::new();
        let mut watches = Vec::new();
        for (i, (key, name, map, gt, red, blue, delay, players)) in scenes.iter().enumerate() {
            let mut cfg = server();
            cfg.key = key.to_string();
            cfg.name = name.to_string();
            cfg.levelshot_dir = maps.to_string_lossy().into_owned();
            let w = Watch::new(deps.clone(), 42, cfg, *delay, None).unwrap();
            for (slot, (n, _, team)) in players.iter().enumerate() {
                feed(&w, &format!("ClientUserinfoChanged: {} n\\{}\\t\\{}", slot, n, team), 1.0);
            }
            // a few real kills, for the K/D column and the live feed
            for k in 0..players.len().min(6) {
                let (a, b) = (k, (k + 3) % players.len());
                for rep in 0..(1 + (players[a].1 / 8)) {
                    feed(&w, &format!("Kill: {} {} 19: {} killed {} by UT_MOD_{}", a, b, players[a].0, players[b].0, ["AK103", "DEAGLE", "LR300", "UMP45"][(k + rep as usize + i) % 4]), 5.0 + k as f64 + rep as f64 * 0.1);
                }
            }
            w.flush_due(200.0);
            let snap = Snapshot {
                ts: 190.0, online: true, hostname: name.to_string(), mapname: map.to_string(), gametype: *gt, max_clients: 16, red: *red, blue: *blue,
                players: players.iter().map(|(n, s, _)| urt::Player { name: n.to_string(), score: *s, ping: 25 + (n.len() as i64 * 7) % 60 }).collect(),
            };
            w.st.lock().unwrap().snaps.push_back(snap);
            watches.push(w);
        }
        // the first redraw of each starts finding its picture; ask again until all four are there
        for _ in 0..300 {
            if watches.iter().all(|w| !w.build_board(200.0 + w.delay() as f64).0.image.is_empty()) { break }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let to_json = |channel: u64, e: &Embed| json!({
            "channel": channel, "title": e.title, "description": e.description, "color": e.color,
            "fields": e.fields.iter().map(|f| json!({"name": f.name, "value": f.value, "inline": f.inline})).collect::<Vec<_>>(),
            "footer": e.footer, "author": e.author, "author_icon": e.author_icon, "thumbnail": e.thumbnail, "image": e.image,
        });
        for w in &watches {
            // the order a real start posts them: the scoreboard, then the feed
            let (board, _) = w.build_board(200.0 + w.delay() as f64);
            posts.push(to_json(w.channel_id, &board));
            posts.push(to_json(w.channel_id, &w.build_feed()));
        }
        std::fs::write(&out, serde_json::to_string_pretty(&json!({"size": size, "posts": posts})).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn relay_filters_rate_limits_and_requires_rcon() {
        let mut cfg = server();
        cfg.relay_chat = true;
        cfg.rcon_password = "pw".into();
        let (w, _) = watch_with(cfg, 0);
        assert!(w.relay(1, "Ann;", "hello \"world\"; rcon quit"));
        assert!(!w.relay(1, "Ann", "again at once"));      // 2 s limit per author
        assert!(!w.relay(2, "Bob", "你好"));                 // nothing printable left
        let (w2, _) = watch_with(server(), 0);
        assert!(!w2.relay(1, "Ann", "hi"));                  // relay not enabled
    }

    #[tokio::test]
    async fn starting_posts_the_board_and_feed_and_stopping_marks_it_ended() {
        let mut cfg = server();
        cfg.log_path.clear();
        let (w, rec) = watch_with(cfg, 0);
        w.start().await.unwrap();
        assert_eq!(rec.sent.lock().unwrap().len(), 2);
        assert_eq!(rec.sent.lock().unwrap()[0].1.title, "⏳ Connecting…");
        w.stop().await;
        let edits = rec.edits.lock().unwrap();
        let last = edits.last().unwrap();
        assert_eq!(last.1, 100);
        assert_eq!(last.2.title, "⏹️ Broadcast ended");
        assert_eq!(last.3, Some(Components::default()));
    }

    #[tokio::test]
    async fn a_deleted_board_message_is_recreated() {
        let mut cfg = server();
        cfg.log_path.clear();
        let (w, rec) = watch_with(cfg, 0);
        w.start().await.unwrap();
        w.stop.stop();
        rec.gone.lock().unwrap().push(100);
        w.edit(true, Embed::new("x", 1), None).await;
        assert_eq!(rec.sent.lock().unwrap().len(), 3);
        assert_eq!(w.st.lock().unwrap().board_id, Some(102));
    }

    fn set_live(w: &Watch, f: impl FnOnce(&mut Live)) {
        let mut l = (**w.deps.live.read().unwrap()).clone();
        f(&mut l);
        *w.deps.live.write().unwrap() = Arc::new(l);
    }

    /// A match was played on the first post: some activity, then the next map starts.
    fn play_one_map_then_change(w: &Watch) {
        feed(w, "Kill: 0 1 19: A killed B by UT_MOD_UMP45", 100.0);
        feed(w, "Exit: Timelimit hit.", 101.0);
        feed(w, "InitGame: \\mapname\\ut4_abbey", 110.0);
        feed(w, "Kill: 0 1 19: A killed B by UT_MOD_DEAGLE", 111.0);
        w.flush_due(500.0);
    }

    #[tokio::test]
    async fn a_new_map_gets_a_new_post_and_the_old_one_is_closed_with_its_final_state() {
        let mut cfg = server();
        cfg.log_path.clear();
        let (w, rec) = watch_with(cfg, 0);
        w.start().await.unwrap();
        assert_eq!(rec.sent.lock().unwrap().len(), 2); // board 100, feed 101
        // the board as it was at the end of the first map
        w.st.lock().unwrap().snaps.push_back(snap(50.0, &[("Alice", 7)], Some(3), Some(5)));
        let (b, _) = w.build_board(60.0);
        w.st.lock().unwrap().last_board = Some(b);
        w.st.lock().unwrap().map_now = "ut4_turnpike".into();
        w.st.lock().unwrap().parser = Some(LogParser::new(true));
        play_one_map_then_change(&w);
        let r = w.st.lock().unwrap().retired.take().expect("the map change was noticed");
        w.rotate(r).await;
        // fresh board + feed were posted below
        assert_eq!(rec.sent.lock().unwrap().len(), 4);
        assert_eq!(w.st.lock().unwrap().board_id, Some(102));
        assert_eq!(w.st.lock().unwrap().feed_id, Some(103));
        // the new feed starts with the new map, not the old fights
        let d = w.build_feed().description;
        assert!(d.contains("abbey") && !d.contains("UMP45"), "{}", d);
        // the old board was closed: marked finished, no Watch button / menu
        let edits = rec.edits.lock().unwrap();
        let ob = edits.iter().find(|e| e.1 == 100).expect("old board edited");
        assert!(ob.2.title.starts_with("🏁 FINISHED"), "{}", ob.2.title);
        assert!(ob.2.description.contains("newest post"));
        assert_eq!(ob.3, Some(Components::default()));
        // the old feed keeps its lines under a finished title
        let of = edits.iter().find(|e| e.1 == 101).expect("old feed edited");
        assert!(of.2.title.contains("finished") && of.2.title.contains("ut4"), "{}", of.2.title);
        assert!(of.2.description.contains("UMP45"), "{}", of.2.description);
        assert!(rec.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn old_posts_can_be_removed_instead_of_kept() {
        let mut cfg = server();
        cfg.log_path.clear();
        let (w, rec) = watch_with(cfg, 0);
        set_live(&w, |l| l.delete_old = true);
        w.start().await.unwrap();
        w.st.lock().unwrap().parser = Some(LogParser::new(true));
        play_one_map_then_change(&w);
        let r = w.st.lock().unwrap().retired.take().unwrap();
        w.rotate(r).await;
        assert_eq!(*rec.deleted.lock().unwrap(), vec![(42, 100), (42, 101)]);
        assert_eq!(w.st.lock().unwrap().board_id, Some(102));
    }

    #[test]
    fn one_post_per_broadcast_when_the_option_is_off_and_nothing_happens_until_something_did() {
        let (w, _) = watch_with(server(), 0);
        set_live(&w, |l| l.each_map = false);
        w.st.lock().unwrap().parser = Some(LogParser::new(true));
        play_one_map_then_change(&w);
        assert!(w.st.lock().unwrap().retired.is_none());
        // on: a map change right after the bot joined (nothing on the posts yet) reuses them
        let (w2, _) = watch_with(server(), 0);
        feed(&w2, "InitGame: \\mapname\\ut4_abbey", 10.0);
        w2.flush_due(100.0);
        assert!(w2.st.lock().unwrap().retired.is_none());
        // ... but after some play, the next one starts a new post
        feed(&w2, "Kill: 0 1 19: A killed B by UT_MOD_UMP45", 200.0);
        feed(&w2, "InitGame: \\mapname\\ut4_turnpike", 210.0);
        w2.flush_due(300.0);
        assert!(w2.st.lock().unwrap().retired.is_some());
    }

    #[tokio::test]
    async fn if_the_new_post_cannot_be_made_the_old_ones_keep_working() {
        struct NoSend(Arc<Recorder>);
        #[async_trait]
        impl Chat for NoSend {
            async fn send(&self, c: u64, e: Embed, k: Option<Components>) -> Result<u64, ChatError> {
                if self.0.sent.lock().unwrap().len() >= 2 { return Err(ChatError::Forbidden); }
                self.0.send(c, e, k).await
            }
            async fn edit(&self, c: u64, m: u64, e: Embed, k: Option<Components>) -> Result<(), ChatError> { self.0.edit(c, m, e, k).await }
            async fn delete(&self, c: u64, m: u64) -> Result<(), ChatError> { self.0.delete(c, m).await }
            async fn react(&self, c: u64, m: u64, e: &str) -> Result<(), ChatError> { self.0.react(c, m, e).await }
        }
        let rec = Arc::new(Recorder::default());
        let mut cfg = server();
        cfg.log_path.clear();
        let deps = WatchDeps::plain(Arc::new(NoSend(rec.clone())), Arc::new(FileSystems::new(&IndexMap::new()).unwrap()));
        let w = Watch::new(deps, 42, cfg, 0, None).unwrap();
        w.start().await.unwrap();
        w.rotate(Retired { board: None, feed: vec![], map: String::new() }).await;
        assert_eq!(w.st.lock().unwrap().board_id, Some(100)); // unchanged
        assert!(rec.edits.lock().unwrap().is_empty() && rec.deleted.lock().unwrap().is_empty());
    }

    #[test]
    fn viewer_count_shows_on_the_board_and_in_the_summary() {
        let mut sc = server();
        sc.key = "main".into();
        let (w, _) = watch_with(sc, 0);
        w.st.lock().unwrap().snaps.push_back(snap(50.0, &[("Alice", 7)], None, None));
        // no stream running: no viewer line at all
        assert!(!w.build_board(60.0).0.description.contains("watching"));
        w.deps.viewers.beat("main", "aaaaaaaa1", true);
        w.deps.viewers.beat("main", "bbbbbbbb2", true);
        w.deps.viewers.beat("main", "cccccccc3", false);
        assert_eq!(w.viewers().watching, 2);
        assert_eq!(w.viewers().open, 3);
        let top = vec![PlayerStats { name: "A".into(), kills: 2, ..Default::default() }];
        let s = Watch::summary_embed(&top, 5, "ut4").unwrap();
        assert!(s.fields.iter().any(|f| f.name.contains("Peak viewers") && f.value == "5"));
        assert!(!Watch::summary_embed(&top, 0, "ut4").unwrap().fields.iter().any(|f| f.name.contains("Peak")));
    }

    #[test]
    fn every_post_carries_the_owner_credit_whatever_the_branding() {
        let (w, _) = watch_with(server(), 0);
        set_live(&w, |l| {
            l.name = "Some Other Clan".into();
            l.color = Some(0xABCDEF);
            l.logo_url = "https://x.example/logo.png".into();
            l.icon_url = "https://x.example/icon.png".into();
        });
        let credit = |e: &Embed| e.footer.contains(crate::version::OWNER) && e.footer.contains(&format!("v{}", VERSION)) && e.footer_icon.is_empty();
        // connecting / offline / live boards
        let (e, _) = w.build_board(100.0);
        assert!(credit(&e) && e.author == "Some Other Clan" && e.thumbnail == "https://x.example/logo.png");
        w.st.lock().unwrap().snaps.push_back(Snapshot { ts: 99.0, online: false, ..Default::default() });
        assert!(credit(&w.build_board(100.0).0));
        w.st.lock().unwrap().snaps.push_back(snap(100.0, &[("Alice", 7)], None, None));
        let (live_board, _) = w.build_board(110.0);
        assert!(credit(&live_board) && live_board.color == 0xABCDEF);
        // team games keep red/blue so the leading side is visible
        w.st.lock().unwrap().snaps.push_back(snap(120.0, &[("Alice", 7)], Some(9), Some(1)));
        assert_eq!(w.build_board(130.0).0.color, 0xE74C3C);
        // the rest
        assert!(credit(&w.build_feed()) && w.build_feed().color == 0xABCDEF);
        assert!(credit(&w.player_card("nobody")));
        let top = vec![PlayerStats { name: "A".into(), kills: 2, ..Default::default() }];
        assert!(credit(&w.live().brand(Watch::summary_embed(&top, 0, "ut4").unwrap())));
    }

    #[tokio::test]
    async fn the_ended_post_is_branded_too() {
        let mut cfg = server();
        cfg.log_path.clear();
        let (w, rec) = watch_with(cfg, 0);
        set_live(&w, |l| l.name = "Clan".into());
        w.start().await.unwrap();
        w.stop().await;
        let edits = rec.edits.lock().unwrap();
        let last = edits.last().unwrap();
        assert!(last.2.footer.contains(crate::version::OWNER) && last.2.author == "Clan");
    }
}
