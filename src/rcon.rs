// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Admin commands sent to a game server's rcon from Discord. Exactly five exist: `cyclemap`, `slap`, `kick`, `bigtext`
//! and `ban` (with a reason). Nothing a person types is sent as a command of its own:
//!
//! * a **player** is looked up in the server's own `status` list, and only its client number (digits) reaches the wire;
//! * a **reason / message** is reduced to plain printable characters without `; " \ % $ ^ ~` or a backtick, and is
//!   always placed inside quotes the person cannot close;
//! * the command words are fixed here.
//!
//! Urban Terror has no "ban with a reason" command: it bans by address (`addip`). A ban is therefore the address ban
//! plus a kick that shows the reason, and it is refused when the address cannot be banned safely (a bot, no address,
//! the server's own address, or an address shared by several players).

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;

use crate::config::ServerConfig;
use crate::urt;

pub const MAX_REASON: usize = 80;
pub const MAX_BIGTEXT: usize = 80;

#[derive(Clone, Debug, PartialEq)]
pub struct RconPlayer {
    pub slot: u32,
    /// Name without colour codes.
    pub name: String,
    /// IPv4 address (without the port), if the server shows one.
    pub ip: Option<String>,
    pub bot: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub map: Option<String>,
    pub players: Vec<RconPlayer>,
}

/// What an admin asked for. Player fields hold what the person typed (a number or part of a name); they are never sent.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Cyclemap,
    Slap { player: String },
    Kick { player: String, reason: String },
    Bigtext { text: String },
    Ban { player: String, reason: String },
}

impl Action {
    pub fn verb(&self) -> &'static str {
        match self {
            Action::Cyclemap => "cyclemap",
            Action::Slap { .. } => "slap",
            Action::Kick { .. } => "kick",
            Action::Bigtext { .. } => "bigtext",
            Action::Ban { .. } => "ban",
        }
    }
}

/// What will be sent, and what to tell the admin.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub commands: Vec<String>,
    pub summary: String,
    /// What the audit log may say (never contains an address).
    pub audit: String,
}

/// Plain text only: printable ASCII, none of `; " \ % $ ^ ~` or a backtick, whitespace squeezed, at most `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    let kept: String = s.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).filter(|c| (' '..='~').contains(c) && !";\"\\%$^~`".contains(*c)).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect::<String>().trim().to_string()
}

fn status_line_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // num score ping name lastmsg address qport rate  (the name may hold spaces; the last four columns pin it down)
    R.get_or_init(|| Regex::new(r"^\s*(\d+)\s+(?:-?\d+|ZMBI|CNCT)\s+(?:\d+|ZMBI|CNCT)\s+(.*?)\s+\d+\s+(\S+)\s+\d+\s+\d+\s*$").expect("regex"))
}

fn ipv4_of(addr: &str) -> Option<String> {
    let host = addr.split(':').next().unwrap_or("");
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n <= 255)) {
        Some(host.to_string())
    } else {
        None
    }
}

/// Read the answer to `rcon status`.
pub fn parse_status(text: &str) -> Status {
    let mut st = Status::default();
    for line in text.lines() {
        let l = line.trim();
        if let Some(m) = l.strip_prefix("map:") {
            st.map = Some(m.trim().to_string()).filter(|m| !m.is_empty());
            continue;
        }
        if let Some(c) = status_line_re().captures(line) {
            let Ok(slot) = c[1].parse::<u32>() else { continue };
            let addr = &c[3];
            st.players.push(RconPlayer { slot, name: urt::strip_colors(&c[2]).trim().to_string(), ip: ipv4_of(addr), bot: addr.eq_ignore_ascii_case("bot") });
        }
    }
    st
}

fn shown(p: &RconPlayer) -> String {
    format!("{} {}", p.slot, p.name.replace('`', "'"))
}

/// Find one player from what the admin typed: a client number, an exact name, or a part of a name that fits one player.
pub fn resolve_player<'a>(players: &'a [RconPlayer], query: &str) -> Result<&'a RconPlayer, String> {
    let q: String = query.trim().trim_start_matches('#').chars().take(60).collect();
    if q.is_empty() {
        return Err("Say which player (a name or the client number).".into());
    }
    if q.chars().all(|c| c.is_ascii_digit()) {
        if let Some(p) = q.parse::<u32>().ok().and_then(|n| players.iter().find(|p| p.slot == n)) {
            return Ok(p);
        }
    }
    let want = urt::strip_colors(&q).to_lowercase();
    let list = |v: &[&RconPlayer]| v.iter().take(6).map(|p| format!("`{}`", shown(p))).collect::<Vec<_>>().join(", ");
    let exact: Vec<&RconPlayer> = players.iter().filter(|p| p.name.to_lowercase() == want).collect();
    match exact.len() {
        1 => return Ok(exact[0]),
        n if n > 1 => return Err(format!("More than one player is called `{}`: {}. Use the client number.", q.replace('`', "'"), list(&exact))),
        _ => {}
    }
    let part: Vec<&RconPlayer> = players.iter().filter(|p| !want.is_empty() && p.name.to_lowercase().contains(&want)).collect();
    match part.len() {
        0 => Err(format!("Nobody on the server matches `{}` right now.", q.replace('`', "'"))),
        1 => Ok(part[0]),
        _ => Err(format!("`{}` fits several players: {}. Type more of the name, or the client number.", q.replace('`', "'"), list(&part))),
    }
}

/// Turn a request plus the server's current player list into the exact commands to send.
pub fn plan(action: &Action, st: &Status) -> Result<Plan, String> {
    match action {
        Action::Cyclemap => {
            let was = st.map.as_deref().map(|m| format!(" (it was `{}`)", m.replace('`', "'"))).unwrap_or_default();
            Ok(Plan { commands: vec!["cyclemap".into()], summary: format!("Moving on to the next map{}.", was), audit: "cyclemap".into() })
        }
        Action::Bigtext { text } => {
            let t = clean(text, MAX_BIGTEXT);
            if t.is_empty() {
                return Err("The message is empty once unusual characters are removed (plain letters, digits and punctuation only).".into());
            }
            Ok(Plan { commands: vec![format!("bigtext \"{}\"", t)], summary: format!("Showed on every screen: **{}**", t), audit: format!("bigtext \"{}\"", t) })
        }
        Action::Slap { player } => {
            let p = resolve_player(&st.players, player)?;
            Ok(Plan { commands: vec![format!("slap {}", p.slot)], summary: format!("Slapped `{}`.", shown(p)), audit: format!("slap slot {} ({})", p.slot, p.name) })
        }
        Action::Kick { player, reason } => {
            let p = resolve_player(&st.players, player)?;
            let r = clean(reason, MAX_REASON);
            let (cmd, why) = if r.is_empty() { (format!("kick {}", p.slot), String::new()) } else { (format!("kick {} \"{}\"", p.slot, r), format!(" Reason shown to them: *{}*", r)) };
            Ok(Plan { commands: vec![cmd], summary: format!("Kicked `{}`.{}", shown(p), why), audit: format!("kick slot {} ({}) reason \"{}\"", p.slot, p.name, r) })
        }
        Action::Ban { player, reason } => {
            let r = clean(reason, MAX_REASON);
            if r.chars().filter(|c| c.is_alphanumeric()).count() < 3 {
                return Err("A ban needs a reason (at least a few letters).".into());
            }
            let p = resolve_player(&st.players, player)?;
            if p.bot {
                return Err(format!("`{}` is a bot - it has no address to ban. Use kick.", shown(p)));
            }
            let Some(ip) = p.ip.clone() else {
                return Err(format!("The server shows no address for `{}`, so there is nothing to ban. Use kick.", shown(p)));
            };
            if ip.starts_with("127.") || ip == "0.0.0.0" {
                return Err(format!("`{}` connects from the server's own address ({}); banning it would shut out the server itself. Use kick.", shown(p), ip));
            }
            let sharing: Vec<&RconPlayer> = st.players.iter().filter(|o| !o.bot && o.ip.as_deref() == Some(ip.as_str())).collect();
            if sharing.len() > 1 {
                let names = sharing.iter().take(6).map(|o| format!("`{}`", shown(o))).collect::<Vec<_>>().join(", ");
                return Err(format!("Not banned: {} players connect from the same address ({}) - {}. Banning it would ban all of them. Use kick.", sharing.len(), ip, names));
            }
            Ok(Plan {
                commands: vec![format!("addip {}", ip), format!("kick {} \"Banned: {}\"", p.slot, r)],
                summary: format!("Banned `{}` (address {}) — reason: *{}*.\nA ban is by address, so anyone else on that address is blocked too. To undo it, run `removeip {}` in the server's rcon.", shown(p), ip, r, ip),
                audit: format!("ban slot {} ({}) reason \"{}\"", p.slot, p.name, r),
            })
        }
    }
}

fn refusal(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    if t.contains("bad rconpassword") {
        Some("The server refused the rcon password saved in the bot (\"Bad rconpassword\"). Check it in the admin panel.")
    } else if t.contains("no rconpassword set") {
        Some("The game server has no rcon password set (`rconpassword` in server.cfg), so it ignores rcon.")
    } else {
        None
    }
}

/// Ask the server who is on it, work out the commands, send them. Returns what to tell the admin, and an audit line.
pub async fn run(cfg: &ServerConfig, action: &Action) -> Result<(String, String), String> {
    if !cfg.rcon_enabled() {
        return Err("Rcon commands are not switched on for this server.".into());
    }
    let (host, port, pw) = (cfg.host.as_str(), cfg.port, cfg.rcon_password.as_str());
    let Some(text) = urt::rcon_collect(host, port, pw, "status").await else {
        return Err("The game server did not answer. Is it online, and are the address and port in the bot's settings right?".into());
    };
    if let Some(r) = refusal(&text) {
        return Err(r.into());
    }
    let st = parse_status(&text);
    let p = plan(action, &st)?;
    for (i, cmd) in p.commands.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // the status answer above already proved the password; the commands themselves print nothing worth waiting for
        if !urt::rcon_send(host, port, pw, cmd).await {
            return Err("Could not send the command to the game server.".into());
        }
    }
    Ok((p.summary, p.audit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    /// The real column layout of `rcon status` (the header and rows come from an Urban Terror 4.3 server).
    const STATUS: &str = "\nmap: ut4_turnpike\nnum score ping name            lastmsg address               qport rate\n--- ----- ---- --------------- ------- --------------------- ----- -----\n  0    18   60 Viper^7                 0 11.22.33.44:27961     38410 32000\n  1    14   53 Nova Star^7             0 11.22.33.45:27961     38411 32000\n  2     9   53 Rook^7                  0 11.22.33.46:27961     38412 32000\n  3     4   53 Echo^7                  0 11.22.33.46:27960     38413 25000\n  4     0    0 ^1B^2ot^7               0 bot                   0     16384\n  5  CNCT CNCT                          0 11.22.33.99:27961     38415 25000\n  6    11   40 Viper2^7                0 127.0.0.1:27961       38416 25000\n";

    fn st() -> Status {
        parse_status(STATUS)
    }

    #[test]
    fn the_status_list_is_read_with_numbers_names_and_addresses() {
        let s = st();
        assert_eq!(s.map.as_deref(), Some("ut4_turnpike"));
        assert_eq!(s.players.len(), 7);
        assert_eq!(s.players[0], RconPlayer { slot: 0, name: "Viper".into(), ip: Some("11.22.33.44".into()), bot: false });
        assert_eq!(s.players[1].name, "Nova Star"); // a space inside the name
        assert!(s.players[4].bot && s.players[4].ip.is_none() && s.players[4].name == "Bot"); // colours stripped, no address
        assert_eq!(s.players[5].slot, 5); // still connecting
        assert!(parse_status("Bad rconpassword.\n").players.is_empty());
    }

    #[test]
    fn a_player_is_found_by_number_exact_name_or_part_of_a_name() {
        let s = st();
        assert_eq!(resolve_player(&s.players, "2").unwrap().name, "Rook");
        assert_eq!(resolve_player(&s.players, "#2").unwrap().name, "Rook");
        assert_eq!(resolve_player(&s.players, "nova star").unwrap().slot, 1);
        assert_eq!(resolve_player(&s.players, "ROO").unwrap().slot, 2);
        // "Viper" is an exact name even though "Viper2" also contains it
        assert_eq!(resolve_player(&s.players, "viper").unwrap().slot, 0);
        // "vip" fits two players: refuse, and say which
        let e = resolve_player(&s.players, "vip").unwrap_err();
        assert!(e.contains("fits several") && e.contains("0 Viper") && e.contains("6 Viper2"), "{}", e);
        assert!(resolve_player(&s.players, "nobody").unwrap_err().contains("Nobody"));
        assert!(resolve_player(&s.players, "  ").is_err());
        // a number that is nobody's slot falls back to names, then fails
        assert!(resolve_player(&s.players, "42").is_err());
        let twins = vec![RconPlayer { slot: 1, name: "Dup".into(), ip: None, bot: false }, RconPlayer { slot: 2, name: "dup".into(), ip: None, bot: false }];
        assert!(resolve_player(&twins, "dup").unwrap_err().contains("More than one"));
    }

    #[test]
    fn typed_text_can_never_become_another_command() {
        assert_eq!(clean("hello; quit", 80), "hello quit");
        assert_eq!(clean("a\" ; rcon \"b", 80), "a rcon b");
        assert_eq!(clean("x\\y%s$z^1~`w", 80), "xysz1w");
        assert_eq!(clean("line1\nline2\r\n\ttab", 80), "line1 line2 tab");
        assert_eq!(clean("é日本語 ok", 80), "ok");
        assert_eq!(clean(&"a".repeat(500), 80).len(), 80);
        // the wire text of every command a hostile reason or message could produce
        let hostile = "x\"; map evil; rcon_password \"y";
        let p = plan(&Action::Kick { player: "2".into(), reason: hostile.into() }, &st()).unwrap();
        assert_eq!(p.commands, vec!["kick 2 \"x map evil rcon_password y\""]);
        let p = plan(&Action::Bigtext { text: hostile.into() }, &st()).unwrap();
        assert_eq!(p.commands, vec!["bigtext \"x map evil rcon_password y\""]);
        assert!(plan(&Action::Bigtext { text: "; \" \\".into() }, &st()).is_err());
        // a "player" that looks like a command is only ever matched against names, never sent
        let e = plan(&Action::Slap { player: "2; quit".into() }, &st()).unwrap_err();
        assert!(e.contains("Nobody"));
    }

    #[test]
    fn each_action_becomes_exactly_the_documented_commands() {
        let s = st();
        assert_eq!(plan(&Action::Cyclemap, &s).unwrap().commands, vec!["cyclemap"]);
        assert!(plan(&Action::Cyclemap, &s).unwrap().summary.contains("ut4_turnpike"));
        assert_eq!(plan(&Action::Slap { player: "Rook".into() }, &s).unwrap().commands, vec!["slap 2"]);
        assert_eq!(plan(&Action::Kick { player: "Rook".into(), reason: String::new() }, &s).unwrap().commands, vec!["kick 2"]);
        assert_eq!(plan(&Action::Kick { player: "Rook".into(), reason: "afk".into() }, &s).unwrap().commands, vec!["kick 2 \"afk\""]);
        assert_eq!(plan(&Action::Bigtext { text: "Map change soon".into() }, &s).unwrap().commands, vec!["bigtext \"Map change soon\""]);
        let b = plan(&Action::Ban { player: "Nova".into(), reason: "cheating".into() }, &s).unwrap();
        assert_eq!(b.commands, vec!["addip 11.22.33.45", "kick 1 \"Banned: cheating\""]);
        assert!(b.summary.contains("removeip 11.22.33.45"));
        assert!(!b.audit.contains("11.22.33.45"), "the audit line must not carry an address: {}", b.audit);
        assert_eq!(Action::Cyclemap.verb(), "cyclemap");
    }

    #[test]
    fn a_ban_is_refused_whenever_it_could_hit_the_wrong_people() {
        let s = st();
        let ban = |who: &str, why: &str| plan(&Action::Ban { player: who.into(), reason: why.into() }, &s);
        assert!(ban("Rook", "").unwrap_err().contains("needs a reason"));
        assert!(ban("Rook", "ab").unwrap_err().contains("needs a reason"));
        assert!(ban("Rook", "; ;;").unwrap_err().contains("needs a reason"));
        assert!(ban("Rook", "cheating").unwrap_err().contains("same address")); // Rook and Echo share 11.22.33.46
        assert!(ban("Bot", "x y z").unwrap_err().contains("bot"));
        assert!(ban("Viper2", "spamming").unwrap_err().contains("server's own address"));
        assert!(ban("5", "no name yet").is_ok()); // a connecting player has an address and can be banned
        let no_ip = Status { map: None, players: vec![RconPlayer { slot: 1, name: "Ghost".into(), ip: None, bot: false }] };
        assert!(plan(&Action::Ban { player: "Ghost".into(), reason: "cheating".into() }, &no_ip).unwrap_err().contains("no address"));
    }

    async fn fake_server(password: &'static str) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
                let text = String::from_utf8_lossy(&buf[4..n]).to_string();
                let Some(rest) = text.strip_prefix("rcon ") else { continue };
                let (pw, cmd) = rest.split_once(' ').unwrap_or((rest, ""));
                let out = |s: &str| [b"\xff\xff\xff\xffprint\n".as_slice(), s.as_bytes()].concat();
                if pw != password {
                    let _ = sock.send_to(&out("Bad rconpassword.\n"), from).await;
                    continue;
                }
                log.lock().unwrap().push(cmd.to_string());
                if cmd == "status" {
                    // a real server cuts long answers into several packets
                    let (a, b) = STATUS.split_at(STATUS.find("  3 ").unwrap());
                    let _ = sock.send_to(&out(a), from).await;
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let _ = sock.send_to(&out(b), from).await;
                }
            }
        });
        (port, seen)
    }

    fn server(port: u16, pw: &str, on: bool) -> ServerConfig {
        ServerConfig { host: "127.0.0.1".into(), port, rcon_password: pw.into(), rcon_commands: on, ..Default::default() }
    }

    #[tokio::test]
    async fn the_whole_path_reads_a_multi_packet_status_and_sends_only_the_planned_commands() {
        let (port, seen) = fake_server("sekrit").await;
        let cfg = server(port, "sekrit", true);
        let (summary, audit) = run(&cfg, &Action::Ban { player: "Nova".into(), reason: "cheating".into() }).await.unwrap();
        assert!(summary.contains("Nova Star") && audit.starts_with("ban slot 1"));
        // "Echo" (slot 3) sits in the SECOND packet: it is only found because every packet was read
        let (s2, _) = run(&cfg, &Action::Slap { player: "Echo".into() }).await.unwrap();
        assert!(s2.contains("3 Echo"), "{}", s2);
        run(&cfg, &Action::Cyclemap).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let got = seen.lock().unwrap().clone();
        assert_eq!(got, vec!["status", "addip 11.22.33.45", "kick 1 \"Banned: cheating\"", "status", "slap 3", "status", "cyclemap"]);
    }

    #[tokio::test]
    async fn a_wrong_password_a_missing_server_and_a_switched_off_server_are_explained() {
        let (port, seen) = fake_server("right").await;
        let e = run(&server(port, "wrong", true), &Action::Cyclemap).await.unwrap_err();
        assert!(e.contains("refused the rcon password"), "{}", e);
        assert!(seen.lock().unwrap().is_empty());
        // switched off, or switched on without the password: nothing is sent at all
        assert!(run(&server(port, "right", false), &Action::Cyclemap).await.unwrap_err().contains("not switched on"));
        assert!(run(&server(port, "", true), &Action::Cyclemap).await.unwrap_err().contains("not switched on"));
        assert!(seen.lock().unwrap().is_empty());
        // nothing listens there
        let closed = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let p = closed.local_addr().unwrap().port();
        let e = run(&server(p, "x", true), &Action::Cyclemap).await.unwrap_err();
        assert!(e.contains("did not answer"), "{}", e);
    }
}
