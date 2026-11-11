// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Spectator client + ffmpeg streaming + auto-director.
//!
//! Linux:    Xvfb  <-  UrT client (spectator)  ->  ffmpeg x11grab -> HLS files / RTMP
//! Windows:  (experimental) the UrT client on the desktop  ->  ffmpeg gdigrab -> HLS files / RTMP
//!
//! The client is steered without console access: the boot config binds F9 to `exec gtv_cmd.cfg`;
//! to run a command we write that file and press the key (xdotool on Linux, SendKeys on Windows).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::config::SpectatorConfig;
use crate::fs::Fs;
use crate::hls::{WINDOW, delayed_view};
use crate::urt::{self, Event, Kind, Snapshot, strip_colors};
use crate::util::{Stop, StopRx};

pub const CMD_KEY: &str = "F9";
pub const CMD_CFG: &str = "gtv_cmd.cfg";
pub const BOOT_CFG: &str = "gtv_boot.cfg";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Windows,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(windows) { Platform::Windows } else { Platform::Linux }
    }
}

/// A camera target: a client number when known, else a (safe) player name.
#[derive(Clone, Debug, PartialEq)]
pub enum CamRef {
    Slot(i32),
    Name(String),
}

const SAFE_NAME: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _-.[]()|+=!@#&";

/// A player name made safe to embed in a quoted console argument (or None).
pub fn safe_name_ref(name: &str) -> Option<String> {
    let n = name.trim();
    if !n.is_empty() && n.chars().all(|c| SAFE_NAME.contains(c)) { Some(n.to_string()) } else { None }
}

/// The keys of a `spectate_key` setting ("Escape 4"): None if a name is not a plain key name or there are too many.
pub fn spectate_keys(setting: &str) -> Option<Vec<String>> {
    let keys: Vec<String> = setting.split_whitespace().map(String::from).collect();
    let ok = keys.len() <= 4 && keys.iter().all(|k| k.len() <= 16 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    ok.then_some(keys)
}

fn is_escape(k: &str) -> bool {
    k.eq_ignore_ascii_case("escape") || k.eq_ignore_ascii_case("esc")
}

pub fn build_boot_cfg(cfg: &SpectatorConfig) -> String {
    let name = cfg.client_name.replace(['"', ';'], "");
    let mut b = format!("seta name \"{}\"\nseta cl_autoRecordDemo 0\nseta s_volume 0.6\nbind {} \"exec {}\"\n", name, CMD_KEY, CMD_CFG);
    if cfg.hide_chat {
        // show no chat lines (public or team) and no console notices on screen, so nothing typed in the game reaches the video
        b.push_str("seta cg_chatHeight 0\nseta cg_teamChatHeight 0\nseta cg_chatTime 0\nseta cg_teamChatTime 0\nseta con_notifytime 0\n");
    }
    b
}

pub fn build_client_argv(cfg: &SpectatorConfig, host: &str, port: u16, home: &str, platform: Platform) -> Vec<String> {
    let s = |x: &str| x.to_string();
    let mut a = vec![
        s(&cfg.client_binary),
        s("+set"), s("fs_basepath"), s(&cfg.base_path),
        s("+set"), s("fs_homepath"), s(home),
        s("+set"), s("com_introplayed"), s("1"),
        s("+set"), s("r_fullscreen"), s("0"),
        s("+set"), s("r_mode"), s("-1"),
        s("+set"), s("r_customwidth"), cfg.width.to_string(),
        s("+set"), s("r_customheight"), cfg.height.to_string(),
        s("+set"), s("r_swapinterval"), s("0"),
        s("+set"), s("com_maxfps"), cfg.fps.to_string(),
        s("+set"), s("in_nograb"), s("1"),
        s("+set"), s("cl_allowDownload"), s("1"),
    ];
    if platform == Platform::Windows {
        // pin the window to the top-left corner so the desktop capture frames it exactly
        a.extend([s("+set"), s("vid_xpos"), s("0"), s("+set"), s("vid_ypos"), s("0")]);
    }
    if !cfg.password.is_empty() {
        a.extend([s("+set"), s("password"), s(&cfg.password)]);
    }
    a.extend(cfg.extra_client_args.iter().cloned());
    a.extend([s("+exec"), s(BOOT_CFG), s("+connect"), format!("{}:{}", host, port)]);
    a
}

pub fn build_ffmpeg_argv(cfg: &SpectatorConfig, display: &str, hls_dir: &str, platform: Platform, enc: crate::gpu::Encoder) -> Result<Vec<String>> {
    if (cfg.output == "rtmp" || cfg.output == "both") && cfg.rtmp_url.is_empty() {
        bail!("output includes rtmp but rtmp_url is empty");
    }
    let s = |x: &str| x.to_string();
    let seg = cfg.segment_seconds;
    let ea = crate::gpu::encoder_args(enc, cfg);
    let mut a = vec![s(&cfg.ffmpeg_binary), s("-hide_banner"), s("-loglevel"), s("warning"), s("-nostdin")];
    a.extend(ea.pre_input.iter().cloned());
    match platform {
        Platform::Linux => a.extend([
            s("-f"), s("x11grab"), s("-framerate"), cfg.fps.to_string(),
            s("-video_size"), format!("{}x{}", cfg.width, cfg.height), s("-draw_mouse"), s("0"),
            s("-i"), format!("{}.0", display),
        ]),
        Platform::Windows => {
            a.extend([s("-f"), s("gdigrab"), s("-framerate"), cfg.fps.to_string(), s("-draw_mouse"), s("0")]);
            if cfg.window_title.is_empty() {
                a.extend([s("-video_size"), format!("{}x{}", cfg.width, cfg.height), s("-offset_x"), s("0"), s("-offset_y"), s("0"), s("-i"), s("desktop")]);
            } else {
                a.extend([s("-i"), format!("title={}", cfg.window_title)]);
            }
        }
    }
    if cfg.audio == "pulse" && platform == Platform::Linux {
        a.extend([s("-f"), s("pulse"), s("-i"), s(&cfg.pulse_source)]);
    } else {
        a.extend([s("-f"), s("lavfi"), s("-i"), s("anullsrc=channel_layout=stereo:sample_rate=44100")]);
    }
    a.extend([s("-map"), s("0:v"), s("-map"), s("1:a")]);
    a.extend(ea.codec);
    a.extend([
        s("-vf"), ea.vf,
        s("-b:v"), format!("{}k", cfg.video_kbps), s("-maxrate"), format!("{}k", cfg.video_kbps),
        s("-bufsize"), format!("{}k", cfg.video_kbps * 2),
        s("-force_key_frames"), format!("expr:gte(t,n_forced*{})", seg), s("-sc_threshold"), s("0"),
        s("-c:a"), s("aac"), s("-b:a"), s("128k"), s("-ar"), s("44100"),
    ]);
    let seg_pattern = format!("{}/seg_%06d.ts", hls_dir);
    let hls_opts = format!("hls_time={}:hls_list_size=120:hls_flags=delete_segments+independent_segments:hls_segment_filename={}", seg, seg_pattern);
    match cfg.output.as_str() {
        "hls" => a.extend([
            s("-f"), s("hls"), s("-hls_time"), seg.to_string(), s("-hls_list_size"), s("120"),
            s("-hls_flags"), s("delete_segments+independent_segments"),
            s("-hls_segment_filename"), seg_pattern, format!("{}/live.m3u8", hls_dir),
        ]),
        "rtmp" => a.extend([s("-f"), s("flv"), s(&cfg.rtmp_url)]),
        "both" => a.extend([
            s("-flags"), s("+global_header"), s("-f"), s("tee"),
            format!("[f=hls:{}]{}/live.m3u8|[f=flv]{}", hls_opts, hls_dir, cfg.rtmp_url),
        ]),
        other => bail!("unknown output mode '{}'", other),
    }
    Ok(a)
}

const HINTS: &[(&str, &str)] = &[
    ("cannot open display", "the X display is not reachable - is Xvfb installed and running, and does `display`/`manage_display` match?"),
    ("can't open display", "the X display is not reachable - is Xvfb installed and running, and does `display`/`manage_display` match?"),
    ("could not create server lock file", "Xvfb cannot use this display number: a leftover /tmp/.X<n>-lock belongs to another user (an earlier run with sudo?). The bot now skips such numbers; delete the old files with sudo rm -f /tmp/.X*-lock /tmp/.X11-unix/X* if it persists"),
    ("server is already active for display", "another X server (or a stale lock) already holds this display number - the bot picks the next free one automatically"),
    ("unknown input format: 'x11grab'", "this ffmpeg build has no x11grab - install your distribution's full ffmpeg package (snap/static/minimal builds often lack it)"),
    ("unknown input format: 'gdigrab'", "this ffmpeg build has no gdigrab - use a full Windows build of ffmpeg (e.g. the gyan.dev 'full' or 'essentials' build)"),
    ("unknown encoder 'libx264'", "this ffmpeg build has no libx264 encoder - install your distribution's full ffmpeg package"),
    ("unknown input format: 'pulse'", "this ffmpeg build has no PulseAudio input - use \"audio\": \"silent\" or install a full ffmpeg"),
    ("outside the screen size", "the virtual screen is smaller than width x height - check `width`/`height` (and `display` if you manage Xvfb yourself)"),
    ("permission denied", "a file or folder cannot be accessed by the bot's user (see the line below for which one) - check its owner/permissions; the stream/ folder itself is tested separately under \"write access\""),
    ("cannot load libcuda", "the NVIDIA encoder cannot start - install the NVIDIA driver (`nvidia-smi` must work) or set encoder to cpu"),
    ("no capable devices found", "the NVIDIA encoder found no usable GPU - set encoder to cpu, or run the GPU test in the panel"),
    ("failed to initialise vaapi", "the VAAPI GPU encoder cannot start - run the GPU test in the panel, or set encoder to cpu"),
    ("error creating a mfx session", "the Quick Sync encoder cannot start - run the GPU test in the panel, or set encoder to cpu"),
    ("connection refused", "the target refused the connection - check the RTMP URL / PulseAudio server"),
    ("input/output error", "the output could not be written - check the RTMP URL, stream key and your internet connection"),
    ("error opening output", "the output could not be opened - check `rtmp_url` and that work_dir is writable"),
    ("libsdl-1.2", "the UrT client needs SDL 1.2: sudo apt-get install libsdl1.2debian (Debian/Ubuntu)"),
    ("error while loading shared libraries", "the UrT client is missing a system library - run `ldd` on it to list what is missing"),
];

/// A one-line human hint for the most common reasons ffmpeg / the client / Xvfb die at start-up.
pub fn explain_failure(text: &str) -> Option<&'static str> {
    let low = text.to_lowercase();
    // Lines that look like errors but are harmless noise on a headless machine (Mesa/EGL probing the GPU and
    // falling back to software rendering) must not be reported as the cause of a failure.
    let relevant: Vec<&str> = low.lines().filter(|l| !is_benign(l)).collect();
    let low = relevant.join("\n");
    HINTS.iter().find(|(needle, _)| low.contains(needle)).map(|(_, hint)| *hint)
}

pub fn is_benign(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("libegl warning") || l.contains("/dev/dri/") || l.starts_with("mesa:") || l.contains("mesa-loader") || l.contains("failed to open /dev/dri")
        || l.contains("dri2") || l.contains("failed to load driver") || l.contains("glx: failed")
}

// ------------------------------------------------------------------------------------------ display numbers

static CLAIMED: Mutex<std::collections::BTreeSet<u32>> = Mutex::new(std::collections::BTreeSet::new());

fn x_files(n: u32) -> (PathBuf, PathBuf) {
    (PathBuf::from(format!("/tmp/.X{}-lock", n)), PathBuf::from(format!("/tmp/.X11-unix/X{}", n)))
}

/// Find an X display number this account can really use, starting at `first`. Returns the number and a
/// note for every number that had to be skipped (and why). Stale files we own are cleaned up.
pub fn claim_display(first: u32) -> (u32, Vec<String>) {
    let mut notes = Vec::new();
    let mut claimed = CLAIMED.lock().unwrap();
    for n in first..first + 40 {
        if claimed.contains(&n) {
            continue;
        }
        let (lock, sock) = x_files(n);
        if let Ok(text) = std::fs::read_to_string(&lock) {
            let pid: Option<u32> = text.trim().parse().ok();
            if pid.is_some_and(|p| Path::new(&format!("/proc/{}", p)).exists()) {
                notes.push(format!("display :{} is in use by another X server (pid {}) - trying the next one", n, pid.unwrap_or(0)));
                continue;
            }
        }
        let mut blocked = false;
        for f in [&lock, &sock] {
            if f.exists() && std::fs::remove_file(f).is_err() {
                blocked = true;
            }
        }
        if blocked {
            notes.push(format!("display :{} has leftover files in /tmp owned by another user (probably an earlier run with sudo) and this account cannot remove them - trying the next one", n));
            continue;
        }
        claimed.insert(n);
        return (n, notes);
    }
    notes.push(format!("no free X display number between :{} and :{} - using :{} anyway", first, first + 39, first));
    claimed.insert(first);
    (first, notes)
}

// ------------------------------------------------------------------------------------------ the streamer

type SlotLookup = Box<dyn Fn(&str) -> Option<i32> + Send + Sync>;
type NameLookup = Box<dyn Fn(i32) -> Option<String> + Send + Sync>;

#[derive(Default)]
struct Camera {
    label: String,
    mode: String,
    want: Option<CamRef>,
    current: Option<CamRef>,
    pin_until: f64,
    last_switch: f64,
}

#[derive(Default)]
struct Health {
    last_snap: Option<Snapshot>,
    last_seen: f64,
    client_started: f64,
    /// The team the game log says the spectator is on ("spec", "red", ...), if known.
    own_team: Option<String>,
    /// When the client was first seen on the server after (re)starting; when the last map load happened.
    joined_at: f64,
    map_event: f64,
    last_map: String,
    /// The join / map-load moment the spectate key was already pressed for, how often, and when last.
    key_for: f64,
    /// The moment Escape was already pressed for.
    esc_for: f64,
    key_presses: u32,
    last_key: f64,
    /// client_started value for which `team s` was already sent once, and when it was last sent.
    team_cmd_for: f64,
    last_team_cmd: f64,
}

pub struct Streamer {
    pub key: String,
    host: String,
    port: u16,
    pub cfg: SpectatorConfig,
    platform: Platform,
    display_num: AtomicU32,
    display: Mutex<String>,
    name_clean: String,
    work: PathBuf,
    delay: AtomicU32,
    running: AtomicBool,
    cam: Mutex<Camera>,
    health: Mutex<Health>,
    lookups: Mutex<Option<(SlotLookup, NameLookup)>>,
    stop: Mutex<Stop>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    restart: Mutex<HashMap<&'static str, Arc<Notify>>>,
    publish: Option<(Arc<dyn Fs>, String)>,
    cmd_lock: tokio::sync::Mutex<()>,
}

impl Streamer {
    pub fn new(key: &str, host: &str, port: u16, cfg: &SpectatorConfig, display_num: u32, delay: u32,
               publish: Option<(Arc<dyn Fs>, String)>) -> Result<Arc<Streamer>> {
        Streamer::with_platform(key, host, port, cfg, display_num, delay, publish, Platform::current())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_platform(key: &str, host: &str, port: u16, cfg: &SpectatorConfig, display_num: u32, delay: u32,
                         publish: Option<(Arc<dyn Fs>, String)>, platform: Platform) -> Result<Arc<Streamer>> {
        if (cfg.output == "rtmp" || cfg.output == "both") && cfg.rtmp_url.is_empty() {
            bail!("output includes rtmp but rtmp_url is empty");
        }
        let display = match platform {
            Platform::Windows => String::new(),
            Platform::Linux if cfg.manage_display => format!(":{}", display_num),
            Platform::Linux if !cfg.display.is_empty() => cfg.display.clone(),
            Platform::Linux => std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
        };
        let work = std::env::current_dir().unwrap_or_default().join(&cfg.work_dir).join(key);
        Ok(Arc::new(Streamer {
            key: key.into(), host: host.into(), port, cfg: cfg.clone(), platform, display_num: AtomicU32::new(display_num), display: Mutex::new(display),
            name_clean: strip_colors(&cfg.client_name), work,
            delay: AtomicU32::new(delay), running: AtomicBool::new(false),
            cam: Mutex::new(Camera { mode: "auto".into(), ..Default::default() }),
            health: Mutex::new(Health::default()),
            lookups: Mutex::new(None), stop: Mutex::new(Stop::new()), tasks: Mutex::new(vec![]),
            restart: Mutex::new(HashMap::new()), publish, cmd_lock: tokio::sync::Mutex::new(()),
        }))
    }

    fn display(&self) -> String { self.display.lock().unwrap().clone() }
    fn display_num(&self) -> u32 { self.display_num.load(Ordering::SeqCst) }

    #[cfg(test)]
    pub fn mark_running_for_test(&self) { self.running.store(true, Ordering::SeqCst); }

    pub fn hls_dir(&self) -> PathBuf { self.work.join("hls") }
    pub fn home_dir(&self) -> PathBuf { self.work.join("home") }
    pub fn logs_dir(&self) -> PathBuf { self.work.join("logs") }
    pub fn is_running(&self) -> bool { self.running.load(Ordering::SeqCst) }
    pub fn delay(&self) -> u32 { self.delay.load(Ordering::Relaxed) }
    pub fn set_delay(&self, d: u32) { self.delay.store(d, Ordering::Relaxed) }
    pub fn wants_web(&self) -> bool { self.cfg.output == "hls" || self.cfg.output == "both" }
    pub fn watch_url_override(&self) -> String { self.cfg.watch_url.clone() }
    pub fn name_clean(&self) -> &str { &self.name_clean }

    pub fn camera(&self) -> (String, String) {
        let c = self.cam.lock().unwrap();
        (c.label.clone(), c.mode.clone())
    }

    pub fn set_lookups(&self, slot: SlotLookup, name: NameLookup) {
        *self.lookups.lock().unwrap() = Some((slot, name));
    }

    fn slot_lookup(&self, name: &str) -> Option<i32> {
        self.lookups.lock().unwrap().as_ref().and_then(|(s, _)| s(name))
    }

    fn name_lookup(&self, slot: i32) -> Option<String> {
        self.lookups.lock().unwrap().as_ref().and_then(|(_, n)| n(slot))
    }

    // ---- lifecycle ----

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        *self.stop.lock().unwrap() = Stop::new();
        let rx = || self.stop.lock().unwrap().rx();
        let _ = tokio::fs::remove_dir_all(self.hls_dir()).await;
        for d in [self.hls_dir(), self.home_dir().join(&self.cfg.game_dir), self.logs_dir()] {
            tokio::fs::create_dir_all(&d).await?;
        }
        tokio::fs::write(self.home_dir().join(&self.cfg.game_dir).join(BOOT_CFG), build_boot_cfg(&self.cfg)).await?;

        let mut tasks = Vec::new();
        if self.platform == Platform::Linux && self.cfg.manage_display {
            // Take a display number nobody else is using. A leftover lock/socket from another user (say an earlier
            // run under sudo) cannot be removed by this account and would make Xvfb die while the stale socket
            // still "exists" - so skip such numbers instead of fighting over them.
            let (num, notes) = claim_display(self.display_num());
            self.display_num.store(num, Ordering::SeqCst);
            *self.display.lock().unwrap() = format!(":{}", num);
            for n in notes {
                tracing::warn!("[{}] {}", self.key, n);
            }
            let argv = vec![self.cfg.xvfb_binary.clone(), self.display(), "-screen".into(), "0".into(),
                            format!("{}x{}x24", self.cfg.width, self.cfg.height), "-nolisten".into(), "tcp".into(), "-ac".into()];
            tasks.push(self.spawn_supervised("xvfb", Ok(argv), self.env(), None, rx()));
            self.wait_display(15.0).await;
        }
        let env = self.env();
        let home = self.home_dir().to_string_lossy().into_owned();
        let argv = build_client_argv(&self.cfg, &self.host, self.port, &home, self.platform);
        tasks.push(self.spawn_supervised("client", Ok(argv), env.clone(), Some(PathBuf::from(&self.cfg.base_path)), rx()));
        self.health.lock().unwrap().client_started = urt::now();
        let hls = self.hls_dir().to_string_lossy().into_owned();
        let (enc, note) = crate::gpu::resolve(&self.cfg, self.platform).await;
        tracing::info!("[{}] {}", self.key, note);
        let argv = build_ffmpeg_argv(&self.cfg, &self.display(), &hls, self.platform, enc);
        tasks.push(self.spawn_supervised("ffmpeg", argv, env, None, rx()));
        tasks.push(tokio::spawn(self.clone().watchdog_loop(rx())));
        tasks.push(tokio::spawn(self.clone().director_loop(rx())));
        if let Some((fs, base)) = self.publish.clone() {
            tasks.push(tokio::spawn(publisher_loop(fs, base, self.hls_dir(), self.clone(), rx())));
        }
        *self.tasks.lock().unwrap() = tasks;
        tracing::info!("[{}] spectator pipeline started{}", self.key, { let d = self.display(); if d.is_empty() { String::new() } else { format!(" on {}", d) } });
        Ok(())
    }

    pub async fn stop(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        self.stop.lock().unwrap().stop();
        let tasks: Vec<_> = self.tasks.lock().unwrap().drain(..).collect();
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(8), t).await;
        }
        if self.platform == Platform::Linux && self.cfg.manage_display {
            CLAIMED.lock().unwrap().remove(&self.display_num());
        }
        tracing::info!("[{}] spectator pipeline stopped", self.key);
    }

    fn env(&self) -> Vec<(String, String)> {
        let mut e = Vec::new();
        if self.platform == Platform::Linux {
            e.push(("DISPLAY".to_string(), self.display()));
            if self.cfg.software_gl {
                e.push(("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()));
            }
            e.push(("SDL_AUDIODRIVER".into(), if self.cfg.audio == "pulse" { "pulseaudio" } else { "dummy" }.into()));
        }
        e
    }

    async fn wait_display(&self, timeout: f64) {
        // Xvfb and its unix socket exist only on Linux; on other systems there is nothing to wait for
        #[cfg(not(unix))]
        {
            let _ = timeout;
            return;
        }
        #[cfg(unix)]
        {
            let sock = format!("/tmp/.X11-unix/X{}", self.display_num());
            let end = tokio::time::Instant::now() + Duration::from_secs_f64(timeout);
            while tokio::time::Instant::now() < end {
                // a leftover socket file from a dead server exists but refuses connections
                if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            tracing::warn!("[{}] Xvfb display {} did not come up in {:.0}s - see {}", self.key, self.display(), timeout, self.logs_dir().join("xvfb.log").display());
        }
    }

    fn spawn_supervised(self: &Arc<Self>, name: &'static str, argv: Result<Vec<String>>, env: Vec<(String, String)>,
                        cwd: Option<PathBuf>, stop: StopRx) -> JoinHandle<()> {
        let notify = Arc::new(Notify::new());
        self.restart.lock().unwrap().insert(name, notify.clone());
        tokio::spawn(self.clone().supervise(name, argv, env, cwd, stop, notify))
    }

    /// Keep one child process alive: restart it (with back-off) whenever it exits.
    async fn supervise(self: Arc<Self>, name: &'static str, argv: Result<Vec<String>>, env: Vec<(String, String)>,
                       cwd: Option<PathBuf>, mut stop: StopRx, restart: Arc<Notify>) {
        let argv = match argv {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("[{}] cannot build the {} command: {}", self.key, name, e);
                return;
            }
        };
        let log_path = self.logs_dir().join(format!("{}.log", name));
        let mut backoff = 2.0f64;
        while !stop.is_stopped() {
            let started = urt::now();
            let offset = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
            let spawned = (|| -> std::io::Result<Child> {
                let out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
                let err = out.try_clone()?;
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]).envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(out).stderr(err).kill_on_drop(true);
                if let Some(c) = &cwd {
                    cmd.current_dir(c);
                }
                cmd.spawn()
            })();
            let mut child = match spawned {
                Ok(c) => c,
                Err(e) => {
                    let what = if e.kind() == std::io::ErrorKind::NotFound {
                        format!("'{}' was not found{}", argv[0], match name {
                            "xvfb" => " - install it (sudo apt-get install xvfb) or set the full path of Xvfb in the panel",
                            "client" => " - check the UrT client program path (use the Browse button in the panel) and that the file is there",
                            "ffmpeg" => " - install ffmpeg or set its full path",
                            _ => "",
                        })
                    } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                        format!("'{}' cannot be run by this account - is it executable (chmod +x) and readable?", argv[0])
                    } else {
                        e.to_string()
                    };
                    tracing::error!("[{}] cannot start {}: {}", self.key, name, what);
                    if stop.sleep_secs(30.0).await {
                        return;
                    }
                    continue;
                }
            };
            if name == "client" {
                self.health.lock().unwrap().client_started = urt::now();
            }
            let code = tokio::select! {
                st = child.wait() => st.ok().and_then(|s| s.code()),
                _ = restart.notified() => { let _ = child.kill().await; None }
                _ = stop.sleep(Duration::from_secs(86_400 * 365)) => { let _ = child.kill().await; return; }
            };
            if urt::now() - started > 30.0 {
                backoff = 2.0; // it was healthy for a while
            }
            tracing::warn!("[{}] {} exited ({}); restarting in {:.0}s", self.key, name, code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()), backoff);
            if code != Some(0) {
                self.report_failure(name, &log_path, offset);
            }
            if stop.sleep_secs(backoff).await {
                return;
            }
            backoff = (backoff * 2.0).min(30.0);
        }
    }

    /// Put the last lines the dead process printed into the bot's own log, with a hint if we recognise it.
    fn report_failure(&self, name: &str, path: &Path, offset: u64) {
        let Ok(data) = std::fs::read(path) else { return };
        let tail = &data[(offset as usize).min(data.len())..];
        let text = String::from_utf8_lossy(&tail[..tail.len().min(20_000)]).trim().to_string();
        if text.is_empty() {
            tracing::warn!("[{}] {} printed nothing before it died; full log: {}", self.key, name, path.display());
            return;
        }
        let lines: Vec<&str> = text.lines().collect();
        let last = lines[lines.len().saturating_sub(6)..].join("\n    ");
        tracing::warn!("[{}] last {} output (full log: {}):\n    {}", self.key, name, path.display(), last);
        if let Some(h) = explain_failure(&text) {
            tracing::warn!("[{}] hint: {}", self.key, h);
            if name != "xvfb" && h.contains("X display") {
                self.report_xvfb();
            }
        }
    }

    /// The real reason a display is unreachable is almost always in Xvfb's own log.
    fn report_xvfb(&self) {
        let path = self.logs_dir().join("xvfb.log");
        let Ok(data) = std::fs::read_to_string(&path) else {
            tracing::warn!("[{}] Xvfb has not written a log ({}) - is `{}` installed? try: which {}", self.key, path.display(), self.cfg.xvfb_binary, self.cfg.xvfb_binary);
            return;
        };
        let tail: Vec<&str> = data.lines().filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("> Warning:") && !t.contains("xkbcomp") && !t.starts_with("Errors from xkbcomp")
        }).collect();
        let last = tail[tail.len().saturating_sub(4)..].join("\n    ");
        if !last.is_empty() {
            tracing::warn!("[{}] Xvfb ({}) says:\n    {}", self.key, path.display(), last);
        }
    }

    // ---- health ----

    /// Fed with every status poll; lets us notice if the client dropped.
    pub fn observe(&self, snap: &Snapshot) {
        if !snap.online {
            return;
        }
        let mut h = self.health.lock().unwrap();
        h.last_snap = Some(snap.clone());
        let now = urt::now();
        if snap.players.iter().any(|p| p.name == self.name_clean) {
            h.last_seen = now;
            if h.joined_at < h.client_started {
                h.joined_at = now; // first time on the server since the client (re)started
            }
        }
        // a different map without a game log still counts as a map load
        if !snap.mapname.is_empty() {
            if !h.last_map.is_empty() && h.last_map != snap.mapname && now - h.map_event > 10.0 {
                h.map_event = now;
            }
            h.last_map = snap.mapname.clone();
        }
    }

    /// The game log said a map was (re)loaded (InitGame): the client will be asked for a team again.
    pub fn map_loaded(&self) {
        let now = urt::now();
        let mut h = self.health.lock().unwrap();
        if now - h.map_event > 10.0 {
            h.map_event = now;
        }
    }

    async fn watchdog_loop(self: Arc<Self>, mut stop: StopRx) {
        let grace = 90.0; // map downloads can be slow
        while !stop.sleep_secs(5.0).await {
            let (snap_ts, reference) = {
                let h = self.health.lock().unwrap();
                (h.last_snap.as_ref().map(|s| s.ts), h.last_seen.max(h.client_started))
            };
            let Some(ts) = snap_ts else { continue };
            let now = urt::now();
            if now - ts > 15.0 {
                continue;
            }
            if now - reference > grace {
                tracing::warn!("[{}] spectator not on the server; restarting client", self.key);
                self.health.lock().unwrap().client_started = now;
                self.restart_client();
            }
        }
    }

    /// Told by the watch from the game log: which team the spectator client is on right now.
    pub fn set_own_team(&self, team: Option<String>) {
        self.health.lock().unwrap().own_team = team;
    }

    /// Keep the client a spectator. A client that just joined may be put on a team by the server (g_teamAutoJoin) or
    /// show the team menu; `team s` moves it to the spectators. Repeated every few seconds while the game log says it
    /// is not a spectator; without a game log it is sent once per client start.
    async fn ensure_spectator(&self) {
        if !self.cfg.force_spectator {
            return;
        }
        let now = urt::now();
        let known_wrong = {
            let mut h = self.health.lock().unwrap();
            let on_server = h.last_seen > h.client_started && now - h.last_seen < 20.0;
            if !on_server || now - h.client_started < 15.0 {
                return;
            }
            let (wrong, known) = match h.own_team.as_deref() {
                Some("spec") => (false, false),
                Some(_) => (true, true),
                None => (h.team_cmd_for != h.client_started, false),
            };
            if !wrong || now - h.last_team_cmd < 5.0 {
                return;
            }
            h.last_team_cmd = now;
            h.team_cmd_for = h.client_started;
            known
        };
        if known_wrong {
            tracing::info!("[{}] the spectator client is on a team - moving it to the spectators", self.key);
        }
        self.run_commands(&["team s".to_string()]).await;
    }

    pub fn restart_client(&self) {
        if let Some(n) = self.restart.lock().unwrap().get("client") {
            n.notify_one();
        }
    }

    // ---- client control ----

    /// Run console commands in the client (write the cfg, press the bound key).
    pub async fn run_commands(&self, cmds: &[String]) {
        let _g = self.cmd_lock.lock().await;
        let path = self.home_dir().join(&self.cfg.game_dir).join(CMD_CFG);
        let tmp = path.with_extension("cfg.tmp");
        if tokio::fs::write(&tmp, cmds.join("\n") + "\n").await.is_err() || tokio::fs::rename(&tmp, &path).await.is_err() {
            tracing::warn!("[{}] cannot write {}", self.key, path.display());
            return;
        }
        self.press_key(CMD_KEY).await;
    }

    /// Press one key in the game window (xdotool on Linux, SendKeys on Windows).
    async fn press_key(&self, key: &str) {
        let mut cmd = match self.platform {
            Platform::Linux => {
                let mut c = Command::new(&self.cfg.xdotool_binary);
                let key = if is_escape(key) { "Escape" } else { key };
                c.args(["mousemove", &(self.cfg.width / 2).to_string(), &(self.cfg.height / 2).to_string(), "key", key]);
                c.env("DISPLAY", self.display());
                c
            }
            Platform::Windows => {
                // SendKeys writes letters and digits as they are; named keys go in braces
                let k = if is_escape(key) { "{ESC}".to_string() } else if key.len() == 1 { key.to_string() } else { format!("{{{}}}", key) };
                let mut c = Command::new("powershell");
                c.args(["-NoProfile", "-NonInteractive", "-Command",
                        &format!("Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('{}')", k)]);
                c
            }
        };
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(5), async { cmd.spawn()?.wait().await }).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("[{}] could not press {}: {}", self.key, key, e),
            Err(_) => tracing::warn!("[{}] pressing {} timed out", self.key, key),
        }
    }

    /// Press the spectate keys (default `Escape 4`; Escape clears the join-team menu, 4 is its "spectate" entry).
    /// The keys other than Escape go once the client has joined the server (and settled) and again after every map load;
    /// if the game log shows the client still on a team they are repeated every 5 s (up to 5 times). Escape follows
    /// `escape_delay` seconds (default 60) after that join / map load, once, and is never repeated: pressed when no menu is
    /// up it opens the in-game menu.
    async fn press_spectate_key(&self) {
        let Some(keys) = spectate_keys(&self.cfg.spectate_key).filter(|k| !k.is_empty()) else { return };
        let delayed = self.cfg.escape_delay > 0.0;
        let now_keys: Vec<&String> = keys.iter().filter(|k| !(delayed && is_escape(k))).collect();
        if !now_keys.is_empty() {
            self.press_keys_now(&now_keys).await;
        }
        if delayed && keys.iter().any(|k| is_escape(k)) {
            self.press_delayed_escape().await;
        }
    }

    async fn press_keys_now(&self, keys: &[&String]) {
        let now = urt::now();
        let retry = {
            let mut h = self.health.lock().unwrap();
            let event = h.joined_at.max(h.map_event);
            let seen_after = h.last_seen > event && now - h.last_seen < 20.0;
            if event <= 0.0 || event < h.client_started || !seen_after || now - event < 8.0 {
                return; // not joined yet / map still loading
            }
            if h.key_for != event {
                h.key_for = event;
                h.key_presses = 0;
            } else if !(h.own_team.as_deref().is_some_and(|t| t != "spec") && h.key_presses < 5 && now - h.last_key >= 5.0) {
                return; // already pressed for this moment (and the log does not say it failed)
            }
            h.key_presses += 1;
            h.last_key = now;
            h.key_presses > 1
        };
        if retry {
            tracing::info!("[{}] still on a team - pressing {} again", self.key, self.cfg.spectate_key.trim());
        }
        let _g = self.cmd_lock.lock().await;
        let mut first = true;
        for k in keys.iter().filter(|k| !(retry && is_escape(k))) {
            if !first {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            first = false;
            self.press_key(k).await;
        }
    }

    /// Escape, `escape_delay` seconds after the join / map load it belongs to - once, and only while the client is
    /// still on the server and no newer map load has started.
    async fn press_delayed_escape(&self) {
        let now = urt::now();
        {
            let mut h = self.health.lock().unwrap();
            let event = h.joined_at.max(h.map_event);
            let seen_after = h.last_seen > event && now - h.last_seen < 20.0;
            if event <= 0.0 || event < h.client_started || !seen_after || now - event < self.cfg.escape_delay || h.esc_for == event {
                return;
            }
            h.esc_for = event;
        }
        let _g = self.cmd_lock.lock().await;
        self.press_key("Escape").await;
    }

    pub async fn follow(&self, r: &CamRef) {
        let arg = match r {
            CamRef::Slot(n) => n.to_string(),
            CamRef::Name(s) => format!("\"{}\"", s),
        };
        self.run_commands(&[format!("follow {}", arg)]).await;
    }

    pub fn ref_for(&self, name: &str) -> Option<CamRef> {
        match self.slot_lookup(name) {
            Some(s) => Some(CamRef::Slot(s)),
            None => safe_name_ref(name).map(CamRef::Name),
        }
    }

    // ---- auto director ----

    pub fn director_event(&self, ev: &Event) {
        if !self.cfg.director {
            return;
        }
        let want = match ev.kind {
            Kind::Kill => ev.data.killer_slot.map(CamRef::Slot),
            Kind::Flag => ev.data.slot.map(CamRef::Slot),
            _ => None,
        };
        if let Some(w) = want {
            self.cam.lock().unwrap().want = Some(w);
        }
    }

    async fn switch(&self, r: CamRef) {
        self.follow(&r).await;
        let label = match &r {
            CamRef::Slot(s) => self.name_lookup(*s).unwrap_or_else(|| s.to_string()),
            CamRef::Name(n) => n.clone(),
        };
        let mut c = self.cam.lock().unwrap();
        c.current = Some(r);
        c.last_switch = urt::now();
        c.want = None;
        c.label = label;
    }

    pub async fn pin(&self, r: CamRef, seconds: u32, label: &str) {
        {
            let mut c = self.cam.lock().unwrap();
            c.pin_until = urt::now() + seconds as f64;
            c.mode = "manual".into();
        }
        self.switch(r).await;
        self.cam.lock().unwrap().label = label.to_string();
    }

    pub fn unpin(&self) {
        let mut c = self.cam.lock().unwrap();
        c.pin_until = 0.0;
        c.mode = "auto".into();
    }

    fn top_ref(&self) -> Option<CamRef> {
        let name = {
            let h = self.health.lock().unwrap();
            let snap = h.last_snap.as_ref()?;
            snap.players.iter().filter(|p| p.name != self.name_clean).max_by_key(|p| p.score)?.name.clone()
        };
        self.ref_for(&name)
    }

    async fn director_loop(self: Arc<Self>, mut stop: StopRx) {
        while !stop.sleep_secs(1.0).await {
            self.press_spectate_key().await;
            self.ensure_spectator().await;
            let now = urt::now();
            let (pinned, want, last_switch, current) = {
                let mut c = self.cam.lock().unwrap();
                if now < c.pin_until {
                    (true, None, 0.0, None)
                } else {
                    c.mode = "auto".into();
                    (false, c.want.clone(), c.last_switch, c.current.clone())
                }
            };
            if pinned || !self.cfg.director {
                continue;
            }
            let want = match want {
                Some(w) => Some(w),
                None if now - last_switch >= self.cfg.idle_seconds => self.top_ref(),
                None => None,
            };
            if let Some(w) = want {
                if Some(&w) != current.as_ref() && now - last_switch >= self.cfg.hold_seconds {
                    self.switch(w).await;
                }
            }
        }
    }
}

/// Copies the delayed HLS window to a (local or remote) filesystem so a static web host can serve the stream.
async fn publisher_loop(fs: Arc<dyn Fs>, base: String, hls_dir: PathBuf, s: Arc<Streamer>, mut stop: StopRx) {
    let mut uploaded: Vec<String> = Vec::new();
    loop {
        let step: Result<()> = async {
            let Ok(text) = tokio::fs::read_to_string(hls_dir.join("live.m3u8")).await else { return Ok(()) };
            let (view, _) = delayed_view(&text, s.delay() as f64, WINDOW);
            let Some(view) = view else { return Ok(()) };
            let wanted: Vec<String> = view.lines().filter(|l| l.starts_with("seg_")).map(str::to_string).collect();
            for name in &wanted {
                if uploaded.contains(name) {
                    continue;
                }
                let Ok(data) = tokio::fs::read(hls_dir.join(name)).await else { continue };
                fs.write_bytes(&fs.join(&base, name), &data).await?;
                uploaded.push(name.clone());
            }
            fs.write_bytes(&fs.join(&base, "live.m3u8"), view.as_bytes()).await?;
            while uploaded.len() > wanted.len() + 10 {
                let old = uploaded.remove(0);
                if !wanted.contains(&old) {
                    fs.remove(&fs.join(&base, &old)).await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = step {
            tracing::warn!("publish to {} failed: {}", base, e);
            if stop.sleep_secs(5.0).await {
                return;
            }
        }
        if stop.sleep_secs(1.0).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)] // X display lock files exist only on Linux
    #[test]
    fn claim_display_skips_busy_numbers_and_cleans_stale_ones() {
        let (lock, _) = x_files(731);
        // a live process holds :731 -> we must move on
        std::fs::write(&lock, format!("{:>10}\n", std::process::id())).unwrap();
        let (n, notes) = claim_display(731);
        assert_eq!(n, 732, "{:?}", notes);
        assert!(notes[0].contains("in use"), "{:?}", notes);
        CLAIMED.lock().unwrap().remove(&n);
        // a dead pid is a stale lock -> removed and reused
        std::fs::write(&lock, "4194000\n").unwrap();
        let (n, notes) = claim_display(731);
        assert_eq!((n, notes.len()), (731, 0));
        assert!(!lock.exists());
        // the same number is not handed out twice inside one process
        assert_eq!(claim_display(731).0, 732);
        let mut c = CLAIMED.lock().unwrap();
        c.remove(&731);
        c.remove(&732);
        std::fs::remove_file(x_files(731).0).ok();
    }

    #[test]
    fn explains_leftover_x_lock_files() {
        assert!(explain_failure("(EE) Fatal server error:\n(EE) Could not create server lock file: /tmp/.X90-lock").unwrap().contains("another user"));
    }

    use super::*;

    fn cfg() -> SpectatorConfig {
        SpectatorConfig { client_binary: "/u/Quake3-UrT".into(), base_path: "/u".into(), ..Default::default() }
    }

    #[test]
    fn harmless_gpu_warnings_are_not_reported_as_failures() {
        assert!(explain_failure("libEGL warning: failed to open /dev/dri/card0: Permission denied").is_none());
        assert!(explain_failure("libEGL warning: DRI2: failed to authenticate\nMESA: error: ZINK: failed to choose pdev\nglx: failed to create dri3 screen").is_none());
        assert!(explain_failure("fopen: /srv/stream/logs/ffmpeg.log: Permission denied").unwrap().contains("cannot be accessed"));
        assert!(explain_failure("libEGL warning: failed to open /dev/dri/card0: Permission denied\nCannot open display :90.0").is_some());
    }

    #[test]
    fn names_are_made_safe_for_the_console() {
        assert_eq!(safe_name_ref(" Al|ice "), Some("Al|ice".into()));
        for bad in ["a\"b", "x;quit", "naïve", "", "  "] {
            assert_eq!(safe_name_ref(bad), None, "{}", bad);
        }
    }

    #[test]
    fn boot_cfg_binds_the_command_key_and_sanitises_the_name() {
        let mut c = cfg();
        c.client_name = "^3G\"T;x".into();
        let b = build_boot_cfg(&c);
        assert!(b.contains("seta name \"^3GTx\"") && b.contains("bind F9 \"exec gtv_cmd.cfg\""));
    }

    #[test]
    fn boot_cfg_hides_the_in_game_chat_unless_turned_off() {
        let mut c = cfg();
        let b = build_boot_cfg(&c);
        for line in ["seta cg_chatHeight 0", "seta cg_teamChatHeight 0", "seta cg_chatTime 0", "seta cg_teamChatTime 0", "seta con_notifytime 0"] {
            assert!(b.lines().any(|l| l == line), "{}", line);
        }
        c.hide_chat = false;
        assert!(!build_boot_cfg(&c).contains("cg_chatHeight"));
    }

    #[test]
    fn client_command_line() {
        let c = cfg();
        let a = build_client_argv(&c, "10.0.0.1", 27960, "/w/home", Platform::Linux);
        assert_eq!(a[0], "/u/Quake3-UrT");
        assert!(a.windows(2).any(|w| w == ["fs_basepath", "/u"]));
        assert!(a.windows(2).any(|w| w == ["fs_homepath", "/w/home"]));
        assert!(a.ends_with(&["+exec".into(), "gtv_boot.cfg".into(), "+connect".into(), "10.0.0.1:27960".into()]));
        assert!(!a.contains(&"vid_xpos".to_string()));
        let w = build_client_argv(&c, "h", 1, "C:\\w", Platform::Windows);
        assert!(w.contains(&"vid_xpos".to_string()));
        let mut c2 = cfg();
        c2.password = "pw".into();
        c2.extra_client_args = vec!["+set".into(), "x".into(), "1".into()];
        let a = build_client_argv(&c2, "h", 1, "/h", Platform::Linux);
        assert!(a.windows(2).any(|w| w == ["password", "pw"]) && a.contains(&"x".to_string()));
    }

    #[test]
    fn ffmpeg_command_uses_the_chosen_encoder() {
        use crate::gpu::Encoder;
        let c = cfg();
        let pos = |a: &[String], x: &str| a.iter().position(|v| v == x).unwrap();
        for (e, codec) in [(Encoder::Cpu, "libx264"), (Encoder::Nvenc, "h264_nvenc"), (Encoder::Vaapi, "h264_vaapi"), (Encoder::Qsv, "h264_qsv")] {
            let a = build_ffmpeg_argv(&c, ":90", "/w/hls", Platform::Linux, e).unwrap();
            assert!(a.windows(2).any(|w| w == ["-c:v", codec]), "{:?}", e);
            assert_eq!(a.iter().filter(|v| v.starts_with("h264_") || *v == "libx264").count(), 1);
            assert!(pos(&a, "-c:v") > pos(&a, "-map") && a.contains(&"-vf".to_string()));
        }
        // VAAPI: the device option must precede the first input, and frames are uploaded to the GPU
        let a = build_ffmpeg_argv(&c, ":90", "/w/hls", Platform::Linux, Encoder::Vaapi).unwrap();
        assert!(pos(&a, "-vaapi_device") < pos(&a, "-i"));
        assert!(a.iter().any(|v| v.ends_with("format=nv12,hwupload")));
        let w = build_ffmpeg_argv(&c, "", "C:/w/hls", Platform::Windows, Encoder::Amf).unwrap();
        assert!(w.windows(2).any(|p| p == ["-c:v", "h264_amf"]));
        // bitrate, keyframes and HLS output are unchanged by the encoder choice
        assert!(a.windows(2).any(|p| p == ["-b:v", "3000k"]) && a.iter().any(|v| v.starts_with("expr:gte(t,n_forced*")) && a.last().unwrap() == "/w/hls/live.m3u8");
    }

    #[test]
    fn ffmpeg_command_lines_per_platform_and_output() {
        let c = cfg();
        let a = build_ffmpeg_argv(&c, ":90", "/w/hls", Platform::Linux, crate::gpu::Encoder::Cpu).unwrap();
        assert!(a.windows(2).any(|w| w == ["-f", "x11grab"]) && a.windows(2).any(|w| w == ["-i", ":90.0"]));
        assert!(a.contains(&"libx264".to_string()) && a.last().unwrap() == "/w/hls/live.m3u8");
        assert!(a.iter().any(|x| x.starts_with("anullsrc")));
        let w = build_ffmpeg_argv(&c, "", "C:/w/hls", Platform::Windows, crate::gpu::Encoder::Cpu).unwrap();
        assert!(w.windows(2).any(|p| p == ["-f", "gdigrab"]) && w.windows(2).any(|p| p == ["-i", "desktop"]));
        let mut ct = cfg();
        ct.window_title = "ioq3".into();
        assert!(build_ffmpeg_argv(&ct, "", "h", Platform::Windows, crate::gpu::Encoder::Cpu).unwrap().contains(&"title=ioq3".to_string()));
        let mut pulse = cfg();
        pulse.audio = "pulse".into();
        assert!(build_ffmpeg_argv(&pulse, ":1", "h", Platform::Linux, crate::gpu::Encoder::Cpu).unwrap().windows(2).any(|p| p == ["-f", "pulse"]));
        let mut rt = cfg();
        rt.output = "rtmp".into();
        assert!(build_ffmpeg_argv(&rt, ":1", "h", Platform::Linux, crate::gpu::Encoder::Cpu).is_err()); // no url
        rt.rtmp_url = "rtmp://x/live/key".into();
        let a = build_ffmpeg_argv(&rt, ":1", "h", Platform::Linux, crate::gpu::Encoder::Cpu).unwrap();
        assert!(a.windows(2).any(|p| p == ["-f", "flv"]) && a.last().unwrap() == "rtmp://x/live/key");
        rt.output = "both".into();
        let a = build_ffmpeg_argv(&rt, ":1", "/h", Platform::Linux, crate::gpu::Encoder::Cpu).unwrap();
        assert!(a.last().unwrap().starts_with("[f=hls:") && a.last().unwrap().contains("|[f=flv]rtmp://x/live/key"));
        rt.output = "bogus".into();
        assert!(build_ffmpeg_argv(&rt, ":1", "h", Platform::Linux, crate::gpu::Encoder::Cpu).is_err());
    }

    #[test]
    fn failure_hints() {
        assert!(explain_failure("[x11grab @ 0x1] Cannot open display :90.0, error 1.").unwrap().contains("Xvfb"));
        assert!(explain_failure("Unknown input format: 'x11grab'").unwrap().contains("x11grab"));
        assert!(explain_failure("Unknown encoder 'libx264'").unwrap().contains("libx264"));
        assert!(explain_failure("Quake3-UrT.x86_64: error while loading shared libraries: libSDL-1.2.so.0").unwrap().contains("SDL 1.2"));
        assert!(explain_failure("something nobody has seen before").is_none());
    }

    #[test]
    fn rtmp_without_url_is_refused_up_front() {
        let mut c = cfg();
        c.output = "rtmp".into();
        assert!(Streamer::new("m", "h", 1, &c, 90, 0, None).is_err());
    }

    #[cfg(unix)]
    fn fake(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{}", body)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipeline_supervision_with_fake_binaries() {
        let t = std::env::temp_dir().join(format!("gtv-spec-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&t).unwrap();
        let mut c = cfg();
        c.base_path = t.to_string_lossy().into_owned();
        c.work_dir = t.join("work").to_string_lossy().into_owned();
        c.manage_display = false; // no Xvfb needed for this test
        c.display = ":99".into();
        c.client_binary = fake(&t, "client", "echo client-started >> \"$0.ran\"\nexec sleep 300\n");
        c.ffmpeg_binary = fake(&t, "ffmpeg", "echo 'Cannot open display :99.0, error 1.' >&2\nexit 1\n");
        c.xdotool_binary = fake(&t, "xdotool", "echo \"$@\" >> \"$0.calls\"\nexit 0\n");
        let s = Streamer::with_platform("m", "127.0.0.1", 27960, &c, 90, 0, None, Platform::Linux).unwrap();
        s.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // the client ran, and the dying ffmpeg left its output in its log
        assert!(std::fs::read_to_string(format!("{}.ran", c.client_binary)).unwrap().contains("client-started"));
        let log = std::fs::read_to_string(s.logs_dir().join("ffmpeg.log")).unwrap();
        assert!(log.contains("Cannot open display"));
        // boot config written
        assert!(std::fs::read_to_string(s.home_dir().join("q3ut4").join(BOOT_CFG)).unwrap().contains("bind F9"));
        // camera: pin writes the command file and presses the key
        s.pin(CamRef::Slot(7), 60, "Alice").await;
        assert!(std::fs::read_to_string(s.home_dir().join("q3ut4").join(CMD_CFG)).unwrap().contains("follow 7"));
        assert!(std::fs::read_to_string(format!("{}.calls", c.xdotool_binary)).unwrap().contains("key F9"));
        assert_eq!(s.camera(), ("Alice".to_string(), "manual".to_string()));
        s.unpin();
        assert_eq!(s.camera().1, "auto");
        // a restart request restarts only the client
        s.restart_client();
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let runs = std::fs::read_to_string(format!("{}.ran", c.client_binary)).unwrap().lines().count();
        assert!(runs >= 2, "client restarted, ran {} times", runs);
        s.stop().await;
        assert!(!s.is_running());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// With a real Xvfb: leftovers from another user on the first display number must not break the stream.
    /// (Run as a normal user while root-owned /tmp/.X<n>-lock exists to see the skip; as root it just starts.)
    #[cfg(unix)]
    #[tokio::test]
    async fn real_xvfb_comes_up_even_with_foreign_leftovers() {
        if std::process::Command::new("Xvfb").arg("-version").output().is_err() {
            return;
        }
        let t = std::env::temp_dir().join(format!("gtv-xv-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&t).unwrap();
        let first: u32 = std::env::var("GTV_TEST_DISPLAY").ok().and_then(|v| v.parse().ok()).unwrap_or(760);
        let mut c = cfg();
        c.base_path = t.to_string_lossy().into_owned();
        c.work_dir = t.join("work").to_string_lossy().into_owned();
        c.manage_display = true;
        c.client_binary = fake(&t, "client", "exec sleep 300\n");
        c.ffmpeg_binary = fake(&t, "ffmpeg", "exec sleep 300\n");
        c.xdotool_binary = fake(&t, "xdotool", "exit 0\n");
        let s = Streamer::with_platform("x", "127.0.0.1", 27960, &c, first, 0, None, Platform::Linux).unwrap();
        s.start().await.unwrap();
        let sock = format!("/tmp/.X11-unix/X{}", s.display_num());
        assert!(std::os::unix::net::UnixStream::connect(&sock).is_ok(), "Xvfb on {} is not accepting connections", s.display());
        eprintln!("xvfb is up on {} (first choice was :{})", s.display(), first);
        s.stop().await;
        let _ = std::fs::remove_dir_all(&t);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_client_is_kept_a_spectator() {
        let t = std::env::temp_dir().join(format!("gtv-team-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&t).unwrap();
        let mut c = cfg();
        c.work_dir = t.join("work").to_string_lossy().into_owned();
        c.manage_display = false;
        c.display = ":99".into();
        c.xdotool_binary = fake(&t, "xdotool", "exit 0\n");
        let s = Streamer::with_platform("m", "127.0.0.1", 27960, &c, 90, 0, None, Platform::Linux).unwrap();
        std::fs::create_dir_all(s.home_dir().join(&c.game_dir)).unwrap();
        let cmd_file = s.home_dir().join(&c.game_dir).join(CMD_CFG);
        let sent = || std::fs::read_to_string(&cmd_file).map(|x| x.contains("team s")).unwrap_or(false);
        let set = |started_ago: f64, seen_ago: Option<f64>, team: Option<&str>, last_cmd_ago: f64, sent_for_current: bool| {
            let now = urt::now();
            let mut h = s.health.lock().unwrap();
            h.client_started = now - started_ago;
            h.last_seen = seen_ago.map(|a| now - a).unwrap_or(0.0);
            h.own_team = team.map(String::from);
            h.last_team_cmd = now - last_cmd_ago;
            h.team_cmd_for = if sent_for_current { h.client_started } else { 0.0 };
        };
        // not on the server yet, or only just started loading: nothing is sent
        set(100.0, None, None, 100.0, false);
        s.ensure_spectator().await;
        assert!(!sent());
        set(5.0, Some(1.0), None, 100.0, false);
        s.ensure_spectator().await;
        assert!(!sent(), "give the client time to finish loading");
        // on the server, team unknown (no game log): `team s` once
        set(60.0, Some(1.0), None, 100.0, false);
        s.ensure_spectator().await;
        assert!(sent());
        std::fs::remove_file(&cmd_file).unwrap();
        set(60.0, Some(1.0), None, 100.0, true);
        s.ensure_spectator().await;
        assert!(!sent(), "only once per client start when the team is unknown");
        // the game log says it is already a spectator: leave it alone
        set(60.0, Some(1.0), Some("spec"), 100.0, true);
        s.ensure_spectator().await;
        assert!(!sent());
        // the server put it on a team: move it out, but not more often than every few seconds
        set(60.0, Some(1.0), Some("red"), 100.0, true);
        s.ensure_spectator().await;
        assert!(sent());
        std::fs::remove_file(&cmd_file).unwrap();
        set(60.0, Some(1.0), Some("red"), 2.0, true);
        s.ensure_spectator().await;
        assert!(!sent());
        // switched off in the settings
        let mut off = c.clone();
        off.force_spectator = false;
        let s2 = Streamer::with_platform("m2", "127.0.0.1", 27960, &off, 91, 0, None, Platform::Linux).unwrap();
        { let now = urt::now(); let mut h = s2.health.lock().unwrap(); h.client_started = now - 60.0; h.last_seen = now - 1.0; h.own_team = Some("red".into()); }
        std::fs::create_dir_all(s2.home_dir().join(&c.game_dir)).unwrap();
        s2.ensure_spectator().await;
        assert!(!s2.home_dir().join(&c.game_dir).join(CMD_CFG).exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_spectate_key_is_pressed_after_joining_and_after_every_map_load() {
        let t = std::env::temp_dir().join(format!("gtv-key-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&t).unwrap();
        let mut c = cfg();
        c.work_dir = t.join("work").to_string_lossy().into_owned();
        c.manage_display = false;
        c.display = ":99".into();
        c.xdotool_binary = fake(&t, "xdotool", "echo \"$@\" >> \"$0.calls\"\nexit 0\n");
        c.escape_delay = 0.0; // this test covers the "Escape first, together with the others" mode
        let calls = format!("{}.calls", c.xdotool_binary);
        let presses = || std::fs::read_to_string(&calls).map(|x| x.lines().filter(|l| l.ends_with(" key 4")).count()).unwrap_or(0);
        let escapes = || std::fs::read_to_string(&calls).map(|x| x.lines().filter(|l| l.ends_with(" key Escape")).count()).unwrap_or(0);
        let s = Streamer::with_platform("m", "127.0.0.1", 27960, &c, 90, 0, None, Platform::Linux).unwrap();
        let now = urt::now();
        let set = |f: &dyn Fn(&mut Health)| f(&mut s.health.lock().unwrap());
        set(&|h| { h.client_started = now - 100.0; });
        // not on the server yet
        s.press_spectate_key().await;
        assert_eq!(presses(), 0);
        // just joined: wait for the game to settle first
        set(&|h| { h.joined_at = now - 3.0; h.last_seen = now - 1.0; });
        s.press_spectate_key().await;
        assert_eq!(presses(), 0);
        // joined a while ago: press once, and not again for the same join
        set(&|h| { h.joined_at = now - 12.0; h.last_seen = now - 1.0; });
        s.press_spectate_key().await;
        s.press_spectate_key().await;
        assert_eq!(presses(), 1, "once per join");
        assert_eq!(escapes(), 1, "Escape clears the team menu first, then 4");
        let order: Vec<String> = std::fs::read_to_string(&calls).unwrap().lines().map(|l| l.rsplit(' ').next().unwrap().to_string()).collect();
        assert_eq!(order, vec!["Escape", "4"]);
        // a map load: the client drops out and comes back, then the key is pressed again
        set(&|h| { h.map_event = now - 10.0; h.last_seen = now - 11.0; });
        s.press_spectate_key().await;
        assert_eq!(presses(), 1, "map is still loading (client not seen since)");
        set(&|h| { h.last_seen = now - 1.0; });
        s.press_spectate_key().await;
        assert_eq!(presses(), 2, "pressed again after the map load");
        assert_eq!(escapes(), 2, "and the menu is cleared again");
        // the game log says it is still on a team: retry, but not faster than every 5 s and not forever
        set(&|h| { h.own_team = Some("red".into()); h.last_key = now - 1.0; });
        s.press_spectate_key().await;
        assert_eq!(presses(), 2);
        for _ in 0..8 {
            set(&|h| { h.last_key = now - 10.0; });
            s.press_spectate_key().await;
        }
        assert_eq!(presses(), 6, "at most 5 presses for the map load (1 earlier press was for the join)");
        assert_eq!(escapes(), 2, "retries never press Escape (it would open the in-game menu if no menu is up)");
        // on the spectators: nothing more
        set(&|h| { h.own_team = Some("spec".into()); h.last_key = now - 10.0; });
        s.press_spectate_key().await;
        assert_eq!(presses(), 6);
        // map loads are noticed from the status too, and double reports are merged
        set(&|h| { h.map_event = 0.0; h.last_map.clear(); });
        let snap = |m: &str| Snapshot { ts: now, online: true, mapname: m.into(), players: vec![crate::urt::Player { name: s.name_clean().into(), score: 0, ping: 1 }], ..Default::default() };
        s.observe(&snap("ut4_abbey"));
        assert_eq!(s.health.lock().unwrap().map_event, 0.0, "first map seen is not a reload");
        s.observe(&snap("ut4_casa"));
        let m1 = s.health.lock().unwrap().map_event;
        assert!(m1 > 0.0);
        s.map_loaded();
        assert_eq!(s.health.lock().unwrap().map_event, m1, "the log and the status report one load, not two");
        // a server without a join menu: just the spectate key
        let mut plain = c.clone();
        plain.spectate_key = "4".into();
        let s3 = Streamer::with_platform("m3", "127.0.0.1", 27960, &plain, 92, 0, None, Platform::Linux).unwrap();
        { let mut h = s3.health.lock().unwrap(); h.client_started = now - 100.0; h.joined_at = now - 20.0; h.last_seen = now - 1.0; }
        s3.press_spectate_key().await;
        assert_eq!((presses(), escapes()), (7, 2));
        // switched off
        let mut off = c.clone();
        off.spectate_key = String::new();
        let s2 = Streamer::with_platform("m2", "127.0.0.1", 27960, &off, 91, 0, None, Platform::Linux).unwrap();
        { let mut h = s2.health.lock().unwrap(); h.client_started = now - 100.0; h.joined_at = now - 20.0; h.last_seen = now - 1.0; }
        s2.press_spectate_key().await;
        assert_eq!(presses(), 7);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn escape_is_sent_a_minute_after_joining_and_after_each_map_load() {
        let t = std::env::temp_dir().join(format!("gtv-esc-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&t).unwrap();
        let mut c = cfg();
        c.work_dir = t.join("work").to_string_lossy().into_owned();
        c.manage_display = false;
        c.display = ":99".into();
        c.xdotool_binary = fake(&t, "xdotool", "echo \"$@\" >> \"$0.calls\"\nexit 0\n");
        assert_eq!((c.spectate_key.as_str(), c.escape_delay), ("Escape 4", 60.0), "defaults");
        let calls = format!("{}.calls", c.xdotool_binary);
        let count = |k: &str| std::fs::read_to_string(&calls).map(|x| x.lines().filter(|l| l.ends_with(&format!(" key {}", k))).count()).unwrap_or(0);
        let s = Streamer::with_platform("m", "127.0.0.1", 27960, &c, 90, 0, None, Platform::Linux).unwrap();
        let now = urt::now();
        let set = |f: &dyn Fn(&mut Health)| f(&mut s.health.lock().unwrap());
        // joined 12 s ago: the spectate key goes, Escape waits
        set(&|h| { h.client_started = now - 200.0; h.joined_at = now - 12.0; h.last_seen = now - 1.0; });
        s.press_spectate_key().await;
        assert_eq!((count("4"), count("Escape")), (1, 0));
        // time passes (the same join moment, only older): at 45 s Escape still waits
        set(&|h| { let old = h.joined_at; h.joined_at = now - 45.0; if h.key_for == old { h.key_for = h.joined_at; } });
        s.press_spectate_key().await;
        assert_eq!((count("4"), count("Escape")), (1, 0));
        // 61 s after joining: Escape, once
        set(&|h| { let old = h.joined_at; h.joined_at = now - 61.0; if h.key_for == old { h.key_for = h.joined_at; } });
        s.press_spectate_key().await;
        s.press_spectate_key().await;
        assert_eq!((count("4"), count("Escape")), (1, 1));
        // a map load: `4` at once (after it settles), Escape a minute later - not before
        set(&|h| { h.map_event = now - 10.0; h.last_seen = now - 1.0; });
        s.press_spectate_key().await;
        assert_eq!((count("4"), count("Escape")), (2, 1));
        set(&|h| { let old = h.map_event; h.map_event = now - 59.0; if h.key_for == old { h.key_for = h.map_event; } });
        s.press_spectate_key().await;
        assert_eq!(count("Escape"), 1);
        // a client that has left the server never gets the Escape
        set(&|h| { let old = h.map_event; h.map_event = now - 70.0; if h.key_for == old { h.key_for = h.map_event; } h.joined_at = now - 300.0; h.last_seen = now - 40.0; });
        s.press_spectate_key().await;
        assert_eq!(count("Escape"), 1, "not seen on the server for 40 s");
        set(&|h| { h.last_seen = now - 1.0; });
        s.press_spectate_key().await;
        assert_eq!(count("Escape"), 2);
        // a newer map load restarts the minute; retries on a team never press Escape
        set(&|h| { h.map_event = now - 20.0; h.own_team = Some("red".into()); h.last_key = now - 10.0; });
        s.press_spectate_key().await;
        assert_eq!(count("Escape"), 2);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn spectate_key_settings_are_parsed_strictly() {
        assert_eq!(spectate_keys("Escape 4"), Some(vec!["Escape".to_string(), "4".to_string()]));
        assert_eq!(spectate_keys("  F5 "), Some(vec!["F5".to_string()]));
        assert_eq!(spectate_keys(""), Some(vec![]));
        assert!(spectate_keys("a b c d e").is_none());
        assert!(spectate_keys("4; quit").is_none());
        assert!(spectate_keys("../x").is_none());
        let mut c = crate::config::Config::default();
        let mut sc = crate::config::ServerConfig { host: "h".into(), ..Default::default() };
        sc.spectator = Some(SpectatorConfig { spectate_key: "Escape 4; quit".into(), ..Default::default() });
        c.servers.insert("a".into(), sc);
        assert!(c.problems().join("\n").contains("spectate_key"));
    }

    #[test]
    fn director_prefers_the_latest_killer_and_ignores_other_events() {
        let s = Streamer::with_platform("m", "h", 1, &cfg(), 90, 0, None, Platform::Linux).unwrap();
        let mut e = Event::new(1.0, Kind::Kill, String::new());
        e.data.killer_slot = Some(4);
        s.director_event(&e);
        assert_eq!(s.cam.lock().unwrap().want, Some(CamRef::Slot(4)));
        s.director_event(&Event::new(2.0, Kind::Join, String::new()));
        assert_eq!(s.cam.lock().unwrap().want, Some(CamRef::Slot(4)));
        let mut f = Event::new(3.0, Kind::Flag, String::new());
        f.data.slot = Some(9);
        s.director_event(&f);
        assert_eq!(s.cam.lock().unwrap().want, Some(CamRef::Slot(9)));
    }

    #[test]
    fn camera_refs_prefer_slots_then_safe_names() {
        let s = Streamer::with_platform("m", "h", 1, &cfg(), 90, 0, None, Platform::Linux).unwrap();
        assert_eq!(s.ref_for("Bob"), Some(CamRef::Name("Bob".into())));
        assert_eq!(s.ref_for("bad\"name"), None);
        s.set_lookups(Box::new(|n| if n == "Bob" { Some(3) } else { None }), Box::new(|_| None));
        assert_eq!(s.ref_for("Bob"), Some(CamRef::Slot(3)));
    }
}
