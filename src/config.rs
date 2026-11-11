// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Configuration: the same JSON format as the Python version (comments allowed) plus an `admin` section.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

fn string_or_int_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v: Vec<serde_json::Value> = Vec::deserialize(d)?;
    Ok(v.into_iter()
        .map(|x| match x {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
        .collect())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordConfig {
    pub token: String,
    /// If non-empty, ONLY these Discord user ids may control broadcasts.
    #[serde(deserialize_with = "string_or_int_vec")]
    pub restrict_to_user_ids: Vec<String>,
    /// Start a fresh scoreboard + feed post when the map changes; the old one is closed with its final result.
    pub new_post_each_map: bool,
    /// With `new_post_each_map`: remove the previous map's scoreboard and feed instead of leaving them in the channel
    /// (the "Match finished" summary stays).
    pub delete_old_posts: bool,
    /// Discord role ids (numbers) whose members may run the `/gtv rcon` commands (cyclemap, slap, kick, bigtext, ban) on servers
    /// that have `rcon_commands` switched on. Empty = nobody can, and the command is not offered.
    #[serde(deserialize_with = "string_or_int_vec")]
    pub rcon_roles: Vec<String>,
    /// Discord user ids (numbers) of individual people who may run the `/gtv rcon` commands, whatever roles they have. Works
    /// together with `rcon_roles`: a person is allowed when they are listed here OR have one of the roles. Both empty = nobody.
    #[serde(deserialize_with = "string_or_int_vec")]
    pub rcon_users: Vec<String>,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        DiscordConfig { token: String::new(), restrict_to_user_ids: vec![], new_post_each_map: true, delete_old_posts: false, rcon_roles: vec![], rcon_users: vec![] }
    }
}

impl DiscordConfig {
    /// The role ids as numbers (anything that is not a number is ignored; the configuration check names it).
    pub fn rcon_role_ids(&self) -> Vec<u64> {
        self.rcon_roles.iter().filter_map(|r| r.trim().parse::<u64>().ok()).filter(|r| *r != 0).collect()
    }

    /// The people chosen for rcon, as numbers (anything that is not a number is ignored; the configuration check names it).
    pub fn rcon_user_ids(&self) -> Vec<u64> {
        self.rcon_users.iter().filter_map(|r| r.trim().parse::<u64>().ok()).filter(|r| *r != 0).collect()
    }
}

/// Make the bot look like the clan's. The footer on every post ("The Forge GTV vX - owner [TFG]Aciddrop") is part of the
/// licence terms and is NOT configurable.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BrandingConfig {
    /// Clan / community name shown above the title of the Discord posts and in the player's header.
    pub name: String,
    /// Logo: a file uploaded in the admin panel, or an https:// address of a picture. Top-right corner of the posts.
    pub logo: String,
    /// Small icon: a file or address. Next to the clan name, and the web player's tab icon.
    pub icon: String,
    /// Accent colour `#RRGGBB` (feed, summaries, player cards); the live scoreboard keeps red/blue for the leading team.
    pub color: String,
}

/// The picture of the map being played, shown on the scoreboard post - always the same square size, so a channel with
/// several broadcasts stays tidy.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LevelshotConfig {
    pub enabled: bool,
    /// Width and height in pixels (128 - 640).
    pub size: u32,
    /// How a picture that is not square becomes square: `crop` (fill, cut the edges) or `pad` (whole picture, dark bars).
    pub fit: String,
}

impl Default for LevelshotConfig {
    fn default() -> Self {
        LevelshotConfig { enabled: true, size: crate::levelshot::DEFAULT_SIZE, fit: "crop".into() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub host: String,
    pub port: u16,
    pub public_url: String,
    pub token: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig { host: "0.0.0.0".into(), port: 8080, public_url: String::new(), token: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// `sha256i$<salt>$<hash>`; empty means "not set up yet" (the panel then refuses to start).
    pub password_hash: String,
    /// Only with remote access: if not empty, only these addresses / ranges (`203.0.113.7`, `10.0.0.0/8`) may reach the
    /// panel. This computer itself is always allowed.
    pub allowed_ips: Vec<String>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        AdminConfig { enabled: true, host: "127.0.0.1".into(), port: 8787, password_hash: String::new(), allowed_ips: vec![] }
    }
}

pub const REMOTE_HOST: &str = "0.0.0.0";
pub const LOCAL_HOST: &str = "127.0.0.1";
/// Shortest password allowed while the panel is reachable from other computers.
pub const REMOTE_MIN_PASSWORD: usize = 12;

pub fn is_loopback_host(h: &str) -> bool {
    let t = h.trim().trim_start_matches('[').trim_end_matches(']').to_lowercase();
    t == "localhost" || t.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl AdminConfig {
    /// True when the panel listens on more than this computer.
    pub fn remote(&self) -> bool {
        !is_loopback_host(&self.host)
    }
}

/// One entry of `allowed_ips`: a single address or `address/bits`.
pub fn parse_cidr(s: &str) -> Option<(std::net::IpAddr, u8)> {
    let t = s.trim();
    let (addr, bits) = match t.split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (t, None),
    };
    let ip: std::net::IpAddr = addr.trim().parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits = bits.unwrap_or(max);
    (bits <= max).then_some((ip, bits))
}

/// Is `ip` inside any of the listed addresses / ranges? (IPv4-mapped IPv6 peers are treated as IPv4.)
pub fn ip_allowed(list: &[String], ip: std::net::IpAddr) -> bool {
    let ip = match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    list.iter().filter_map(|e| parse_cidr(e)).any(|(net, bits)| match (net, ip) {
        (std::net::IpAddr::V4(n), std::net::IpAddr::V4(a)) => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            u32::from(n) & mask == u32::from(a) & mask
        }
        (std::net::IpAddr::V6(n), std::net::IpAddr::V6(a)) => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            u128::from(n) & mask == u128::from(a) & mask
        }
        _ => false,
    })
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct FsConfig {
    #[serde(rename = "type")]
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub password_env: String,
    pub key_file: String,
    pub known_hosts: String,
    pub trust_unknown_hosts: bool,
}

impl Default for FsConfig {
    fn default() -> Self {
        FsConfig {
            kind: "sftp".into(), host: String::new(), port: 22, user: String::new(), password: String::new(),
            password_env: String::new(), key_file: String::new(), known_hosts: String::new(),
            trust_unknown_hosts: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct SpectatorConfig {
    pub client_binary: String,
    pub base_path: String,
    pub game_dir: String,
    pub client_name: String,
    pub password: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    /// hls | rtmp | both
    pub output: String,
    pub rtmp_url: String,
    pub watch_url: String,
    /// Local dir or "fsname:/remote/dir" (static hosting).
    pub publish_to: String,
    /// Give this server's video player a web port of its own (for example 8081), so each server streams on a different
    /// port. 0 = share the main web player port (`web.port`), as before. Needs the `web` section; the port opens when
    /// the video starts and closes when it stops.
    pub web_port: u16,
    /// Address viewers use for this server's player, if it differs from `web.public_url` with this port - for example
    /// `https://gtv2.example.com` behind a reverse proxy. Empty = work it out.
    pub public_url: String,
    pub audio: String,
    pub pulse_source: String,
    pub software_gl: bool,
    pub work_dir: String,
    pub segment_seconds: u32,
    pub x264_preset: String,
    /// cpu | auto | nvenc | vaapi | qsv | amf  (auto = best GPU encoder that passes a test encode, else CPU)
    pub encoder: String,
    /// VAAPI render device (Linux).
    pub vaapi_device: String,
    pub display_base: u32,
    pub manage_display: bool,
    pub display: String,
    pub director: bool,
    /// Hide the in-game chat (and console notices) on the spectator client so it never appears in the video.
    pub hide_chat: bool,
    /// Keep the client a spectator: after it joins, send `team s` until the server agrees (never pick a team).
    pub force_spectator: bool,
    /// Keys pressed in the game, in order, once it has joined and after every map load: by default Escape (clears the
    /// join-team menu) then 4 (the menu's "spectate" entry). Space separated; "" = off.
    pub spectate_key: String,
    /// Seconds after joining / a map load before Escape is pressed (the other spectate keys go at once, after the game
    /// has settled). 0 = Escape goes first together with the others.
    pub escape_delay: f64,
    pub hold_seconds: f64,
    pub idle_seconds: f64,
    pub xvfb_binary: String,
    pub ffmpeg_binary: String,
    pub xdotool_binary: String,
    pub extra_client_args: Vec<String>,
    /// Windows only (experimental): title of the client window to capture instead of the whole desktop.
    pub window_title: String,
}

impl Default for SpectatorConfig {
    fn default() -> Self {
        SpectatorConfig {
            client_binary: String::new(), base_path: String::new(), game_dir: "q3ut4".into(),
            client_name: "^3GTV".into(), password: String::new(), width: 1280, height: 720, fps: 30,
            video_kbps: 3000, output: "hls".into(), rtmp_url: String::new(), watch_url: String::new(),
            publish_to: String::new(), web_port: 0, public_url: String::new(), audio: "silent".into(), pulse_source: "gtv.monitor".into(),
            software_gl: true, work_dir: "./stream".into(), segment_seconds: 2, x264_preset: "veryfast".into(), encoder: "cpu".into(), vaapi_device: "/dev/dri/renderD128".into(),
            display_base: 90, manage_display: true, display: String::new(), director: true, hide_chat: true, force_spectator: true, spectate_key: "Escape 4".into(), escape_delay: 60.0,
            hold_seconds: 4.0, idle_seconds: 10.0, xvfb_binary: "Xvfb".into(), ffmpeg_binary: "ffmpeg".into(),
            xdotool_binary: "xdotool".into(), extra_client_args: vec![], window_title: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(skip)]
    pub key: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub rcon_password: String,
    /// Local path or "fsname:/remote/games.log".
    pub log_path: String,
    /// Where finished-match summaries (JSON) are written; local dir or "fs:/dir".
    pub archive_dir: String,
    pub poll_interval: f64,
    pub relay_chat: bool,
    pub show_joins: bool,
    /// Let members of the Discord admin role (`discord.rcon_roles`) run `/gtv rcon` commands on this server. Only ever
    /// active together with `rcon_password`.
    pub rcon_commands: bool,
    /// Where to look for this server's map pictures, besides the game folder of the spectator video: a folder holding
    /// `<map>.jpg` files, `levelshots/<map>.jpg`, or `.pk3` files. Empty = only the spectator's `base_path/game_dir`.
    pub levelshot_dir: String,
    pub spectator: Option<SpectatorConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            key: String::new(), name: String::new(), host: String::new(), port: 27960,
            rcon_password: String::new(), log_path: String::new(), archive_dir: String::new(),
            poll_interval: 2.0, relay_chat: false, show_joins: true, rcon_commands: false, levelshot_dir: String::new(), spectator: None,
        }
    }
}

impl ServerConfig {
    /// Discord `/gtv rcon` commands are possible on this server: switched on AND the rcon password is entered.
    pub fn rcon_enabled(&self) -> bool {
        self.rcon_commands && !self.rcon_password.trim().is_empty()
    }

    /// The folders searched for this server's map pictures, in order.
    pub fn levelshot_sources(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if !self.levelshot_dir.trim().is_empty() {
            v.push(expand_tilde(self.levelshot_dir.trim()));
        }
        if let Some(sp) = &self.spectator {
            if !sp.base_path.trim().is_empty() {
                let game = if sp.game_dir.trim().is_empty() { "q3ut4" } else { sp.game_dir.trim() };
                v.push(expand_tilde(sp.base_path.trim()).join(game));
            }
        }
        v
    }

    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.key } else { &self.name }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub discord: DiscordConfig,
    pub web: Option<WebConfig>,
    pub admin: AdminConfig,
    pub branding: BrandingConfig,
    pub levelshots: LevelshotConfig,
    pub filesystems: IndexMap<String, FsConfig>,
    pub data_dir: String,
    pub log_file: String,
    pub servers: IndexMap<String, ServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            discord: DiscordConfig::default(), web: None, admin: AdminConfig::default(), branding: BrandingConfig::default(), levelshots: LevelshotConfig::default(),
            filesystems: IndexMap::new(), data_dir: "./data".into(), log_file: String::new(),
            servers: IndexMap::new(),
        }
    }
}

/// Remove `#` and `//` comments (full-line or trailing); string contents are left alone.
pub fn strip_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut in_str, mut esc) = (0, false, false);
    while i < chars.len() {
        let ch = chars[i];
        if in_str {
            out.push(ch);
            if esc { esc = false } else if ch == '\\' { esc = true } else if ch == '"' { in_str = false }
        } else if ch == '"' {
            in_str = true;
            out.push(ch);
        } else if ch == '#' || (ch == '/' && chars.get(i + 1) == Some(&'/')) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else {
            out.push(ch);
        }
        i += 1;
    }
    out
}

/// `#RRGGBB` (or `RRGGBB`) -> 0xRRGGBB.
pub fn parse_color(s: &str) -> Option<u32> {
    let t = s.trim().trim_start_matches('#');
    (t.len() == 6 && t.chars().all(|c| c.is_ascii_hexdigit())).then(|| u32::from_str_radix(t, 16).ok()).flatten()
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 32 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let mut cfg: Config = serde_json::from_str(&strip_comments(text)).context("the configuration is not valid")?;
        cfg.fix_keys();
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn fix_keys(&mut self) {
        for (k, s) in self.servers.iter_mut() {
            s.key = k.clone();
            if s.poll_interval < 1.0 {
                s.poll_interval = 1.0;
            }
        }
    }

    /// Everything wrong with this configuration, in plain words (empty = fine).
    pub fn problems(&self) -> Vec<String> {
        self.problems_on(crate::spectator::Platform::current())
    }

    /// `problems()` as it would be on another operating system (the video limits differ).
    pub fn problems_on(&self, platform: crate::spectator::Platform) -> Vec<String> {
        let mut p = Vec::new();
        // Linux gives every video its own virtual screen; Windows has one desktop that every client window sits on
        if platform == crate::spectator::Platform::Windows {
            let with_video: Vec<&str> = self.servers.iter().filter(|(_, s)| s.spectator.is_some()).map(|(k, _)| k.as_str()).collect();
            if with_video.len() > 1 {
                p.push(format!(
                    "servers {} all have video, but on Windows every game client opens on the same desktop (top-left corner), so their videos cannot run at the same time - broadcast one video at a time (scoreboards for any number of servers are fine). Several videos at once is only supported on Linux",
                    with_video.join(", ")
                ));
            }
        }
        if let Some(w) = &self.web {
            match fix_public_url(&w.public_url) {
                Err(e) => p.push(format!("web.public_url: {}", e)),
                Ok(f) if f != w.public_url.trim() => p.push(format!("web.public_url '{}' is mistyped - the bot uses '{}' instead; open Settings and Save to correct it", w.public_url, f)),
                _ => {}
            }
        }
        for r in &self.discord.rcon_roles {
            if r.trim().parse::<u64>().map_or(true, |n| n == 0) {
                p.push(format!("discord.rcon_roles: '{}' is not a role id (a long number - in Discord with Developer Mode on, right-click the role, Copy Role ID)", r));
            }
        }
        for u in &self.discord.rcon_users {
            if u.trim().parse::<u64>().map_or(true, |n| n == 0) {
                p.push(format!("discord.rcon_users: '{}' is not a user id (a long number - in Discord with Developer Mode on, right-click the person, Copy User ID)", u));
            }
        }
        let lv = &self.levelshots;
        if !(crate::levelshot::MIN_SIZE..=crate::levelshot::MAX_SIZE).contains(&lv.size) {
            p.push(format!("levelshots.size {} must be between {} and {} pixels", lv.size, crate::levelshot::MIN_SIZE, crate::levelshot::MAX_SIZE));
        }
        if crate::levelshot::Fit::parse(&lv.fit).is_none() {
            p.push(format!("levelshots.fit '{}' must be crop or pad", lv.fit));
        }
        let could_find = self.servers.values().any(|s| !s.levelshot_sources().is_empty());
        if lv.enabled && could_find && self.web.as_ref().is_none_or(|w| fix_public_url(&w.public_url).unwrap_or_default().is_empty()) {
            p.push("levelshots: the map pictures are shown through the web player's public address - set the \"web\" section's public_url (Settings tab), or turn the pictures off (Branding tab)".into());
        }
        let b = &self.branding;
        if b.name.chars().count() > 40 || b.name.contains(['\n', '\r']) {
            p.push("branding.name: use up to 40 characters on one line".into());
        }
        if !b.color.is_empty() && parse_color(&b.color).is_none() {
            p.push(format!("branding.color '{}' is not a colour - use the form #RRGGBB, for example #E67E22", b.color));
        }
        for (field, v) in [("logo", &b.logo), ("icon", &b.icon)] {
            if !v.is_empty() && !crate::brand::valid_asset_ref(v) {
                p.push(format!("branding.{} must be a picture uploaded in the admin panel or an https:// address", field));
            }
        }
        for e in &self.admin.allowed_ips {
            if parse_cidr(e).is_none() {
                p.push(format!("admin.allowed_ips: '{}' is not an address like 203.0.113.7 or a range like 10.0.0.0/8", e));
            }
        }
        if self.admin.enabled && self.admin.remote() && self.admin.password_hash.is_empty() {
            p.push("admin.host allows other computers, but no admin password is set yet - set it first on this computer (open http://127.0.0.1:PORT)".into());
        }
        for (k, s) in &self.servers {
            let w = format!("servers.{}", k);
            if !valid_key(k) {
                p.push(format!("{}: the key may only contain letters, digits, '-' and '_' (max 32)", w));
            }
            if s.host.trim().is_empty() {
                p.push(format!("{}.host is empty", w));
            }
            for (field, spec) in [("log_path", &s.log_path), ("archive_dir", &s.archive_dir)] {
                if let Some(fsname) = fs_name_of(spec) {
                    if !self.filesystems.contains_key(&fsname) {
                        p.push(format!("{}.{} uses filesystem '{}', which is not defined", w, field, fsname));
                    }
                }
            }
            if s.relay_chat && s.rcon_password.is_empty() {
                p.push(format!("{}: relay_chat needs rcon_password", w));
            }
            if s.rcon_commands && s.rcon_password.trim().is_empty() {
                p.push(format!("{}: rcon_commands needs rcon_password (the /gtv rcon commands stay off until it is entered)", w));
            }
            if s.rcon_commands && !s.rcon_password.trim().is_empty() && self.discord.rcon_role_ids().is_empty() && self.discord.rcon_user_ids().is_empty() {
                p.push(format!("{}: rcon_commands is on, but no admin role and no person is chosen (discord.rcon_roles / discord.rcon_users), so nobody can use /gtv rcon", w));
            }
            if let Some(sp) = &s.spectator {
                let sw = format!("{}.spectator", w);
                if sp.client_binary.is_empty() { p.push(format!("{}.client_binary is empty", sw)); }
                if sp.base_path.is_empty() { p.push(format!("{}.base_path is empty", sw)); }
                if !["hls", "rtmp", "both"].contains(&sp.output.as_str()) {
                    p.push(format!("{}.output must be hls, rtmp or both", sw));
                }
                if (sp.output == "rtmp" || sp.output == "both") && sp.rtmp_url.is_empty() {
                    p.push(format!("{}: output includes rtmp but rtmp_url is empty", sw));
                }
                if (sp.output == "hls" || sp.output == "both") && self.web.is_none() {
                    p.push(format!("{}: output includes hls but there is no \"web\" section", sw));
                }
                if !(0.0..=600.0).contains(&sp.escape_delay) {
                    p.push(format!("{}.escape_delay must be between 0 and 600 seconds", sw));
                }
                if crate::spectator::spectate_keys(&sp.spectate_key).is_none() {
                    p.push(format!("{}.spectate_key must be up to 4 key names separated by spaces, such as \"Escape 4\" or \"F5\" (letters, digits, _) - or empty", sw));
                }
                match crate::gpu::parse_choice(&sp.encoder) {
                    None => p.push(format!("{}.encoder must be cpu, auto, nvenc, vaapi, qsv or amf", sw)),
                    Some(crate::gpu::Choice::Fixed(e)) if !e.supported_on(crate::spectator::Platform::current()) =>
                        p.push(format!("{}.encoder {} is not available on this operating system", sw, e.setting())),
                    _ => {}
                }
                if sp.width < 160 || sp.height < 120 || sp.fps == 0 {
                    p.push(format!("{}: width/height/fps are too small", sw));
                }
                if let Some(fsname) = fs_name_of(&sp.publish_to) {
                    if !self.filesystems.contains_key(&fsname) {
                        p.push(format!("{}.publish_to uses filesystem '{}', which is not defined", sw, fsname));
                    }
                }
                if sp.web_port != 0 {
                    if self.web.is_none() {
                        p.push(format!("{}.web_port is set but there is no \"web\" section - add one (Settings tab) so the player can run", sw));
                    }
                    if self.web.as_ref().is_some_and(|w| w.port == sp.web_port) {
                        p.push(format!("{}.web_port {} is the main web player's port - pick a different one (or 0 to share it)", sw, sp.web_port));
                    }
                    if self.admin.enabled && self.admin.port == sp.web_port {
                        p.push(format!("{}.web_port {} is the admin panel's port - pick a different one", sw, sp.web_port));
                    }
                    if sp.output == "rtmp" {
                        p.push(format!("{}: web_port has no effect with output \"rtmp\" (there is no web player)", sw));
                    }
                    if let Some((other, _)) = self.servers.iter().find(|(ok, o)| *ok != k && k.as_str() < ok.as_str() && o.spectator.as_ref().is_some_and(|x| x.web_port == sp.web_port)) {
                        p.push(format!("{}.web_port {} is also used by servers.{} - every server needs its own port", sw, sp.web_port, other));
                    }
                }
                if !sp.public_url.trim().is_empty() {
                    if let Err(e) = fix_public_url(&sp.public_url) {
                        p.push(format!("{}.public_url: {}", sw, e));
                    }
                }
                // two servers writing to the same place would overwrite each other's video
                for (field, mine) in [("publish_to", sp.publish_to.trim()), ("rtmp_url", sp.rtmp_url.trim())] {
                    if mine.is_empty() || (field == "rtmp_url" && sp.output == "hls") { continue }
                    let clash = self.servers.iter().find(|(ok, o)| *ok != k && o.spectator.as_ref().is_some_and(|x| {
                        let theirs = if field == "publish_to" { x.publish_to.trim() } else if x.output == "hls" { "" } else { x.rtmp_url.trim() };
                        theirs == mine
                    }));
                    if let Some((other, _)) = clash {
                        if k.as_str() < other.as_str() {
                            p.push(format!("{}.{} is the same as servers.{}'s - the two videos would overwrite each other; give each server its own", sw, field, other));
                        }
                    }
                }
                // with real sound, two clients on one PulseAudio source would hear each other
                if sp.audio == "pulse" {
                    if let Some((other, _)) = self.servers.iter().find(|(ok, o)| *ok != k && o.spectator.as_ref().is_some_and(|x| x.audio == "pulse" && x.pulse_source == sp.pulse_source)) {
                        if k.as_str() < other.as_str() {
                            p.push(format!("{}.pulse_source '{}' is also used by servers.{} - each video with sound needs its own PulseAudio sink, or the streams mix their sound (use audio \"silent\" to avoid it)", sw, sp.pulse_source, other));
                        }
                    }
                }
            }
        }
        for (k, f) in &self.filesystems {
            if f.kind != "sftp" { p.push(format!("filesystems.{}: unsupported type '{}'", k, f.kind)); }
            if f.host.is_empty() || f.user.is_empty() { p.push(format!("filesystems.{}: host and user are required", k)); }
            if !valid_key(k) { p.push(format!("filesystems.{}: the name may only contain letters, digits, '-' and '_'", k)); }
        }
        p
    }

    pub fn discord_token(&self) -> String {
        std::env::var("DISCORD_TOKEN").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| self.discord.token.clone())
    }

    pub fn web_token(&self) -> String {
        std::env::var("GTV_WEB_TOKEN").ok().filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.web.as_ref().map(|w| w.token.clone()).unwrap_or_default())
    }

    /// Write the configuration atomically (keeps one `.bak`), private to the owner on Unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)? + "\n";
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            let _ = std::fs::copy(path, path.with_extension("json.bak"));
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        set_private(&tmp);
        keep_owner(path, &tmp);
        std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }
}

/// A command run with sudo must not turn the config into a root-owned file the service user can no longer read:
/// give the new file the owner and group of the file it replaces.
#[cfg(unix)]
fn keep_owner(old: &Path, new: &Path) {
    use std::os::unix::fs::MetadataExt;
    if let Ok(m) = std::fs::metadata(old) {
        let _ = std::os::unix::fs::chown(new, Some(m.uid()), Some(m.gid()));
    }
}
#[cfg(not(unix))]
fn keep_owner(_old: &Path, _new: &Path) {}

#[cfg(unix)]
pub fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
pub fn set_private(_path: &Path) {}

/// Make a typed public address usable as the base of the Discord "Watch" link, or say what is wrong.
/// Fixes the usual typing slips: `https//host` (missing colon), `host/:8080` (port after a slash), no scheme at all,
/// stray spaces and trailing slashes. The result has the form `scheme://host[:port][/path]`.
pub fn fix_public_url(raw: &str) -> std::result::Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(String::new());
    }
    let mut rest = t.to_string();
    let mut scheme = "http".to_string();
    let low = t.to_lowercase();
    for sch in ["https", "http"] {
        for sep in ["://", ":/", "//", ":"] {
            let prefix = format!("{}{}", sch, sep);
            // "https:8080" must not be mistaken for a scheme typo: only accept ":" when no digit follows
            if low.starts_with(&prefix) && !(sep == ":" && low[prefix.len()..].chars().next().is_some_and(|c| c.is_ascii_digit())) {
                scheme = sch.to_string();
                rest = t[prefix.len()..].trim_start_matches('/').to_string();
                break;
            }
        }
        if rest != t {
            break;
        }
    }
    if rest.contains("://") {
        return Err(format!("'{}' has a second '://' - it should look like http://your-address:8080", t));
    }
    let (auth, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.clone(), String::new()),
    };
    // host/:8080  ->  host:8080
    let (auth, path) = match path.strip_prefix("/:") {
        Some(after) if !after.is_empty() && after.chars().next().is_some_and(|c| c.is_ascii_digit()) && !auth.contains(':') => {
            let (port, tail) = match after.find('/') { Some(i) => (&after[..i], &after[i..]), None => (after, "") };
            (format!("{}:{}", auth, port), tail.to_string())
        }
        _ => (auth, path),
    };
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.to_string(), Some(p.to_string())),
        _ => (auth.clone(), None),
    };
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':')) {
        return Err(format!("'{}' is not a valid address - use something like http://your-address:8080", t));
    }
    if let Some(p) = &port {
        if p.parse::<u16>().ok().filter(|n| *n > 0).is_none() {
            return Err(format!("'{}': the port '{}' is not a number between 1 and 65535", t, p));
        }
    }
    Ok(format!("{}://{}{}", scheme, auth, path.trim_end_matches('/')))
}

/// `base` with its port replaced (and any path dropped): `http://host:8080/x` + 8081 -> `http://host:8081`.
/// Anything that is not `scheme://host[:port]` comes back unchanged.
pub fn with_port(base: &str, port: u16) -> String {
    let Some((scheme, rest)) = base.split_once("://") else { return base.to_string() };
    let auth = &rest[..rest.find('/').unwrap_or(rest.len())];
    let host = if auth.starts_with('[') {
        &auth[..auth.find(']').map(|i| i + 1).unwrap_or(auth.len())]
    } else {
        auth.split(':').next().unwrap_or(auth)
    };
    if host.is_empty() { return base.to_string() }
    format!("{}://{}:{}", scheme, host, port)
}

/// The address of one server's web player: its own `public_url` if it has one, else the shared one with the server's
/// own port, else the shared one. Empty when nothing is known.
pub fn stream_base(shared_public: &str, sp: &SpectatorConfig) -> String {
    let own = fix_public_url(&sp.public_url).unwrap_or_default();
    if !own.is_empty() {
        own
    } else if sp.web_port != 0 && !shared_public.trim().is_empty() {
        with_port(shared_public.trim(), sp.web_port)
    } else {
        shared_public.trim().to_string()
    }
}

/// `name` of a `name:/path` spec when it is syntactically one (a drive letter like `C:` is not).
pub fn fs_name_of(spec: &str) -> Option<String> {
    let (name, rest) = spec.split_once(':')?;
    let ok = name.len() >= 2
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok && !rest.starts_with('\\') { Some(name.to_string()) } else { None }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    if p == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    PathBuf::from(p)
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

// ---- admin password hashing ----

const ITERATIONS: u32 = 60_000;

fn derive(salt: &[u8], password: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password.as_bytes());
    let mut d = h.finalize().to_vec();
    for _ in 0..ITERATIONS {
        let mut h = Sha256::new();
        h.update(&d);
        h.update(salt);
        d = h.finalize().to_vec();
    }
    d
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn hash_password(password: &str) -> String {
    let salt: [u8; 16] = rand::random();
    format!("sha256i${}${}", hex::encode(salt), hex::encode(derive(&salt, password)))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut it = stored.split('$');
    let (Some("sha256i"), Some(salt), Some(hash), None) = (it.next(), it.next(), it.next(), it.next()) else { return false };
    let (Ok(salt), Ok(hash)) = (hex::decode(salt), hex::decode(hash)) else { return false };
    ct_eq(&derive(&salt, password), &hash)
}

pub fn random_token(bytes: usize) -> String {
    let mut v = vec![0u8; bytes];
    for b in v.iter_mut() {
        *b = rand::random();
    }
    hex::encode(v)
}

pub fn must_dir(p: &Path) -> Result<()> {
    if p.exists() && !p.is_dir() {
        bail!("{} exists and is not a folder", p.display());
    }
    std::fs::create_dir_all(p).with_context(|| format!("cannot create {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
    # comment
    {
      "discord": {"token": "a.b.c", "restrict_to_user_ids": [111, "222"]},
      "web": {"port": 9090, "public_url": "http://x", "token": "t"},   // trailing
      "filesystems": {"gs": {"type": "sftp", "host": "h", "user": "u", "key_file": "~/.ssh/id"}},
      "servers": {
        "main": {"host": "1.2.3.4", "log_path": "gs:/a/games.log", "poll_interval": 0.1,
                 "spectator": {"client_binary": "/x/Quake3-UrT", "base_path": "/x"}},
        "second": {"host": "5.6.7.8", "port": 27961, "name": "Two"}
      }
    }"#;

    #[test]
    fn loads_the_python_style_config() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.discord.restrict_to_user_ids, vec!["111", "222"]);
        assert_eq!(c.web.as_ref().unwrap().port, 9090);
        assert_eq!(c.servers.len(), 2);
        assert_eq!(c.servers.keys().collect::<Vec<_>>(), vec!["main", "second"]); // order preserved
        assert_eq!(c.servers["main"].key, "main");
        assert_eq!(c.servers["main"].poll_interval, 1.0); // clamped
        assert_eq!(c.servers["second"].display_name(), "Two");
        assert_eq!(c.servers["main"].spectator.as_ref().unwrap().width, 1280);
        assert!(c.problems().is_empty(), "{:?}", c.problems());
    }

    #[test]
    fn unknown_options_are_rejected_with_their_name() {
        let e = format!("{:#}", Config::parse(r#"{"servers": {"a": {"host": "h", "prot": 1}}}"#).unwrap_err());
        assert!(e.contains("prot"), "{}", e);
        assert!(Config::parse(r#"{"nope": 1}"#).is_err());
    }

    #[test]
    fn comment_stripping_leaves_strings_alone() {
        let t = strip_comments("{\"u\": \"http://a#b\"} # c\n");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&t).unwrap()["u"], "http://a#b");
    }

    #[cfg(unix)]
    #[test]
    fn saving_keeps_the_files_owner() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("gtv-own-{}", random_token(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut c = Config::default();
        c.save(&path).unwrap();
        // only root may hand a file to someone else; elsewhere the test just checks nothing changes
        let me = std::fs::metadata(&path).unwrap();
        let (uid, gid) = if me.uid() == 0 { (23456, 23456) } else { (me.uid(), me.gid()) };
        let _ = std::os::unix::fs::chown(&path, Some(uid), Some(gid));
        c.data_dir = "./changed".into();
        c.save(&path).unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!((after.uid(), after.gid()), (uid, gid));
        assert_eq!(after.mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn encoder_setting_is_validated() {
        let mut c = Config::parse(EXAMPLE).unwrap();
        let key = c.servers.keys().next().unwrap().clone();
        let mut sp = SpectatorConfig::default();
        sp.client_binary = "/x".into();
        sp.base_path = "/y".into();
        c.web = Some(WebConfig::default());
        c.servers[&key].spectator = Some(sp);
        assert!(!c.problems().iter().any(|p| p.contains("encoder")), "{:?}", c.problems());
        for ok in ["cpu", "auto", "gpu", "nvenc", "qsv"] {
            c.servers[&key].spectator.as_mut().unwrap().encoder = ok.into();
            assert!(!c.problems().iter().any(|p| p.contains("encoder")), "{}", ok);
        }
        c.servers[&key].spectator.as_mut().unwrap().encoder = "turbo".into();
        assert!(c.problems().iter().any(|p| p.contains("encoder must be")));
        c.servers[&key].spectator.as_mut().unwrap().encoder = "amf".into(); // Windows only
        assert!(c.problems().iter().any(|p| p.contains("not available on this operating system")) == cfg!(unix));
    }

    #[test]
    fn rcon_commands_need_the_password_and_an_admin_role_or_person() {
        let mut c = Config::parse(r#"{"discord":{"rcon_roles":[111,"222"]},"servers":{"x":{"host":"h","rcon_password":"pw","rcon_commands":true}}}"#).unwrap();
        assert_eq!(c.discord.rcon_roles, vec!["111", "222"]);
        assert_eq!(c.discord.rcon_role_ids(), vec![111, 222]);
        assert!(c.servers["x"].rcon_enabled());
        assert!(!c.problems().iter().any(|m| m.contains("rcon")), "{:?}", c.problems());
        // switched on without the password: off, and the check says why
        c.servers.get_mut("x").unwrap().rcon_password = "  ".into();
        assert!(!c.servers["x"].rcon_enabled());
        assert!(c.problems().iter().any(|m| m.contains("rcon_commands needs rcon_password")));
        // the password is there but no role: the command would exist for nobody
        c.servers.get_mut("x").unwrap().rcon_password = "pw".into();
        c.discord.rcon_roles.clear();
        assert!(c.problems().iter().any(|m| m.contains("no admin role and no person is chosen")));
        // a role that is not a number is named
        c.discord.rcon_roles = vec!["Admins".into()];
        assert!(c.problems().iter().any(|m| m.contains("'Admins' is not a role id")));
        // a chosen person is enough on its own (no role); a user id that is not a number is named
        c.discord.rcon_roles.clear();
        c.discord.rcon_users = vec!["123456789012345678".into()];
        assert_eq!(c.discord.rcon_user_ids(), vec![123456789012345678u64]);
        assert!(!c.problems().iter().any(|m| m.contains("no admin role")), "{:?}", c.problems());
        c.discord.rcon_users = vec!["Bob".into(), "0".into()];
        assert!(c.problems().iter().any(|m| m.contains("'Bob' is not a user id")));
        assert!(c.discord.rcon_user_ids().is_empty());
        c.discord.rcon_users = vec!["5".into()];
        let parsed = Config::parse(r#"{"discord":{"rcon_users":[111,"222"]}}"#).unwrap();
        assert_eq!(parsed.discord.rcon_users, vec!["111", "222"]);
        // default: off, and old configs without the fields load unchanged
        let old = Config::parse(r#"{"servers":{"x":{"host":"h"}}}"#).unwrap();
        assert!(!old.servers["x"].rcon_commands && old.discord.rcon_roles.is_empty() && old.discord.rcon_users.is_empty());
        let again = Config::parse(&serde_json::to_string(&c).unwrap()).unwrap();
        assert!(again.servers["x"].rcon_commands);
    }

    #[test]
    fn problems_are_listed_in_plain_words() {
        let mut c = Config::parse(EXAMPLE).unwrap();
        c.filesystems.clear();
        c.web = None;
        c.servers["second"].relay_chat = true;
        let p = c.problems().join("\n");
        assert!(p.contains("filesystem 'gs'"));
        assert!(p.contains("relay_chat needs rcon_password"));
        assert!(p.contains("no \"web\" section"));
        let mut c = Config::default();
        c.servers.insert("bad key!".into(), ServerConfig::default());
        let p = c.problems().join("\n");
        assert!(p.contains("may only contain") && p.contains("host is empty"));
    }

    #[test]
    fn public_url_typos_are_repaired() {
        let fix = |s: &str| fix_public_url(s).unwrap();
        assert_eq!(fix("http://10.0.0.5:8080"), "http://10.0.0.5:8080");
        assert_eq!(fix("https//theforgeservers.com/:8080"), "https://theforgeservers.com:8080");
        assert_eq!(fix("https//theforgeservers.com/"), "https://theforgeservers.com");
        assert_eq!(fix("  theforgeservers.com:8080/  "), "http://theforgeservers.com:8080");
        assert_eq!(fix("HTTPS://Example.com/player/"), "https://Example.com/player");
        assert_eq!(fix("http:/host:8080"), "http://host:8080");
        assert_eq!(fix("host/:8080/x"), "http://host:8080/x");
        assert_eq!(fix(""), "");
        assert!(fix_public_url("http://bad host").is_err());
        assert!(fix_public_url("http://host:99999").is_err());
        assert!(fix_public_url("http://host:abc").is_err());
        assert!(fix_public_url("http://http://host").is_err());
        let mut c = Config::parse(EXAMPLE).unwrap();
        c.web.as_mut().unwrap().public_url = "https//theforgeservers.com/:8080".into();
        assert!(c.problems().join("\n").contains("https://theforgeservers.com:8080"));
    }

    #[test]
    fn fs_spec_detection() {
        assert_eq!(fs_name_of("gs:/a/b").as_deref(), Some("gs"));
        assert_eq!(fs_name_of("C:\\games\\games.log"), None);
        assert_eq!(fs_name_of("/plain/path"), None);
        assert_eq!(fs_name_of("a:/x"), None); // single letters are drive letters
    }

    #[test]
    fn save_and_reload_roundtrip_keeps_a_backup_and_is_private() {
        let dir = std::env::temp_dir().join(format!("gtv-cfg-{}", random_token(4)));
        let path = dir.join("config.json");
        let c = Config::parse(EXAMPLE).unwrap();
        c.save(&path).unwrap();
        c.save(&path).unwrap();
        assert!(dir.join("config.json.bak").exists());
        let back = Config::load(&path).unwrap();
        assert_eq!(back.servers.len(), 2);
        assert_eq!(back.servers["main"].log_path, "gs:/a/games.log");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn password_hashing() {
        let h = hash_password("correct horse");
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("x", "garbage"));
        assert_ne!(h, hash_password("correct horse")); // salted
    }

    #[test]
    fn address_lists_match_single_addresses_and_ranges() {
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        let l = list(&["203.0.113.7", "10.0.0.0/8", "192.168.1.128/25", "2001:db8::/32", "0.0.0.0/0"]);
        assert!(ip_allowed(&l, ip("203.0.113.7")) && ip_allowed(&l, ip("10.200.1.1")) && ip_allowed(&l, ip("2001:db8:1::5")));
        assert!(ip_allowed(&list(&["192.168.1.128/25"]), ip("192.168.1.200")) && !ip_allowed(&list(&["192.168.1.128/25"]), ip("192.168.1.100")));
        assert!(!ip_allowed(&list(&["203.0.113.7"]), ip("203.0.113.8")));
        assert!(!ip_allowed(&list(&["10.0.0.0/8"]), ip("::1")), "families do not mix");
        assert!(ip_allowed(&list(&["10.0.0.0/8"]), ip("::ffff:10.1.1.1")), "IPv4 behind a dual-stack socket");
        assert!(ip_allowed(&list(&["0.0.0.0/0"]), ip("8.8.8.8")));
        assert!(!ip_allowed(&[], ip("10.0.0.1")));
        for bad in ["", "abc", "10.0.0.0/33", "10.0.0.0/-1", "::1/129", "10.0.0.256", "10.0.0.0/8/8"] {
            assert!(parse_cidr(bad).is_none(), "{}", bad);
        }
    }

    #[test]
    fn local_or_remote_follows_the_listen_address() {
        for h in ["127.0.0.1", "localhost", "::1", "[::1]", "127.1.2.3", " LOCALHOST "] {
            assert!(is_loopback_host(h), "{}", h);
        }
        for h in ["0.0.0.0", "::", "192.168.1.5", "example.com", ""] {
            assert!(!is_loopback_host(h), "{}", h);
        }
        let mut a = AdminConfig::default();
        assert!(!a.remote());
        a.host = REMOTE_HOST.into();
        assert!(a.remote());
    }

    #[test]
    fn branding_and_access_settings_are_checked() {
        let mut c = Config::default();
        c.branding = BrandingConfig { name: "The Forge".into(), logo: "0123456789abcdef.png".into(), icon: "https://x.example/i.png".into(), color: "#E67E22".into() };
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        assert_eq!(parse_color("#E67E22"), Some(0xE67E22));
        assert_eq!(parse_color("e67e22"), Some(0xE67E22));
        assert_eq!(parse_color("#E67E2"), None);
        assert_eq!(parse_color("#GGGGGG"), None);
        let mut bad = c.clone();
        bad.branding = BrandingConfig { name: "x".repeat(41), logo: "../a.png".into(), icon: "file:///x".into(), color: "red".into() };
        bad.admin.allowed_ips = vec!["nope".into()];
        let p = bad.problems().join("\n");
        for needle in ["branding.name", "branding.color", "branding.logo", "branding.icon", "admin.allowed_ips"] {
            assert!(p.contains(needle), "{} in {}", needle, p);
        }
        // open to the network without a password is a problem; with one it is fine
        let mut r = Config::default();
        r.admin.host = REMOTE_HOST.into();
        assert!(r.problems().join("\n").contains("no admin password"));
        r.admin.password_hash = hash_password("long enough password");
        assert!(r.problems().is_empty());
    }

    #[test]
    fn older_configs_load_with_the_new_options_at_their_defaults() {
        let c = Config::parse(r#"{"discord": {"token": "a.b.c"}, "admin": {"port": 8787}, "servers": {"one": {"host": "1.2.3.4"}}}"#).unwrap();
        assert!(c.discord.new_post_each_map && !c.discord.delete_old_posts);
        assert_eq!(c.branding, BrandingConfig::default());
        assert!(!c.admin.remote() && c.admin.allowed_ips.is_empty());
        // and the options survive a save / load
        let mut d = c.clone();
        d.discord.new_post_each_map = false;
        d.branding.name = "Clan".into();
        let again = Config::parse(&serde_json::to_string(&d).unwrap()).unwrap();
        assert!(!again.discord.new_post_each_map && again.branding.name == "Clan");
    }

    // ---- one player port per server ----

    fn two_servers(pa: u16, pb: u16) -> Config {
        let mut c = Config::default();
        c.web = Some(WebConfig { public_url: "http://h:8080".into(), ..Default::default() }); // port 8080; the admin panel is on 8787
        for (k, p) in [("a", pa), ("b", pb)] {
            let sp = SpectatorConfig { client_binary: "/x".into(), base_path: "/y".into(), web_port: p, ..Default::default() };
            c.servers.insert(k.into(), ServerConfig { key: k.into(), host: "h".into(), spectator: Some(sp), ..Default::default() });
        }
        c
    }

    fn sp_mut<'a>(c: &'a mut Config, k: &str) -> &'a mut SpectatorConfig {
        c.servers.get_mut(k).unwrap().spectator.as_mut().unwrap()
    }

    #[test]
    fn a_port_is_swapped_into_the_public_address() {
        assert_eq!(with_port("http://h:8080/x", 8081), "http://h:8081");
        assert_eq!(with_port("https://example.org", 8081), "https://example.org:8081");
        assert_eq!(with_port("http://[::1]:8080", 9), "http://[::1]:9");
        assert_eq!(with_port("not an address", 5), "not an address");
    }

    #[test]
    fn each_server_works_out_its_own_player_address() {
        let sp = |port: u16, url: &str| SpectatorConfig { web_port: port, public_url: url.into(), ..Default::default() };
        let shared = "http://h:8080";
        assert_eq!(stream_base(shared, &sp(0, "")), "http://h:8080", "no own port: the shared player, as before");
        assert_eq!(stream_base(shared, &sp(8081, "")), "http://h:8081");
        assert_eq!(stream_base(shared, &sp(8081, "https://gtv2.example.com/")), "https://gtv2.example.com", "an own address wins");
        assert_eq!(stream_base("", &sp(8081, "")), "", "nothing to build on");
    }

    #[test]
    fn distinct_player_ports_are_fine_and_clashes_are_named() {
        assert!(two_servers(8081, 8082).problems_on(crate::spectator::Platform::Linux).is_empty(), "{:?}", two_servers(8081, 8082).problems_on(crate::spectator::Platform::Linux));
        assert!(two_servers(0, 0).problems_on(crate::spectator::Platform::Linux).is_empty(), "sharing the main port stays valid");

        let same = two_servers(8081, 8081).problems_on(crate::spectator::Platform::Linux);
        assert_eq!(same.iter().filter(|m| m.contains("web_port 8081") && m.contains("also used")).count(), 1, "{:?}", same);

        assert!(two_servers(8080, 8082).problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("main web player's port")));
        assert!(two_servers(8787, 8082).problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("admin panel's port")));

        let mut no_web = two_servers(8081, 8082);
        no_web.web = None;
        assert!(no_web.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("web_port") && m.contains("web")));

        let mut rtmp = two_servers(8081, 8082);
        { let a = sp_mut(&mut rtmp, "a"); a.output = "rtmp".into(); a.rtmp_url = "rtmp://x/live/1".into(); }
        assert!(rtmp.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("no effect")));

        let mut bad = two_servers(8081, 8082);
        sp_mut(&mut bad, "a").public_url = "http://a b::".into();
        assert!(bad.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("servers.a.spectator.public_url")));
    }

    #[test]
    fn servers_must_not_share_an_output_or_a_sound_source() {
        let mut c = two_servers(8081, 8082);
        sp_mut(&mut c, "a").publish_to = "/srv/www/live".into();
        sp_mut(&mut c, "b").publish_to = "/srv/www/live".into();
        let p = c.problems_on(crate::spectator::Platform::Linux);
        assert_eq!(p.iter().filter(|m| m.contains("publish_to")).count(), 1, "reported once, not per server: {:?}", p);

        let mut c = two_servers(8081, 8082);
        for k in ["a", "b"] { let s = sp_mut(&mut c, k); s.output = "both".into(); s.rtmp_url = "rtmp://x/live/key".into(); }
        assert!(c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("rtmp_url") && m.contains("overwrite")));
        for k in ["a", "b"] { sp_mut(&mut c, k).rtmp_url = format!("rtmp://x/live/{}", k); }
        assert!(c.problems_on(crate::spectator::Platform::Linux).is_empty());

        let mut c = two_servers(8081, 8082);
        for k in ["a", "b"] { sp_mut(&mut c, k).audio = "pulse".into(); }
        assert!(c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("pulse_source") && m.contains("mix")));
        sp_mut(&mut c, "b").pulse_source = "gtv_b.monitor".into();
        assert!(c.problems_on(crate::spectator::Platform::Linux).is_empty());
    }

    #[test]
    fn the_new_settings_round_trip_through_the_config_file() {
        let c = two_servers(8081, 0);
        let again = Config::parse(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(again.servers["a"].spectator.as_ref().unwrap().web_port, 8081);
        assert_eq!(again.servers["b"].spectator.as_ref().unwrap().web_port, 0);
        // an older config without the settings still loads, sharing the main port
        let old = Config::parse(r#"{"servers":{"x":{"host":"h","spectator":{"client_binary":"c","base_path":"b"}}}}"#).unwrap();
        assert_eq!(old.servers["x"].spectator.as_ref().unwrap().web_port, 0);
    }

    #[test]
    fn windows_warns_that_several_videos_share_one_desktop() {
        use crate::spectator::Platform;
        let c = two_servers(8081, 8082);
        let on_windows = c.problems_on(Platform::Windows);
        assert_eq!(on_windows.iter().filter(|m| m.contains("same desktop")).count(), 1, "{:?}", on_windows);
        assert!(!c.problems_on(Platform::Linux).iter().any(|m| m.contains("same desktop")));
        // one video on Windows is fine
        let mut one = two_servers(0, 0);
        one.servers.get_mut("b").unwrap().spectator = None;
        assert!(!one.problems_on(Platform::Windows).iter().any(|m| m.contains("same desktop")));
    }

    // ---- map pictures ----

    #[test]
    fn map_pictures_default_to_a_320_square_and_older_configs_still_load() {
        let c = Config::default();
        assert_eq!((c.levelshots.enabled, c.levelshots.size, c.levelshots.fit.as_str()), (true, 320, "crop"));
        let old = Config::parse(r#"{"servers":{"x":{"host":"h"}}}"#).unwrap();
        assert_eq!(old.levelshots, LevelshotConfig::default());
        assert_eq!(old.servers["x"].levelshot_dir, "");
        let set = Config::parse(r#"{"levelshots":{"size":256,"fit":"pad"},"servers":{"x":{"host":"h","levelshot_dir":"/maps"}}}"#).unwrap();
        assert_eq!((set.levelshots.size, set.levelshots.fit.as_str(), set.levelshots.enabled), (256, "pad", true));
        let again = Config::parse(&serde_json::to_string(&set).unwrap()).unwrap();
        assert_eq!(again.servers["x"].levelshot_dir, "/maps");
    }

    #[test]
    fn map_picture_folders_come_from_the_server_and_the_video_game_folder() {
        let mut s = ServerConfig::default();
        assert!(s.levelshot_sources().is_empty());
        s.levelshot_dir = " /maps ".into();
        assert_eq!(s.levelshot_sources(), vec![PathBuf::from("/maps")]);
        s.spectator = Some(SpectatorConfig { base_path: "/games/urt".into(), game_dir: "q3ut4".into(), ..Default::default() });
        assert_eq!(s.levelshot_sources(), vec![PathBuf::from("/maps"), PathBuf::from("/games/urt/q3ut4")]);
        s.levelshot_dir.clear();
        assert_eq!(s.levelshot_sources(), vec![PathBuf::from("/games/urt/q3ut4")]);
    }

    #[test]
    fn map_picture_settings_are_checked_and_the_public_address_is_asked_for_only_when_it_matters() {
        let mut c = two_servers(8081, 8082);
        c.web.as_mut().unwrap().public_url = "http://h:8080".into();
        assert!(c.problems_on(crate::spectator::Platform::Linux).is_empty(), "{:?}", c.problems_on(crate::spectator::Platform::Linux));
        c.levelshots.size = 50;
        assert!(c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("levelshots.size 50")));
        c.levelshots.size = 700;
        assert!(c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("levelshots.size 700")));
        c.levelshots.size = 320;
        c.levelshots.fit = "stretch".into();
        assert!(c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("levelshots.fit")));
        c.levelshots.fit = "pad".into();
        assert!(c.problems_on(crate::spectator::Platform::Linux).is_empty());
        // pictures could be found (the servers have a game folder) but Discord could not be given an address
        c.web.as_mut().unwrap().public_url.clear();
        assert!(c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("levelshots") && m.contains("public_url")));
        c.levelshots.enabled = false;
        assert!(!c.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("levelshots")));
        // a scoreboard-only setup with no folders is not nagged about pictures it can never find
        let mut plain = Config::default();
        plain.servers.insert("a".into(), ServerConfig { key: "a".into(), host: "h".into(), ..Default::default() });
        assert!(!plain.problems_on(crate::spectator::Platform::Linux).iter().any(|m| m.contains("levelshots")), "{:?}", plain.problems_on(crate::spectator::Platform::Linux));
    }
}
