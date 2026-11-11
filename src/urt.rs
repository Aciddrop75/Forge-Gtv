// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Urban Terror (Quake 3 protocol) helpers: status query, rcon, games.log parsing.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use tokio::net::UdpSocket;

pub const OOB: &[u8] = b"\xff\xff\xff\xff";

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("regex"))
        }
    };
}

re!(color_re, r"(?s)\^[^\^]");
re!(md_re, r"([\\*_~`|>])");
re!(ts_re, r"^\d+:\d\d\s+");
re!(kill_re, r"^Kill: (\d+) (\d+) (\d+): (.+?) killed (.+?) by (\S+)$");
re!(hit_re, r"^Hit: (\d+) (\d+) (\d+) (\d+): .*? hit .*? in the (.+)$");
re!(flag_re, r"^Flag: (\d+) (\d+): (\S+)");
re!(player_re, r#"^(-?\d+) (-?\d+) "(.*)"$"#);
re!(mod_prefix_re, r"^(UT_)?MOD_");

/// Remove Quake colour codes (^1, ^x ...).
pub fn strip_colors(s: &str) -> String {
    color_re().replace_all(s, "").trim().to_string()
}

/// Escape Discord markdown.
pub fn esc(s: &str) -> String {
    md_re().replace_all(s, "\\$1").into_owned()
}

pub fn gametype_name(n: i32) -> &'static str {
    match n {
        0 => "FFA",
        1 => "LMS",
        3 => "TDM",
        4 => "Team Survivor",
        5 => "Follow the Leader",
        6 => "Capture & Hold",
        7 => "CTF",
        8 => "Bomb",
        9 => "Jump",
        10 => "Freeze Tag",
        11 => "Gun Game",
        _ => "?",
    }
}

fn known_gametype(n: i32) -> Option<&'static str> {
    let g = gametype_name(n);
    if g == "?" { None } else { Some(g) }
}

const MODS: &[(&str, &str)] = &[
    ("KNIFE", "Knife"), ("KNIFE_THROWN", "Thrown knife"), ("BERETTA", "Beretta"),
    ("DEAGLE", "Desert Eagle"), ("SPAS", "SPAS-12"), ("UMP45", "UMP45"), ("MP5K", "MP5K"),
    ("LR300", "LR-300"), ("G36", "G36"), ("PSG1", "PSG-1"), ("HK69", "HK69"),
    ("HK69_HIT", "HK69 (direct)"), ("BLED", "Bleeding"), ("KICKED", "Boot"),
    ("HEGRENADE", "HE grenade"), ("FLAG", "Flag"), ("GOOMBA", "Goomba"), ("SR8", "SR-8"),
    ("AK103", "AK-103"), ("NEGEV", "Negev"), ("M4", "M4"), ("GLOCK", "Glock"),
    ("COLT1911", "Colt 1911"), ("MAC11", "MAC-11"), ("FRF1", "FR-F1"), ("BENELLI", "Benelli"),
    ("P90", "P90"), ("MAGNUM", ".44 Magnum"), ("TOD50", "TOD-50"), ("SMITED", "Smited"),
    ("SLAPPED", "Slapped"), ("BOMBED", "Bomb"), ("NUKED", "Nuked"), ("TELEFRAG", "Telefrag"),
    ("FALLING", "Fall damage"), ("SUICIDE", "Suicide"), ("TRIGGER_HURT", "Map hazard"),
    ("CHANGE_TEAM", "Team change"), ("WATER", "Drowned"), ("LAVA", "Lava"), ("SLIME", "Slime"),
    ("CRUSH", "Crushed"),
];

fn title_case(s: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for c in s.chars() {
        if c.is_alphabetic() {
            if start { out.extend(c.to_uppercase()); } else { out.extend(c.to_lowercase()); }
            start = false;
        } else {
            out.push(c);
            start = true;
        }
    }
    out
}

pub fn weapon_name(modname: &str) -> String {
    let key = mod_prefix_re().replace(modname, "").into_owned();
    match MODS.iter().find(|(k, _)| *k == key) {
        Some((_, v)) => v.to_string(),
        None => title_case(&key.replace('_', " ")),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Player {
    pub name: String,
    pub score: i64,
    pub ping: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub ts: f64,
    pub online: bool,
    pub hostname: String,
    pub mapname: String,
    pub gametype: i32,
    pub max_clients: i64,
    pub red: Option<i64>,
    pub blue: Option<i64>,
    pub players: Vec<Player>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Kill,
    Flag,
    Map,
    Exit,
    Join,
    Leave,
    Team,
    Round,
    Info,
    Score,
}

#[derive(Clone, Debug, Default)]
pub struct EventData {
    pub name: Option<String>,
    pub killer: Option<String>,
    pub victim: Option<String>,
    pub killer_slot: Option<i32>,
    #[allow(dead_code)]
    pub victim_slot: Option<i32>,
    pub weapon: Option<String>,
    pub hs: bool,
    pub slot: Option<i32>,
    pub map: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub ts: f64,
    pub kind: Kind,
    pub text: String,
    pub data: EventData,
}

impl Event {
    pub fn new(ts: f64, kind: Kind, text: String) -> Self {
        Event { ts, kind, text, data: EventData::default() }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PlayerStats {
    pub name: String,
    pub kills: u32,
    pub deaths: u32,
    pub headshots: u32,
    pub streak: u32,
    pub best_streak: u32,
    pub weapons: HashMap<String, u32>,
}

impl PlayerStats {
    pub fn new(name: &str) -> Self {
        PlayerStats { name: name.to_string(), ..Default::default() }
    }

    /// Weapons, most used first (ties broken by name so the order is stable).
    pub fn top_weapons(&self, n: usize) -> Vec<(String, u32)> {
        let mut v: Vec<_> = self.weapons.iter().map(|(k, v)| (k.clone(), *v)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

// ---- server query / rcon (plain UDP) ----

async fn oob_request(host: &str, port: u16, payload: &[u8], expect: &[u8], timeout: f64) -> Option<Vec<u8>> {
    let addr = tokio::net::lookup_host((host, port)).await.ok()?.next()?;
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = UdpSocket::bind(bind).await.ok()?;
    sock.connect(addr).await.ok()?;
    let mut msg = OOB.to_vec();
    msg.extend_from_slice(payload);
    sock.send(&msg).await.ok()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(timeout);
    let mut buf = vec![0u8; 65535];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                let data = &buf[..n];
                if data.starts_with(OOB) && data[OOB.len()..].starts_with(expect) {
                    return Some(data.to_vec());
                }
            }
            _ => return None,
        }
    }
}

pub fn parse_status(data: &[u8]) -> Snapshot {
    let body = if data.len() >= OOB.len() { &data[OOB.len()..] } else { data };
    let text = String::from_utf8_lossy(body);
    let lines: Vec<&str> = text.split('\n').collect();
    let info = lines.get(1).copied().unwrap_or("");
    let parts: Vec<&str> = info.trim_start_matches('\\').split('\\').collect();
    let mut cv: HashMap<String, String> = HashMap::new();
    for kv in parts.chunks(2) {
        if kv.len() == 2 {
            cv.insert(kv[0].to_lowercase(), kv[1].to_string());
        }
    }
    let mut players = Vec::new();
    for ln in lines.iter().skip(2) {
        if let Some(m) = player_re().captures(ln.trim()) {
            players.push(Player {
                name: strip_colors(&m[3]),
                score: m[1].parse().unwrap_or(0),
                ping: m[2].parse().unwrap_or(0),
            });
        }
    }
    let num = |k: &str| cv.get(k).and_then(|v| v.trim().parse::<i64>().ok());
    Snapshot {
        ts: now(),
        online: true,
        hostname: strip_colors(cv.get("sv_hostname").map(String::as_str).unwrap_or("")),
        mapname: cv.get("mapname").cloned().unwrap_or_else(|| "?".into()),
        gametype: num("g_gametype").unwrap_or(-1) as i32,
        max_clients: num("sv_maxclients").unwrap_or(0),
        red: num("score_red"),
        blue: num("score_blue"),
        players,
    }
}

pub async fn query_status(host: &str, port: u16) -> Snapshot {
    match oob_request(host, port, b"getstatus", b"statusResponse", 2.0).await {
        Some(d) => parse_status(&d),
        None => Snapshot { ts: now(), online: false, ..Default::default() },
    }
}

/// Like `oob_request`, but keeps reading: an rcon answer longer than about 1 KB (a `status` with a full server) arrives in
/// several "print" packets. Waits up to `first` for the first one, then until `idle` passes with nothing new (at most 4 s).
async fn oob_collect(host: &str, port: u16, payload: &[u8], expect: &[u8], first: f64, idle: f64) -> Option<Vec<u8>> {
    let addr = tokio::net::lookup_host((host, port)).await.ok()?.next()?;
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = UdpSocket::bind(bind).await.ok()?;
    sock.connect(addr).await.ok()?;
    let mut msg = OOB.to_vec();
    msg.extend_from_slice(payload);
    sock.send(&msg).await.ok()?;
    let hard = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut wait = Duration::from_secs_f64(first);
    let mut out: Option<Vec<u8>> = None;
    let mut buf = vec![0u8; 65535];
    loop {
        let left = hard.saturating_duration_since(tokio::time::Instant::now()).min(wait);
        if left.is_zero() {
            return out;
        }
        match tokio::time::timeout(left, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                let data = &buf[..n];
                if data.starts_with(OOB) && data[OOB.len()..].starts_with(expect) {
                    out.get_or_insert_with(Vec::new).extend_from_slice(&data[OOB.len() + expect.len()..]);
                    wait = Duration::from_secs_f64(idle);
                }
            }
            _ => return out,
        }
    }
}

/// Send one rcon command without waiting for an answer (most admin commands print nothing). True if it was sent.
pub async fn rcon_send(host: &str, port: u16, password: &str, command: &str) -> bool {
    let Some(addr) = tokio::net::lookup_host((host, port)).await.ok().and_then(|mut a| a.next()) else { return false };
    let bind = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let Ok(sock) = UdpSocket::bind(bind).await else { return false };
    if sock.connect(addr).await.is_err() {
        return false;
    }
    let mut msg = OOB.to_vec();
    msg.extend_from_slice(format!("rcon {} {}", password, command).as_bytes());
    sock.send(&msg).await.is_ok()
}

/// Send one rcon command and return everything the server printed in answer (all packets), or None if it said nothing.
pub async fn rcon_collect(host: &str, port: u16, password: &str, command: &str) -> Option<String> {
    let payload = format!("rcon {} {}", password, command);
    let data = oob_collect(host, port, payload.as_bytes(), b"print", 1.5, 0.3).await?;
    Some(String::from_utf8_lossy(&data).into_owned())
}

pub async fn rcon(host: &str, port: u16, password: &str, command: &str) -> Option<String> {
    let payload = format!("rcon {} {}", password, command);
    let data = oob_request(host, port, payload.as_bytes(), b"print", 1.5).await?;
    Some(String::from_utf8_lossy(&data[OOB.len() + 5..]).into_owned())
}

// ---- games.log parsing ----

const WORLD: i32 = 1022;

fn team_name(code: &str) -> &'static str {
    match code {
        "1" => "red",
        "2" => "blue",
        "3" => "spec",
        _ => "free",
    }
}

fn flag_action(n: i32) -> Option<&'static str> {
    match n {
        0 => Some("dropped"),
        1 => Some("returned"),
        2 => Some("captured"),
        _ => None,
    }
}

fn info_map(s: &str) -> HashMap<String, String> {
    let parts: Vec<&str> = s.trim_start_matches('\\').split('\\').collect();
    parts.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].to_string(), c[1].to_string())).collect()
}

#[derive(Default)]
pub struct LogParser {
    pub show_joins: bool,
    pub slots: HashMap<i32, String>,
    pub teams: HashMap<String, String>,
    last_hit: HashMap<(i32, i32), (f64, bool)>,
    last_init: f64,
}

impl LogParser {
    pub fn new(show_joins: bool) -> Self {
        LogParser { show_joins, ..Default::default() }
    }

    /// Client number of a (colour-stripped) player name, if known.
    pub fn slot_for(&self, name: &str) -> Option<i32> {
        let n = name.to_lowercase();
        self.slots.iter().find(|(_, v)| v.to_lowercase() == n).map(|(k, _)| *k)
    }

    fn name(&self, slot: i32, fallback: &str) -> String {
        self.slots.get(&slot).cloned().unwrap_or_else(|| strip_colors(fallback))
    }

    pub fn feed(&mut self, raw: &str) -> Vec<Event> {
        self.feed_at(raw, now())
    }

    pub fn feed_at(&mut self, raw: &str, now: f64) -> Vec<Event> {
        let trimmed = raw.trim();
        let line = ts_re().replace(trimmed, "").into_owned();
        let line = line.as_str();

        if line.starts_with("InitGame:") {
            self.last_init = now;
            self.slots.clear();
            self.teams.clear();
            self.last_hit.clear();
            let map = Regex::new(r"\\mapname\\([^\\]+)").unwrap().captures(line).map(|c| c[1].to_string());
            let gt = Regex::new(r"\\g_gametype\\(\d+)").unwrap().captures(line).and_then(|c| c[1].parse::<i32>().ok());
            return match map {
                Some(m) => {
                    let label = gt.and_then(known_gametype).map(|g| format!(" ({})", g)).unwrap_or_default();
                    let mut e = Event::new(now, Kind::Map, format!("🗺️ New map: **{}**{}", esc(&m), label));
                    e.data.map = Some(m);
                    vec![e]
                }
                None => vec![],
            };
        }

        if line.starts_with("ClientUserinfoChanged:") {
            let Some(m) = Regex::new(r"^ClientUserinfoChanged: (\d+) (.*)").unwrap().captures(line) else { return vec![] };
            let slot: i32 = m[1].parse().unwrap_or(-1);
            let kv = info_map(&m[2]);
            let name = strip_colors(kv.get("n").map(String::as_str).unwrap_or(""));
            if name.is_empty() {
                return vec![];
            }
            let team = team_name(kv.get("t").map(String::as_str).unwrap_or("0"));
            let is_new = !self.slots.contains_key(&slot);
            let old_team = self.teams.get(&name.to_lowercase()).cloned();
            self.slots.insert(slot, name.clone());
            self.teams.insert(name.to_lowercase(), team.to_string());
            let quiet = now - self.last_init < 15.0;
            if is_new && self.show_joins && !quiet {
                let mut e = Event::new(now, Kind::Join, format!("➕ **{}** joined", esc(&name)));
                e.data.name = Some(name);
                return vec![e];
            }
            if let Some(old) = old_team {
                if old != team && !quiet {
                    let label = match team {
                        "red" => "🔴 Red",
                        "blue" => "🔵 Blue",
                        "spec" => "👁️ Spectators",
                        _ => "the game",
                    };
                    let mut e = Event::new(now, Kind::Team, format!("🔁 **{}** moved to {}", esc(&name), label));
                    e.data.name = Some(name);
                    return vec![e];
                }
            }
            return vec![];
        }

        if line.starts_with("ClientDisconnect:") {
            if let Some(m) = Regex::new(r"^ClientDisconnect: (\d+)").unwrap().captures(line) {
                let slot: i32 = m[1].parse().unwrap_or(-1);
                if let Some(name) = self.slots.remove(&slot) {
                    self.teams.remove(&name.to_lowercase());
                    if self.show_joins {
                        let mut e = Event::new(now, Kind::Leave, format!("➖ **{}** left", esc(&name)));
                        e.data.name = Some(name);
                        return vec![e];
                    }
                }
            }
            return vec![];
        }

        if line.starts_with("Hit:") {
            if let Some(m) = hit_re().captures(line) {
                let (victim, attacker): (i32, i32) = (m[1].parse().unwrap_or(-1), m[2].parse().unwrap_or(-1));
                let loc = m[5].to_lowercase();
                self.last_hit.insert((attacker, victim), (now, loc == "head" || loc == "helmet"));
            }
            return vec![];
        }

        if line.starts_with("Kill:") {
            let Some(m) = kill_re().captures(line) else { return vec![] };
            let (k_id, v_id): (i32, i32) = (m[1].parse().unwrap_or(-1), m[2].parse().unwrap_or(-1));
            let killer = self.name(k_id, &m[4]);
            let victim = self.name(v_id, &m[5]);
            let weapon = weapon_name(&m[6]);
            let hit = self.last_hit.remove(&(k_id, v_id));
            let hs = matches!(hit, Some((t, h)) if now - t < 1.0 && h);
            let solo = k_id == WORLD || k_id == v_id || killer == "<world>";
            let text = if solo {
                format!("💀 **{}** died ({})", esc(&victim), weapon)
            } else {
                format!("🔫 **{}** ▸ {}  ·  *{}*{}", esc(&killer), esc(&victim), weapon, if hs { " 🎯" } else { "" })
            };
            let mut e = Event::new(now, Kind::Kill, text);
            e.data = EventData {
                killer: if solo { None } else { Some(killer) },
                victim: Some(victim),
                killer_slot: if solo { None } else { Some(k_id) },
                victim_slot: Some(v_id),
                weapon: Some(weapon),
                hs,
                ..Default::default()
            };
            return vec![e];
        }

        // Player chat (say / sayteam / tell) is deliberately NOT turned into an event: it must not reach Discord.
        if line.starts_with("say:") || line.starts_with("sayteam:") || line.starts_with("tell:") || line.starts_with("sayteamchat:") {
            return vec![];
        }

        if line.starts_with("Flag:") {
            if let Some(m) = flag_re().captures(line) {
                let slot: i32 = m[1].parse().unwrap_or(-1);
                let action: i32 = m[2].parse().unwrap_or(-1);
                let flag = m[3].to_lowercase();
                let who = self.slots.get(&slot).cloned().unwrap_or_else(|| format!("#{}", slot));
                let colour = if flag.contains("red") { "🔴 red" } else if flag.contains("blue") { "🔵 blue" } else { "" };
                let verb = flag_action(action).map(str::to_string).unwrap_or_else(|| format!("touched (action {})", action));
                let icon = if verb == "captured" { "🏆" } else { "🚩" };
                let text = format!("{} **{}** {} the {} flag", icon, esc(&who), verb, colour).replace("  ", " ");
                let mut e = Event::new(now, Kind::Flag, text);
                e.data.slot = Some(slot);
                return vec![e];
            }
            return vec![];
        }

        if let Some(m) = Regex::new(r"^Flag Return: (\w+)").unwrap().captures(line) {
            return vec![Event::new(now, Kind::Flag, format!("🔄 The **{}** flag returned to base", &m[1]))];
        }

        if let Some(m) = Regex::new(r"^Bomb was (planted|defused) by (\d+)").unwrap().captures(line) {
            let slot: i32 = m[2].parse().unwrap_or(-1);
            let who = self.slots.get(&slot).cloned().unwrap_or_else(|| format!("#{}", &m[2]));
            let icon = if &m[1] == "planted" { "💣" } else { "✂️" };
            let mut e = Event::new(now, Kind::Flag, format!("{} **{}** {} the bomb", icon, esc(&who), &m[1]));
            e.data.slot = Some(slot);
            return vec![e];
        }

        if let Some(m) = Regex::new(r"^SurvivorWinner: (.+)$").unwrap().captures(line) {
            return vec![Event::new(now, Kind::Round, format!("🏆 Round won by **{}**", esc(m[1].trim())))];
        }

        if let Some(rest) = line.strip_prefix("Exit:") {
            return vec![Event::new(now, Kind::Exit, format!("🏁 {}", esc(rest.trim())))];
        }

        if let Some(m) = Regex::new(r"^red:(-?\d+)\s+blue:(-?\d+)").unwrap().captures(line) {
            return vec![Event::new(now, Kind::Info, format!("📊 Final score: 🔴 **{}** – **{}** 🔵", &m[1], &m[2]))];
        }

        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(p: &mut LogParser, slot: i32, name: &str, team: u8, t: f64) {
        p.feed_at(&format!("  0:01 ClientUserinfoChanged: {} n\\{}\\t\\{}\\r\\1", slot, name, team), t);
    }

    #[test]
    fn colours_and_markdown() {
        assert_eq!(strip_colors("^1Red^7Dog ^x"), "RedDog");
        assert_eq!(strip_colors("^^1x"), "^x");
        assert_eq!(esc("a_b*c"), "a\\_b\\*c");
    }

    #[test]
    fn weapon_names() {
        assert_eq!(weapon_name("UT_MOD_DEAGLE"), "Desert Eagle");
        assert_eq!(weapon_name("MOD_KNIFE_THROWN"), "Thrown knife");
        assert_eq!(weapon_name("UT_MOD_SOME_NEW_GUN"), "Some New Gun");
    }

    #[test]
    fn status_response() {
        let mut d = OOB.to_vec();
        d.extend_from_slice(b"statusResponse\n\\sv_hostname\\^2My ^7Server\\mapname\\ut4_turnpike\\g_gametype\\7\\sv_maxclients\\16\\score_red\\3\\score_blue\\1\n5 40 \"^1Alice\"\n-1 999 \"Bob\"\n");
        let s = parse_status(&d);
        assert!(s.online);
        assert_eq!(s.hostname, "My Server");
        assert_eq!(s.mapname, "ut4_turnpike");
        assert_eq!(s.gametype, 7);
        assert_eq!(gametype_name(s.gametype), "CTF");
        assert_eq!((s.red, s.blue, s.max_clients), (Some(3), Some(1), 16));
        assert_eq!(s.players.len(), 2);
        assert_eq!(s.players[0], Player { name: "Alice".into(), score: 5, ping: 40 });
        assert_eq!(s.players[1].score, -1);
    }

    #[test]
    fn kills_hits_and_headshots() {
        let mut p = LogParser::new(true);
        join(&mut p, 0, "Alice", 1, 0.0);
        join(&mut p, 1, "Bob", 2, 0.0);
        p.feed_at("  1:00 Hit: 1 0 0 5: Alice hit Bob in the Head", 100.0);
        let ev = p.feed_at("  1:00 Kill: 0 1 19: Alice killed Bob by UT_MOD_DEAGLE", 100.2);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, Kind::Kill);
        assert!(ev[0].data.hs);
        assert!(ev[0].text.contains("Alice") && ev[0].text.contains("Desert Eagle") && ev[0].text.contains("🎯"));
        assert_eq!(ev[0].data.killer_slot, Some(0));
        // a hit from long ago does not count
        p.feed_at("  1:05 Hit: 1 0 0 5: Alice hit Bob in the Head", 100.0);
        let ev = p.feed_at("  1:09 Kill: 0 1 19: Alice killed Bob by UT_MOD_DEAGLE", 110.0);
        assert!(!ev[0].data.hs);
    }

    #[test]
    fn world_and_suicide_kills_have_no_killer() {
        let mut p = LogParser::new(true);
        join(&mut p, 3, "Zed", 0, 0.0);
        let ev = p.feed_at("Kill: 1022 3 22: <world> killed Zed by MOD_FALLING", 50.0);
        assert!(ev[0].text.contains("died") && ev[0].data.killer.is_none());
        let ev = p.feed_at("Kill: 3 3 7: Zed killed Zed by MOD_SUICIDE", 51.0);
        assert!(ev[0].data.killer_slot.is_none());
    }

    #[test]
    fn joins_leaves_and_team_moves_respect_the_quiet_period() {
        let mut p = LogParser::new(true);
        p.feed_at("InitGame: \\mapname\\ut4_abbey\\g_gametype\\7", 100.0);
        // inside 15 s of a map change: silent
        assert!(p.feed_at("ClientUserinfoChanged: 0 n\\Alice\\t\\1", 105.0).is_empty());
        // later: a new client announces itself
        let ev = p.feed_at("ClientUserinfoChanged: 1 n\\Bob\\t\\2", 200.0);
        assert_eq!(ev[0].kind, Kind::Join);
        let ev = p.feed_at("ClientUserinfoChanged: 1 n\\Bob\\t\\1", 210.0);
        assert_eq!(ev[0].kind, Kind::Team);
        let ev = p.feed_at("ClientDisconnect: 1", 220.0);
        assert_eq!(ev[0].kind, Kind::Leave);
        assert!(p.slots.get(&1).is_none());
        assert_eq!(p.slot_for("alice"), Some(0));
        let mut quiet = LogParser::new(false);
        assert!(quiet.feed_at("ClientUserinfoChanged: 1 n\\Bob\\t\\2", 200.0).is_empty());
    }

    #[test]
    fn map_change_event_names_the_gametype() {
        let mut p = LogParser::new(true);
        let ev = p.feed_at("  0:00 InitGame: \\sv_hostname\\x\\g_gametype\\4\\mapname\\ut4_casa", 1.0);
        assert_eq!(ev[0].kind, Kind::Map);
        assert!(ev[0].text.contains("ut4\\_casa") && ev[0].text.contains("Team Survivor"));
    }

    #[test]
    fn player_chat_never_becomes_a_feed_event() {
        let mut p = LogParser::new(true);
        assert!(p.feed_at("say: 0 console: hello from discord", 1.0).is_empty());
        assert!(p.feed_at("say: 2 ^1Alice: gg everyone", 1.0).is_empty());
        assert!(p.feed_at("sayteam: 2 ^1Alice: rush b", 1.0).is_empty());
        assert!(p.feed_at("tell: 2 ^1Alice: psst", 1.0).is_empty());
    }

    #[test]
    fn flags_bombs_rounds_and_exit() {
        let mut p = LogParser::new(true);
        join(&mut p, 4, "Cap", 1, 0.0);
        let ev = p.feed_at("Flag: 4 2: team_CTF_blueflag", 9.0);
        assert!(ev[0].text.contains("captured") && ev[0].text.contains("🔵 blue") && ev[0].text.contains('🏆'));
        assert_eq!(ev[0].data.slot, Some(4));
        assert!(p.feed_at("Flag Return: RED", 9.0)[0].text.contains("returned to base"));
        assert!(p.feed_at("Bomb was planted by 4", 9.0)[0].text.contains("planted the bomb"));
        assert!(p.feed_at("SurvivorWinner: Red", 9.0)[0].text.contains("Round won"));
        assert_eq!(p.feed_at("Exit: Timelimit hit.", 9.0)[0].kind, Kind::Exit);
        assert!(p.feed_at("red:8  blue:5", 9.0)[0].text.contains("**8** – **5**"));
        assert!(p.feed_at("Item: 3 ut_weapon_ump45", 9.0).is_empty());
    }
}
