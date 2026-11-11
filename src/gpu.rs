// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Video encoding on the CPU or the GPU, and the detection run that proves a GPU encoder really works.
//!
//! Only the *encoding* (ffmpeg) can move to the GPU. The game client itself renders inside Xvfb, a virtual
//! screen without GPU access, so it is software-rendered either way - at 720p that is fine.
//! Detection does the real thing: it looks for hardware, then runs a short test encode with every
//! encoder this ffmpeg has, and only recommends one that produced video.

use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::process::Command;

use crate::config::SpectatorConfig;
use crate::spectator::Platform;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Encoder {
    Cpu,
    Nvenc,
    Vaapi,
    Qsv,
    Amf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Fixed(Encoder),
    /// Use the best GPU encoder that passes the test, otherwise the CPU.
    Auto,
}

pub fn parse_choice(s: &str) -> Option<Choice> {
    Some(match s.trim().to_lowercase().as_str() {
        "" | "cpu" | "x264" | "software" => Choice::Fixed(Encoder::Cpu),
        "auto" | "gpu" => Choice::Auto,
        "nvenc" | "nvidia" => Choice::Fixed(Encoder::Nvenc),
        "vaapi" => Choice::Fixed(Encoder::Vaapi),
        "qsv" | "quicksync" => Choice::Fixed(Encoder::Qsv),
        "amf" | "amd" => Choice::Fixed(Encoder::Amf),
        _ => return None,
    })
}

impl Encoder {
    pub fn codec(self) -> &'static str {
        match self {
            Encoder::Cpu => "libx264",
            Encoder::Nvenc => "h264_nvenc",
            Encoder::Vaapi => "h264_vaapi",
            Encoder::Qsv => "h264_qsv",
            Encoder::Amf => "h264_amf",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoder::Cpu => "CPU (x264)",
            Encoder::Nvenc => "NVIDIA GPU (NVENC)",
            Encoder::Vaapi => "Intel/AMD GPU (VAAPI)",
            Encoder::Qsv => "Intel GPU (Quick Sync)",
            Encoder::Amf => "AMD GPU (AMF)",
        }
    }

    pub fn setting(self) -> &'static str {
        match self {
            Encoder::Cpu => "cpu",
            Encoder::Nvenc => "nvenc",
            Encoder::Vaapi => "vaapi",
            Encoder::Qsv => "qsv",
            Encoder::Amf => "amf",
        }
    }

    pub fn supported_on(self, p: Platform) -> bool {
        match self {
            Encoder::Cpu | Encoder::Nvenc | Encoder::Qsv => true,
            Encoder::Vaapi => p == Platform::Linux,
            Encoder::Amf => p == Platform::Windows,
        }
    }
}

const GPU_ORDER: [Encoder; 4] = [Encoder::Nvenc, Encoder::Qsv, Encoder::Vaapi, Encoder::Amf];

pub struct EncArgs {
    /// ffmpeg options that must come before the first `-i`.
    pub pre_input: Vec<String>,
    pub vf: String,
    /// `-c:v ...` and the encoder-specific options.
    pub codec: Vec<String>,
}

pub fn encoder_args(e: Encoder, cfg: &SpectatorConfig) -> EncArgs {
    let s = |x: &str| x.to_string();
    let even = "scale=trunc(iw/2)*2:trunc(ih/2)*2";
    match e {
        Encoder::Cpu => EncArgs {
            pre_input: vec![], vf: s(even),
            codec: vec![s("-c:v"), s("libx264"), s("-preset"), s(&cfg.x264_preset), s("-tune"), s("zerolatency"), s("-pix_fmt"), s("yuv420p")],
        },
        Encoder::Nvenc => EncArgs {
            pre_input: vec![], vf: s(even),
            codec: vec![s("-c:v"), s("h264_nvenc"), s("-preset"), s("p4"), s("-tune"), s("ll"), s("-rc"), s("cbr"), s("-pix_fmt"), s("yuv420p")],
        },
        Encoder::Vaapi => EncArgs {
            pre_input: vec![s("-vaapi_device"), s(&cfg.vaapi_device)],
            vf: format!("{},format=nv12,hwupload", even),
            codec: vec![s("-c:v"), s("h264_vaapi")],
        },
        Encoder::Qsv => EncArgs {
            pre_input: vec![], vf: s(even),
            codec: vec![s("-c:v"), s("h264_qsv"), s("-preset"), s("veryfast"), s("-pix_fmt"), s("nv12")],
        },
        Encoder::Amf => EncArgs {
            pre_input: vec![], vf: s(even),
            codec: vec![s("-c:v"), s("h264_amf"), s("-usage"), s("lowlatency"), s("-quality"), s("speed"), s("-pix_fmt"), s("yuv420p")],
        },
    }
}

// ------------------------------------------------------------------------------------------ detection

#[derive(Clone, Debug, Serialize)]
pub struct EncoderTest {
    pub encoder: Encoder,
    pub label: String,
    /// The ffmpeg build contains this encoder at all.
    pub available: bool,
    /// A real test encode produced video.
    pub ok: bool,
    pub detail: String,
    pub fix: String,
    /// Roughly how many times faster than real time the test encode ran (higher is better).
    pub speed: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct GpuReport {
    pub ffmpeg_found: bool,
    pub ffmpeg: String,
    pub hardware: Vec<String>,
    pub notes: Vec<String>,
    pub tests: Vec<EncoderTest>,
    pub recommended: Encoder,
    pub rendering: String,
}

async fn run_cmd(argv: &[String], secs: u64) -> Option<(i32, String)> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(secs), cmd.output()).await.ok()?.ok()?;
    Some((out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

fn sv(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| x.to_string()).collect()
}

/// Plain-words reason (and fix) for a failed test encode.
pub fn explain_encoder_failure(e: Encoder, text: &str) -> (String, String) {
    let low = text.to_lowercase();
    // the first message is the cause; later lines are consequences ("Nothing was written ...")
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| match l.strip_prefix('[').and_then(|r| r.split_once(']')) { Some((_, rest)) => rest.trim(), None => l })
        .unwrap_or("no output").chars().take(160).collect::<String>();
    let has = |n: &str| low.contains(n);
    let (why, fix) = match e {
        Encoder::Nvenc if has("libcuda") || has("cuda") && has("load") || has("no capable devices") || has("driver does not support") || has("cannot load libnvidia") =>
            ("no usable NVIDIA GPU/driver", "install the NVIDIA driver (the command `nvidia-smi` must work), or choose CPU"),
        Encoder::Nvenc if has("unknown encoder") => ("this ffmpeg has no NVENC", "use an ffmpeg build with NVENC (ffmpeg -encoders | grep nvenc)"),
        Encoder::Nvenc => ("NVENC could not start", "check `nvidia-smi`; some GPUs (and virtual machines) have no NVENC"),
        Encoder::Vaapi if has("permission denied") =>
            ("no permission to use the GPU render device", "add the bot's user to the render group: sudo usermod -aG render,video $USER (then restart the bot / log in again)"),
        Encoder::Vaapi if has("no such file") || has("no va display") || has("failed to initialise vaapi") || has("device creation failed") =>
            ("no GPU render device (/dev/dri/renderD128)", "this machine has no usable Intel/AMD GPU, or set vaapi_device to the right /dev/dri/renderD* node"),
        Encoder::Vaapi if has("vainitialize") || has("va_openDriver") || has("libva") =>
            ("the VAAPI driver is missing", "install it: Intel: sudo apt-get install intel-media-va-driver (or i965-va-driver); AMD: mesa-va-drivers. `vainfo` should then list H264 entrypoints"),
        Encoder::Vaapi if has("unknown encoder") => ("this ffmpeg has no VAAPI", "use a full ffmpeg build"),
        Encoder::Vaapi => ("VAAPI could not start", "run `vainfo` to see what the GPU supports"),
        Encoder::Qsv if has("unknown encoder") => ("this ffmpeg has no Quick Sync", "use an ffmpeg build with QSV"),
        Encoder::Qsv => ("Quick Sync could not start", "needs an Intel GPU plus the media driver / oneVPL runtime (Linux: intel-media-va-driver-non-free, libvpl2)"),
        Encoder::Amf if has("unknown encoder") => ("this ffmpeg has no AMF", "use a full Windows ffmpeg build (gyan.dev 'full')"),
        Encoder::Amf => ("AMF could not start", "needs an AMD GPU and a current AMD Adrenalin driver"),
        Encoder::Cpu if has("unknown encoder") => ("this ffmpeg has no libx264", "install the full ffmpeg package"),
        Encoder::Cpu => ("the x264 encoder failed", "check the ffmpeg install"),
    };
    (format!("{} ({})", why, first), fix.to_string())
}

async fn list_encoders(ffmpeg: &str) -> Option<String> {
    run_cmd(&[ffmpeg.to_string(), "-hide_banner".into(), "-encoders".into()], 15).await.map(|(_, t)| t)
}

async fn hardware(platform: Platform, cfg: &SpectatorConfig, notes: &mut Vec<String>) -> Vec<String> {
    let mut hw = Vec::new();
    if platform == Platform::Windows {
        let ps = sv(&["powershell", "-NoProfile", "-Command", "(Get-CimInstance Win32_VideoController).Name"]);
        if let Some((0, out)) = run_cmd(&ps, 15).await {
            hw.extend(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| format!("graphics adapter: {}", l)));
        }
        return hw;
    }
    if let Some((0, out)) = run_cmd(&sv(&["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"]), 8).await {
        hw.extend(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| format!("NVIDIA: {}", l)));
    }
    if let Some((0, out)) = run_cmd(&sv(&["lspci"]), 8).await {
        for l in out.lines().filter(|l| { let low = l.to_lowercase(); low.contains("vga") || low.contains("3d controller") || low.contains("display controller") }) {
            hw.push(format!("PCI: {}", l.splitn(2, ' ').nth(1).unwrap_or(l).trim()));
        }
    }
    let nodes: Vec<String> = std::fs::read_dir("/dev/dri").map(|d| d.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    if nodes.is_empty() {
        notes.push("no /dev/dri devices: this machine has no GPU the kernel can use for VAAPI/Quick Sync (NVIDIA's NVENC does not need them)".into());
    }
    for n in nodes.iter().filter(|n| n.starts_with("renderD")) {
        let path = format!("/dev/dri/{}", n);
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(_) => hw.push(format!("render device {} (usable by this user)", path)),
            Err(e) => notes.push(format!("{} exists but this user cannot open it ({}): sudo usermod -aG render,video $USER, then restart the bot", path, e)),
        }
    }
    if !std::path::Path::new(&cfg.vaapi_device).exists() && nodes.iter().any(|n| n.starts_with("renderD")) {
        notes.push(format!("vaapi_device is {} but that device does not exist - use one of the renderD* nodes above", cfg.vaapi_device));
    }
    hw
}

pub async fn test_encoder(e: Encoder, cfg: &SpectatorConfig) -> EncoderTest {
    let mut t = EncoderTest { encoder: e, label: e.label().into(), available: true, ok: false, detail: String::new(), fix: String::new(), speed: None };
    let args = encoder_args(e, cfg);
    let clip = 2.0;
    let mut a = vec![cfg.ffmpeg_binary.clone(), "-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostdin".into()];
    a.extend(args.pre_input);
    a.extend(sv(&["-f", "lavfi", "-i"]));
    a.push(format!("testsrc2=size={}x{}:rate={}", cfg.width.max(160), cfg.height.max(120), cfg.fps.max(1)));
    a.extend(["-t".to_string(), clip.to_string(), "-vf".into(), args.vf]);
    a.extend(args.codec);
    a.extend(["-b:v".to_string(), format!("{}k", cfg.video_kbps), "-f".into(), "null".into(), "-".into()]);
    let started = Instant::now();
    match run_cmd(&a, 30).await {
        Some((0, _)) => {
            let secs = started.elapsed().as_secs_f64().max(0.01);
            t.ok = true;
            t.speed = Some((clip / secs * 10.0).round() / 10.0);
            t.detail = format!("encoded a {}s test clip in {:.1}s (about {:.1}x real time)", clip, secs, clip / secs);
        }
        Some((_, text)) => {
            if text.to_lowercase().contains("unknown encoder") {
                t.available = false;
            }
            let (d, f) = explain_encoder_failure(e, &text);
            t.detail = d;
            t.fix = f;
        }
        None => {
            t.detail = "the test did not finish in 30 seconds".into();
            t.fix = "the encoder hangs - the driver may be broken".into();
        }
    }
    t
}

/// Look for GPUs and run a short test encode with each encoder this ffmpeg has.
pub async fn detect(cfg: &SpectatorConfig, platform: Platform) -> GpuReport {
    let mut notes = Vec::new();
    let hardware = hardware(platform, cfg, &mut notes).await;
    let rendering = if platform == Platform::Linux && cfg.software_gl {
        "The game itself is drawn in software inside Xvfb (a virtual screen has no GPU access) - that is normal and fine at 720p. Only the video ENCODING can move to the GPU. To render the game on the GPU too you would need a real desktop display: manage_display=false, display=\":0\", software_gl=false."
    } else if platform == Platform::Windows {
        "On Windows the game is drawn by your normal desktop (already GPU-accelerated); this test covers the video encoding."
    } else {
        "software_gl is off: the game uses the GPU only if the X display it runs on has one."
    }.to_string();
    let Some(listing) = list_encoders(&cfg.ffmpeg_binary).await else {
        return GpuReport { ffmpeg_found: false, ffmpeg: cfg.ffmpeg_binary.clone(), hardware, notes, tests: vec![], recommended: Encoder::Cpu, rendering };
    };
    let mut tests = Vec::new();
    for e in std::iter::once(Encoder::Cpu).chain(GPU_ORDER) {
        if !e.supported_on(platform) {
            continue;
        }
        if !listing.contains(e.codec()) {
            tests.push(EncoderTest { encoder: e, label: e.label().into(), available: false, ok: false, detail: format!("this ffmpeg was built without {}", e.codec()), fix: String::new(), speed: None });
            continue;
        }
        tests.push(test_encoder(e, cfg).await);
    }
    let recommended = GPU_ORDER.iter().copied().find(|g| tests.iter().any(|t| t.encoder == *g && t.ok)).unwrap_or(Encoder::Cpu);
    GpuReport { ffmpeg_found: true, ffmpeg: cfg.ffmpeg_binary.clone(), hardware, notes, tests, recommended, rendering }
}

pub fn render(r: &GpuReport) -> String {
    let mut o = String::from("Video encoder detection\n");
    if !r.ffmpeg_found {
        o.push_str(&format!("  ffmpeg           PROBLEM - '{}' could not be run (install ffmpeg or fix ffmpeg_binary)\n", r.ffmpeg));
        return o;
    }
    if r.hardware.is_empty() {
        o.push_str("  hardware         .. - no GPU found\n");
    }
    for h in &r.hardware {
        o.push_str(&format!("  hardware         .. - {}\n", h));
    }
    for n in &r.notes {
        o.push_str(&format!("  note             .. - {}\n", n));
    }
    for t in &r.tests {
        let tag = if t.ok { "ok" } else if t.available { "FAILED" } else { "n/a" };
        o.push_str(&format!("  {:<16} {} - {}\n", t.encoder.setting(), tag, if t.ok { t.detail.clone() } else { t.detail.clone() }));
        if !t.fix.is_empty() {
            o.push_str(&format!("  {:<16}   -> {}\n", "", t.fix));
        }
    }
    o.push_str(&format!("  recommended      -> {} (set encoder to \"{}\")\n  rendering        .. - {}\n", r.recommended.label(), r.recommended.setting(), r.rendering));
    o
}

// ------------------------------------------------------------------------------------------ choosing at start

static AUTO_CACHE: Mutex<Vec<(String, Encoder)>> = Mutex::new(Vec::new());

/// The encoder to use for this configuration. `auto` runs detection once per program run and remembers it.
pub async fn resolve(cfg: &SpectatorConfig, platform: Platform) -> (Encoder, String) {
    match parse_choice(&cfg.encoder) {
        None => (Encoder::Cpu, format!("unknown encoder '{}' - using the CPU", cfg.encoder)),
        Some(Choice::Fixed(e)) if !e.supported_on(platform) => (Encoder::Cpu, format!("{} is not available on this OS - using the CPU", e.label())),
        Some(Choice::Fixed(e)) => (e, format!("video encoder: {}", e.label())),
        Some(Choice::Auto) => {
            let key = format!("{}|{}|{}x{}", cfg.ffmpeg_binary, cfg.vaapi_device, cfg.width, cfg.height);
            let cached = AUTO_CACHE.lock().unwrap().iter().find(|(k, _)| *k == key).map(|(_, e)| *e);
            let e = match cached {
                Some(e) => e,
                None => {
                    let e = detect(cfg, platform).await.recommended;
                    AUTO_CACHE.lock().unwrap().push((key, e));
                    e
                }
            };
            (e, format!("video encoder (auto-detected): {}", e.label()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_parse_and_platform_rules() {
        assert_eq!(parse_choice(""), Some(Choice::Fixed(Encoder::Cpu)));
        assert_eq!(parse_choice("GPU"), Some(Choice::Auto));
        assert_eq!(parse_choice("nvidia"), Some(Choice::Fixed(Encoder::Nvenc)));
        assert_eq!(parse_choice("bogus"), None);
        assert!(Encoder::Vaapi.supported_on(Platform::Linux) && !Encoder::Vaapi.supported_on(Platform::Windows));
        assert!(Encoder::Amf.supported_on(Platform::Windows) && !Encoder::Amf.supported_on(Platform::Linux));
    }

    #[test]
    fn encoder_arguments_per_hardware() {
        let c = SpectatorConfig::default();
        let cpu = encoder_args(Encoder::Cpu, &c);
        assert!(cpu.codec.contains(&"libx264".to_string()) && cpu.pre_input.is_empty());
        let nv = encoder_args(Encoder::Nvenc, &c);
        assert!(nv.codec.windows(2).any(|w| w == ["-c:v", "h264_nvenc"]));
        let va = encoder_args(Encoder::Vaapi, &c);
        assert_eq!(va.pre_input, vec!["-vaapi_device", "/dev/dri/renderD128"]);
        assert!(va.vf.ends_with("format=nv12,hwupload"));
        assert!(encoder_args(Encoder::Amf, &c).codec.contains(&"h264_amf".to_string()));
    }

    #[test]
    fn failures_are_explained_in_words() {
        let (d, f) = explain_encoder_failure(Encoder::Nvenc, "Cannot load libcuda.so.1\nError initializing output stream");
        assert!(d.contains("no usable NVIDIA") && f.contains("nvidia-smi"));
        let (_, f) = explain_encoder_failure(Encoder::Vaapi, "Failed to open /dev/dri/renderD128: Permission denied");
        assert!(f.contains("usermod -aG render"));
        let (d, _) = explain_encoder_failure(Encoder::Qsv, "[h264_qsv @ 0x55] Error creating a MFX session: -9.\n[out#0] Nothing was written");
        assert!(d.contains("Quick Sync") && d.contains("Error creating a MFX session") && !d.contains("Nothing was written"), "{}", d);
        let (d, _) = explain_encoder_failure(Encoder::Vaapi, "[AVHWDeviceContext @ 0x1] No VA display found for device /dev/dri/renderD128.");
        assert!(d.contains("no GPU render device"));
    }

    /// A stand-in ffmpeg: lists encoders, "encodes" with libx264 and h264_vaapi, fails NVENC like a machine without a driver.
    #[cfg(unix)]
    fn fake_ffmpeg(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("ffmpeg");
        std::fs::write(&p, r#"#!/bin/sh
case "$*" in
  *-encoders*) printf ' V....D libx264 x\n V....D h264_nvenc x\n V....D h264_vaapi x\n V..... h264_qsv x\n'; exit 0;;
  *h264_nvenc*) echo "Cannot load libcuda.so.1" >&2; exit 1;;
  *h264_qsv*) echo "Error creating a MFX session" >&2; exit 1;;
  *h264_vaapi*|*libx264*) exit 0;;
esac
exit 1
"#).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detection_tests_every_encoder_and_recommends_one_that_works() {
        let dir = std::env::temp_dir().join(format!("gtv-gpu-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = SpectatorConfig::default();
        c.ffmpeg_binary = fake_ffmpeg(&dir);
        let r = detect(&c, Platform::Linux).await;
        assert!(r.ffmpeg_found);
        let by = |e: Encoder| r.tests.iter().find(|t| t.encoder == e).unwrap();
        assert!(by(Encoder::Cpu).ok && by(Encoder::Vaapi).ok);
        assert!(!by(Encoder::Nvenc).ok && by(Encoder::Nvenc).available && by(Encoder::Nvenc).detail.contains("no usable NVIDIA"));
        assert!(!by(Encoder::Qsv).ok);
        assert!(r.tests.iter().all(|t| t.encoder != Encoder::Amf)); // Windows only
        assert_eq!(r.recommended, Encoder::Vaapi); // nvenc and qsv failed, vaapi passed
        let text = render(&r);
        assert!(text.contains("FAILED") && text.contains("recommended      -> Intel/AMD GPU (VAAPI)"), "{}", text);
        // `auto` uses the same answer, and is remembered
        c.encoder = "auto".into();
        assert_eq!(resolve(&c, Platform::Linux).await.0, Encoder::Vaapi);
        c.ffmpeg_binary = "/no/such/ffmpeg".into();
        c.encoder = "cpu".into();
        assert_eq!(resolve(&c, Platform::Linux).await.0, Encoder::Cpu);
        c.encoder = "amf".into();
        assert_eq!(resolve(&c, Platform::Linux).await.0, Encoder::Cpu); // not on Linux
        let missing = detect(&c, Platform::Linux).await;
        assert!(!missing.ffmpeg_found && missing.recommended == Encoder::Cpu);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn real_ffmpeg_cpu_encoder_works_here() {
        let c = SpectatorConfig::default();
        if crate::doctor::which(&c.ffmpeg_binary).is_none() {
            eprintln!("no ffmpeg - skipping");
            return;
        }
        let t = test_encoder(Encoder::Cpu, &c).await;
        assert!(t.ok, "{} {}", t.detail, t.fix);
        assert!(t.speed.unwrap() > 0.0);
    }
}
