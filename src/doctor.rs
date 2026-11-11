// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Self-checks shared by the CLI (`check-servers`, `check-stream`) and the admin panel.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::config::{Config, ServerConfig, SpectatorConfig};
use crate::fs::FileSystems;
use crate::spectator::{Platform, explain_failure};
use crate::urt;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Info,
    Problem,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub label: String,
    pub status: Status,
    pub msg: String,
    pub fix: String,
}

impl Check {
    fn new(label: &str, status: Status, msg: impl Into<String>, fix: &str) -> Check {
        Check { label: label.into(), status, msg: msg.into(), fix: fix.into() }
    }
    pub fn ok(label: &str, msg: impl Into<String>) -> Check { Check::new(label, Status::Ok, msg, "") }
    pub fn info(label: &str, msg: impl Into<String>) -> Check { Check::new(label, Status::Info, msg, "") }
    pub fn bad(label: &str, msg: impl Into<String>, fix: &str) -> Check { Check::new(label, Status::Problem, msg, fix) }
}

pub fn problems(checks: &[Check]) -> usize {
    checks.iter().filter(|c| c.status == Status::Problem).count()
}

pub fn render(title: &str, checks: &[Check]) -> String {
    let mut out = format!("{}\n", title);
    for c in checks {
        let tag = match c.status { Status::Ok => "ok", Status::Info => "..", Status::Problem => "PROBLEM" };
        out.push_str(&format!("  {:<16} {} - {}\n", c.label, tag, c.msg));
        if !c.fix.is_empty() {
            out.push_str(&format!("  {:<16}   -> {}\n", "", c.fix));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------ game server

/// Try the real connections the bot will use: UDP status, and games.log (local or over SFTP).
pub async fn check_server(cfg: &Config, sc: &ServerConfig) -> Vec<Check> {
    let mut v = Vec::new();
    if sc.host.trim().is_empty() {
        return vec![Check::bad("host", "no host set", "enter the IP address or name of the game server")];
    }
    let snap = urt::query_status(&sc.host, sc.port).await;
    if snap.online {
        v.push(Check::ok("UDP status", format!("map {}, {} player(s){}", snap.mapname, snap.players.len(),
            if snap.hostname.is_empty() { String::new() } else { format!(", \"{}\"", snap.hostname) })));
    } else {
        v.push(Check::bad("UDP status", "no answer", &format!("server down, wrong port, or a firewall blocks UDP {} from this machine", sc.port)));
    }
    if sc.log_path.is_empty() {
        v.push(Check::info("games.log", "not configured (scoreboard only, no live kill feed)"));
    } else {
        match FileSystems::new(&cfg.filesystems).and_then(|f| f.resolve(&sc.log_path)) {
            Err(e) => v.push(Check::bad("games.log", e.to_string(), "check log_path and the filesystem name")),
            Ok((fs, path)) => match tokio::time::timeout(Duration::from_secs(25), fs.size(&path)).await {
                Ok(Ok(size)) => v.push(Check::ok("games.log", format!("{} on {}, {} bytes", path, fs.label(), size))),
                Ok(Err(e)) => v.push(Check::bad("games.log", format!("{} ({:#}){}", sc.log_path, e, crate::fs::did_you_mean(&fs.label(), &path)),
                    "the file must exist (add `g_log games.log` and `g_logsync 2` to the game server's config) and be readable")),
                Err(_) => v.push(Check::bad("games.log", "timed out", "the SFTP host did not answer in time")),
            },
        }
    }
    v
}

// ------------------------------------------------------------------------------------------ video

async fn run(argv: &[&str], timeout: u64) -> Option<String> {
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(timeout), cmd.output()).await.ok()?.ok()?;
    Some(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

pub fn which(name: &str) -> Option<String> {
    let p = Path::new(name);
    if p.components().count() > 1 {
        return if p.is_file() { Some(name.to_string()) } else { None };
    }
    let exts: Vec<String> = if cfg!(windows) { vec!["".into(), ".exe".into()] } else { vec!["".into()] };
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for e in &exts {
            let cand = dir.join(format!("{}{}", name, e));
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn tail_lines(path: &Path, n: usize) -> Option<Vec<String>> {
    let data = std::fs::read(path).ok()?;
    let slice = &data[data.len().saturating_sub(6000)..];
    let text = String::from_utf8_lossy(slice).trim().to_string();
    // drop blank lines and fold immediate repeats ("... (x2)") so a repeating warning does not fill the view
    let mut lines: Vec<(String, u32)> = Vec::new();
    for l in text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()) {
        match lines.last_mut() {
            Some((prev, c)) if prev == l => *c += 1,
            _ => lines.push((l.to_string(), 1)),
        }
    }
    let out: Vec<String> = lines.into_iter().map(|(l, c)| {
        let low = l.to_lowercase();
        let harmless = low.contains("libegl warning") || low.contains("mesa:") || low.contains("failed to create dri3");
        format!("{}{}{}", l, if c > 1 { format!("  (x{})", c) } else { String::new() }, if harmless { "   <- harmless: no GPU here, software rendering is used" } else { "" })
    }).collect();
    Some(out[out.len().saturating_sub(n)..].to_vec())
}

pub async fn http_get(port: u16, path: &str, cookie: Option<&str>) -> Result<(u16, String), String> {
    let mut s = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(("127.0.0.1", port)))
        .await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
    let ck = cookie.map(|c| format!("Cookie: {}\r\n", c)).unwrap_or_default();
    s.write_all(format!("GET {} HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n", path, ck).as_bytes()).await.map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("no HTTP answer")?;
    Ok((code, body.to_string()))
}

/// Walk the video pipeline stage by stage and say which stage is broken.
pub async fn check_stream(cfg: &Config, key: &str, sc: &ServerConfig, platform: Platform) -> Vec<Check> {
    let Some(sp) = &sc.spectator else {
        return vec![Check::info("video", "this server has no \"spectator\" section, so there is no video to check")];
    };
    let mut v = Vec::new();
    v.push(Check::info("settings", format!("output={}, {}x{} @ {} fps", sp.output, sp.width, sp.height, sp.fps)));
    prerequisites(&mut v, sp, platform).await;
    encoder_check(&mut v, sp, platform).await;

    let work = std::env::current_dir().unwrap_or_default().join(&sp.work_dir).join(key);
    let hls = work.join("hls");
    let mut probe = work.clone();
    while !probe.exists() && probe.pop() {}
    if std::fs::metadata(&probe).map(|m| !m.permissions().readonly()).unwrap_or(false) {
        v.push(Check::ok("work dir", format!("{} (writable)", work.display())));
    } else {
        v.push(Check::bad("work dir", format!("{} cannot be written by this user", work.display()), "make it writable for the user the bot runs as"));
    }
    v.push(write_access(&work).await);
    let m3u8 = hls.join("live.m3u8");
    match std::fs::metadata(&m3u8).and_then(|m| m.modified()) {
        Ok(t) => {
            let age = SystemTime::now().duration_since(t).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let segs = std::fs::read_dir(&hls).map(|d| d.filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("seg_")).count()).unwrap_or(0);
            if age < 15.0 {
                v.push(Check::ok("stream output", format!("ffmpeg is writing HLS right now ({} segments, playlist updated {:.0}s ago)", segs, age)));
            } else {
                v.push(Check::bad("stream output", format!("live.m3u8 exists but was last written {:.0}s ago - ffmpeg is not running", age),
                    "is a broadcast with video running? (see the process logs below)"));
            }
        }
        Err(_) => v.push(Check::info("stream output", "no live.m3u8 yet - nothing is being produced. Video only starts when a broadcast is started with stream:True (/gtv start ... stream:True) or /gtv stream enable:True")),
    }
    for name in ["xvfb", "client", "ffmpeg"] {
        let Some(lines) = tail_lines(&work.join("logs").join(format!("{}.log", name)), 4) else { continue };
        if lines.is_empty() {
            v.push(Check::info(&format!("{}.log", name), "(empty)"));
            continue;
        }
        v.push(Check::info(&format!("{}.log", name), format!("last lines:\n{}", lines.iter().map(|l| format!("      {}", l.chars().take(150).collect::<String>())).collect::<Vec<_>>().join("\n"))));
        if let Some(h) = explain_failure(&lines.join("\n")) {
            let culprit = lines.iter().find(|l| explain_failure(l) == Some(h)).map(|l| l.chars().take(200).collect::<String>()).unwrap_or_default();
            v.push(Check::bad("hint", format!("{}{}", h, if culprit.is_empty() { String::new() } else { format!("\n      from {}.log: {}", name, culprit.trim()) }), ""));
        }
    }
    if sp.output == "hls" || sp.output == "both" {
        match &cfg.web {
            None => v.push(Check::bad("web player", format!("output is \"{}\" but there is no \"web\" section in the config", sp.output), "add the web player settings (Settings tab)")),
            Some(web) => web_probe(&mut v, web.port, &cfg.web_token(), &web.public_url, key).await,
        }
    } else {
        v.push(Check::info("web player", format!("not used (output is rtmp); the stream goes to {}", if sp.rtmp_url.is_empty() { "NO rtmp_url!" } else { "your RTMP URL" })));
    }
    v
}

/// CPU or GPU encoding: for a GPU choice, really test-encode with it and say what to fix.
async fn encoder_check(v: &mut Vec<Check>, sp: &SpectatorConfig, platform: Platform) {
    use crate::gpu::{self, Choice, Encoder};
    match gpu::parse_choice(&sp.encoder) {
        None => v.push(Check::bad("encoder", format!("unknown encoder '{}'", sp.encoder), "use cpu, auto, nvenc, vaapi, qsv or amf")),
        Some(Choice::Fixed(Encoder::Cpu)) => v.push(Check::info("encoder", format!("CPU (x264, preset {}). If the machine is struggling, try the GPU option and press \"Detect GPU\"", sp.x264_preset))),
        Some(Choice::Fixed(e)) if !e.supported_on(platform) => v.push(Check::bad("encoder", format!("{} is not available on this operating system", e.label()), "choose cpu or auto")),
        Some(Choice::Fixed(e)) => {
            if which(&sp.ffmpeg_binary).is_none() {
                return; // the ffmpeg check already says so
            }
            let t = gpu::test_encoder(e, sp).await;
            if t.ok {
                v.push(Check::ok("encoder", format!("{}: {}", e.label(), t.detail)));
            } else {
                v.push(Check::bad("encoder", format!("{} does not work here: {}", e.label(), t.detail), &format!("{} - or set encoder to cpu / auto", t.fix)));
            }
        }
        Some(Choice::Auto) => {
            if which(&sp.ffmpeg_binary).is_none() {
                return;
            }
            let r = gpu::detect(sp, platform).await;
            let used = r.tests.iter().find(|t| t.encoder == r.recommended);
            v.push(Check::ok("encoder", format!("auto -> {}{}", r.recommended.label(), used.map(|t| format!(": {}", t.detail)).unwrap_or_default())));
            for t in r.tests.iter().filter(|t| t.encoder != Encoder::Cpu && t.available && !t.ok) {
                v.push(Check::info(&format!("  {}", t.encoder.setting()), format!("not usable: {}", t.detail)));
            }
        }
    }
}

async fn prerequisites(v: &mut Vec<Check>, sp: &SpectatorConfig, platform: Platform) {
    let cb = Path::new(&sp.client_binary);
    if cb.is_file() {
        v.push(Check::ok("UrT client", &sp.client_binary));
        if platform == Platform::Linux && which("ldd").is_some() {
            if let Some(out) = run(&["ldd", &sp.client_binary], 15).await {
                let missing: Vec<&str> = out.lines().filter(|l| l.contains("=> not found")).filter_map(|l| l.split_whitespace().next()).collect();
                if missing.is_empty() {
                    v.push(Check::ok("client libs", "all shared libraries found"));
                } else {
                    v.push(Check::bad("client libs", format!("missing: {}", missing.join(", ")),
                        "e.g. libSDL-1.2.so.0 -> sudo apt-get install libsdl1.2debian ; then re-run this check"));
                }
            }
        }
    } else {
        v.push(Check::bad("UrT client", format!("{} is missing or not executable{}", sp.client_binary, crate::fs::did_you_mean("local disk", &sp.client_binary)), "fix the client_binary path (the Browse button in the panel picks it for you)"));
    }
    let gd = Path::new(&sp.base_path).join(&sp.game_dir);
    let paks = std::fs::read_dir(&gd).map(|d| d.filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "pk3")).count()).unwrap_or(0);
    if paks > 0 {
        v.push(Check::ok("UrT data", format!("{} .pk3 files in {}", paks, gd.display())));
    } else {
        v.push(Check::bad("UrT data", format!("no .pk3 files in {}{}", gd.display(), crate::fs::did_you_mean("local disk", &gd.to_string_lossy())), &format!("base_path must be the folder that contains {}/ with the game's pk3 files", sp.game_dir)));
    }
    if platform == Platform::Linux {
        if sp.manage_display {
            match which(&sp.xvfb_binary) {
                Some(p) => v.push(Check::ok("Xvfb", p)),
                None => v.push(Check::bad("Xvfb", format!("{} not found", sp.xvfb_binary), "sudo apt-get install xvfb   (or run: forge-gtv install-video-deps)")),
            }
        }
        match which(&sp.xdotool_binary) {
            Some(p) => v.push(Check::ok("xdotool", p)),
            None => v.push(Check::bad("xdotool", format!("{} not found", sp.xdotool_binary), "sudo apt-get install xdotool (needed for the camera)   (or run: forge-gtv install-video-deps)")),
        }
    } else {
        v.push(Check::info("Windows video", "experimental: the client runs on your desktop (the PC must stay logged in) and ffmpeg captures it with gdigrab"));
    }
    match which(&sp.ffmpeg_binary) {
        None => v.push(Check::bad("ffmpeg", format!("{} not found", sp.ffmpeg_binary), if platform == Platform::Windows { "install a full ffmpeg build and add it to PATH (or run: forge-gtv install-video-deps - uses winget)" } else { "sudo apt-get install ffmpeg   (or run: forge-gtv install-video-deps)" })),
        Some(p) => {
            v.push(Check::ok("ffmpeg", &p));
            let formats = run(&[&p, "-hide_banner", "-formats"], 15).await.unwrap_or_default();
            let encoders = run(&[&p, "-hide_banner", "-encoders"], 15).await.unwrap_or_default();
            let grab = if platform == Platform::Windows { "gdigrab" } else { "x11grab" };
            for (label, hay, needle, fix) in [
                (grab, &formats, grab, "this ffmpeg build cannot capture the screen - install the full ffmpeg package (snap/minimal builds often lack it)"),
                ("libx264", &encoders, "libx264", "this ffmpeg build has no H.264 encoder - install the full ffmpeg package"),
                ("hls muxer", &formats, " hls ", "this ffmpeg build has no HLS output"),
            ] {
                if hay.contains(needle) {
                    v.push(Check::ok(&format!("ffmpeg {}", label), "available"));
                } else {
                    v.push(Check::bad(&format!("ffmpeg {}", label), "not available", fix));
                }
            }
            if sp.audio == "pulse" && platform == Platform::Linux && !formats.contains("pulse") {
                v.push(Check::bad("ffmpeg pulse", "no PulseAudio input", "use \"audio\": \"silent\" or install a full ffmpeg"));
            }
        }
    }
}

/// Really try to write in the stream work folder (and into the files already there), and say who owns what.
async fn write_access(work: &Path) -> Check {
    let mut bad: Vec<String> = Vec::new();
    let mut targets: Vec<PathBuf> = vec![work.to_path_buf()];
    for sub in ["logs", "hls", "home"] {
        targets.push(work.join(sub));
    }
    for d in targets {
        if !d.exists() {
            continue;
        }
        let probe = d.join(format!(".gtv-write-test-{}", std::process::id()));
        match std::fs::write(&probe, b"x") {
            Ok(_) => { let _ = std::fs::remove_file(&probe); }
            Err(e) => bad.push(format!("{} (folder: {})", d.display(), e)),
        }
        // files left behind by an earlier run as another user
        if let Ok(rd) = std::fs::read_dir(&d) {
            for f in rd.filter_map(Result::ok).take(200) {
                let p = f.path();
                if p.is_file() && std::fs::OpenOptions::new().append(true).open(&p).is_err() {
                    bad.push(format!("{} (file cannot be written)", p.display()));
                }
            }
        }
    }
    if bad.is_empty() {
        return Check::ok("write access", format!("this user can write in {}", work.display()));
    }
    let me = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "the bot's user".into());
    let owner = if cfg!(unix) { run(&["stat", "-c", "%U", &work.to_string_lossy()], 5).await.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) } else { None };
    let shown: Vec<String> = bad.iter().take(5).map(|b| format!("      {}", b)).collect();
    Check::bad("write access",
        format!("the bot runs as '{}' but cannot write here ({}):\n{}", me, owner.map(|o| format!("folder owner: {}", o)).unwrap_or_else(|| "owner unknown".into()), shown.join("\n")),
        &format!("sudo chown -R {} '{}'   (this usually happens when the bot was first started with sudo/as root, then runs as a normal user)", me, work.display()))
}

fn public_url_check(public_url: &str) -> Check {
    let fixed = crate::config::fix_public_url(public_url);
    if let Err(e) = &fixed {
        return Check::bad("public url", e.clone(), "fix web.public_url in Settings - the Discord Watch button needs a valid http(s):// address");
    }
    let public_url = &fixed.unwrap_or_default();
    if public_url.is_empty() {
        return Check::bad("public url", "web.public_url is empty, so the Discord button cannot link to the player", "set the public URL in Settings");
    }
    let host = public_url.split("://").nth(1).unwrap_or(public_url).split(['/', ':']).next().unwrap_or("");
    let private = host == "localhost" || host.starts_with("127.") || host.starts_with("10.") || host.starts_with("192.168.")
        || host.strip_prefix("172.").and_then(|r| r.split('.').next()).and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (16..=31).contains(&n));
    if private {
        Check::info("public url", format!("{}  is a private/LAN address: only viewers on the same network (or this computer) can open the Discord button. For friends elsewhere use your public address, a port-forward, or a tunnel", public_url))
    } else {
        Check::info("public url", format!("{}  (viewers need this reachable; the page also loads hls.js from cdn.jsdelivr.net in their browser)", public_url))
    }
}

async fn web_probe(v: &mut Vec<Check>, port: u16, token: &str, public_url: &str, key: &str) {
    let cookie = if token.is_empty() { None } else { Some(format!("gtv={}", token)) };
    v.push(public_url_check(public_url));
    match http_get(port, "/healthz", None).await {
        Ok((200, _)) => v.push(Check::ok("web player", format!("web server answers on port {}", port))),
        other => {
            v.push(Check::bad("web player", format!("the bot's web server is not answering on port {} ({})", port, other.map(|o| o.0.to_string()).unwrap_or_else(|e| e)),
                "is the bot running? Is another program using the port?"));
            return;
        }
    }
    match http_get(port, &format!("/hls/{}/live.m3u8", key), cookie.as_deref()).await {
        Ok((200, body)) => v.push(Check::ok("playlist", format!("viewers get a playlist ({} segment lines)", body.matches(".ts").count()))),
        Ok((404, _)) => v.push(Check::info("playlist", format!("404: the bot has no running stream for '{}' right now (start one with stream:True)", key))),
        Ok((403, _)) => v.push(Check::bad("playlist", "403: token mismatch", "the config's web token differs from the one the running bot uses; restart the bot")),
        Ok((503, _)) => v.push(Check::info("playlist", "503 'stream not ready': ffmpeg has produced less video than your delay (the first delay+20 seconds are normal); if it stays like this, ffmpeg is not producing segments")),
        Ok((c, b)) => v.push(Check::bad("playlist", format!("unexpected answer {} {}", c, b.chars().take(80).collect::<String>()), "")),
        Err(e) => v.push(Check::bad("playlist", e, "")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::config::WebConfig;
    #[cfg(unix)]
    use crate::logbuf::LogBuffer;
    use crate::util::Stop;
    #[cfg(unix)]
    use std::sync::Arc;

    #[tokio::test]
    async fn server_check_reports_udp_and_log_problems_in_words() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let stop = Stop::new();
        let mut rx = stop.rx();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                tokio::select! {
                    r = sock.recv_from(&mut buf) => {
                        let (n, from) = r.unwrap();
                        if buf[..n].starts_with(b"\xff\xff\xff\xffgetstatus") {
                            let _ = sock.send_to(b"\xff\xff\xff\xffstatusResponse\n\\mapname\\ut4_abbey\\sv_hostname\\Hi\n5 40 \"A\"\n", from).await;
                        }
                    }
                    _ = rx.sleep(Duration::from_secs(3600)) => return,
                }
            }
        });
        let cfg = Config::default();
        let dir = std::env::temp_dir().join(format!("gtv-doc-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("games.log"), "abc").unwrap();
        let mut sc = ServerConfig { host: "127.0.0.1".into(), port, log_path: dir.join("games.log").to_string_lossy().into_owned(), ..Default::default() };
        let v = check_server(&cfg, &sc).await;
        assert_eq!(v[0].status, Status::Ok);
        assert!(v[0].msg.contains("ut4_abbey") && v[1].msg.contains("3 bytes"), "{:?}", v);
        sc.log_path = dir.join("nope.log").to_string_lossy().into_owned();
        sc.port = 1; // nothing listens there
        let v = check_server(&cfg, &sc).await;
        assert_eq!(problems(&v), 2);
        assert!(render("t", &v).contains("PROBLEM"));
        sc.log_path = "ghost:/x.log".into();
        assert!(check_server(&cfg, &sc).await[1].msg.contains("unknown filesystem"));
        sc.host = " ".into();
        assert_eq!(check_server(&cfg, &sc).await.len(), 1);
        stop.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stream_check_finds_missing_pieces_and_reads_logs() {
        let dir = std::env::temp_dir().join(format!("gtv-sc-{}", crate::config::random_token(4)));
        let work = dir.join("work").join("m");
        std::fs::create_dir_all(work.join("logs")).unwrap();
        std::fs::create_dir_all(work.join("hls")).unwrap();
        std::fs::write(work.join("logs").join("ffmpeg.log"), "x\n[x11grab @ 0x1] Cannot open display :90.0, error 1.\n").unwrap();
        std::fs::write(work.join("hls").join("live.m3u8"), "#EXTM3U\n").unwrap();
        let sp = SpectatorConfig {
            client_binary: dir.join("nope").to_string_lossy().into_owned(), base_path: dir.to_string_lossy().into_owned(),
            work_dir: dir.join("work").to_string_lossy().into_owned(), ffmpeg_binary: "no-such-ffmpeg".into(),
            xvfb_binary: "no-such-xvfb".into(), xdotool_binary: "no-such-xdotool".into(), ..Default::default()
        };
        let sc = ServerConfig { key: "m".into(), host: "h".into(), spectator: Some(sp), ..Default::default() };
        let mut cfg = Config::default();
        cfg.web = Some(WebConfig { port: 1, ..Default::default() });
        let v = check_stream(&cfg, "m", &sc, Platform::Linux).await;
        let text = render("video", &v);
        for needle in ["is missing or not executable", "no .pk3 files", "no-such-ffmpeg not found", "no-such-xvfb not found",
                       "no-such-xdotool not found", "Cannot open display :90.0", "X display is not reachable", "not answering on port 1",
                       "public_url is empty"] {
            assert!(text.contains(needle), "missing {:?} in\n{}", needle, text);
        }
        let none = ServerConfig { key: "x".into(), ..Default::default() };
        assert!(check_stream(&cfg, "x", &none, Platform::Linux).await[0].msg.contains("no video to check"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = (Arc::new(LogBuffer::new()),);
    }

    #[test]
    fn public_url_flags_lan_addresses() {
        assert!(public_url_check("").msg.contains("empty"));
        for u in ["http://10.0.0.134:8080", "http://192.168.1.5", "http://172.20.1.1:80/x", "http://localhost:8080"] {
            assert!(public_url_check(u).msg.contains("private/LAN"), "{}", u);
        }
        for u in ["http://172.32.0.1", "https://gtv.example.com", "http://8.8.8.8:80"] {
            assert!(!public_url_check(u).msg.contains("private/LAN"), "{}", u);
        }
    }

    #[tokio::test]
    async fn write_access_reports_what_cannot_be_written() {
        let dir = std::env::temp_dir().join(format!("gtv-wa-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        let c = write_access(&dir).await;
        assert_eq!(c.status, Status::Ok, "{}", c.msg);
        assert!(std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).all(|e| !e.file_name().to_string_lossy().starts_with(".gtv-write-test")));
        // a plain file where a folder should be cannot be written into (works even as root)
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let c = write_access(&blocked).await;
        assert_eq!(c.status, Status::Problem);
        assert!(c.fix.contains("chown -R") && c.msg.contains("cannot write"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn log_tail_folds_repeats_drops_blanks_and_marks_gpu_noise() {
        let f = std::env::temp_dir().join(format!("gtv-tail-{}.log", crate::config::random_token(4)));
        std::fs::write(&f, "\nlibEGL warning: failed to open /dev/dri/card0: Permission denied\n\nlibEGL warning: failed to open /dev/dri/card0: Permission denied\nreal line\n").unwrap();
        let t = tail_lines(&f, 4).unwrap();
        assert_eq!(t.len(), 2, "{:?}", t);
        assert!(t[0].contains("(x2)") && t[0].contains("harmless"));
        assert_eq!(t[1], "real line");
        let _ = std::fs::remove_file(f);
    }

    #[test]
    fn which_finds_things_on_path() {
        assert!(which("sh").is_some() || cfg!(windows));
        assert!(which("definitely-not-a-program-xyz").is_none());
    }
}
