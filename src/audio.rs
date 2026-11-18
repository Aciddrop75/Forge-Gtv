// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.
//! Game sound for the spectator video.
//!
//! Linux:   the bot makes a private PulseAudio "null sink" per server (a sound card that goes nowhere), the game client
//!          plays into it (PULSE_SINK) and ffmpeg records the sink's monitor. Nothing is heard on the machine, two
//!          servers never mix, and no real sound card is needed. Works with PulseAudio and with PipeWire's pulse server.
//! Windows: ffmpeg records a DirectShow sound device the owner names (for example "Stereo Mix" or a virtual cable).
//!          Experimental like the rest of the Windows video; it records whatever that device carries.

use std::time::Duration;

use crate::config::SpectatorConfig;
use crate::spectator::Platform;

/// Where the stream's sound comes from in this run.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioIn {
    /// A silent track (players expect an audio track to exist).
    Silent,
    /// A PulseAudio source, usually `<sink>.monitor`.
    Pulse(String),
    /// A DirectShow audio device name (Windows).
    Dshow(String),
}

impl AudioIn {
    /// What the settings ask for. `sink` is the bot's own sink when it is ready (Linux, audio "game"); without it
    /// "game" falls back to silence so the video still works.
    pub fn resolve(cfg: &SpectatorConfig, platform: Platform, sink: Option<&str>) -> AudioIn {
        match (platform, cfg.audio.as_str()) {
            (Platform::Linux, "game") => sink.map(|s| AudioIn::Pulse(format!("{}.monitor", s))).unwrap_or(AudioIn::Silent),
            (Platform::Linux, "pulse") if valid_pulse_name(&cfg.pulse_source) => AudioIn::Pulse(cfg.pulse_source.clone()),
            (Platform::Windows, "game") if valid_device_name(&cfg.audio_device) => AudioIn::Dshow(cfg.audio_device.clone()),
            _ => AudioIn::Silent,
        }
    }

    /// The sink the game client must play into (Linux), if any.
    pub fn sink(&self) -> Option<&str> {
        match self {
            AudioIn::Pulse(src) => src.strip_suffix(".monitor"),
            _ => None,
        }
    }

    /// The ffmpeg input arguments for this sound source.
    pub fn ffmpeg_input(&self) -> Vec<String> {
        let s = |x: &str| x.to_string();
        match self {
            AudioIn::Silent => vec![s("-f"), s("lavfi"), s("-i"), s("anullsrc=channel_layout=stereo:sample_rate=44100")],
            AudioIn::Pulse(src) => vec![s("-thread_queue_size"), s("1024"), s("-f"), s("pulse"), s("-i"), src.clone()],
            AudioIn::Dshow(dev) => vec![s("-thread_queue_size"), s("1024"), s("-f"), s("dshow"), s("-i"), format!("audio={}", dev)],
        }
    }

    pub fn is_silent(&self) -> bool {
        *self == AudioIn::Silent
    }
}

/// The bot's own sink for one server: `gtv_<key>_<4 hex>`. Only letters, digits and `_`; the hex keeps two keys that
/// differ only in punctuation ("a-b", "a_b") apart.
pub fn sink_name(key: &str) -> String {
    let clean: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).take(40).collect();
    let mut h: u32 = 0x811c9dc5;
    for b in key.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x01000193);
    }
    format!("gtv_{}_{:04x}", clean, h & 0xffff)
}

/// A PulseAudio source or sink name the bot will pass on: plain characters only, never starting like an option.
pub fn valid_pulse_name(s: &str) -> bool {
    s.len() <= 128 && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A DirectShow device name the bot will pass to ffmpeg: printable, and without the characters ffmpeg's dshow input
/// treats as syntax (`:` separates devices, `"` quotes).
pub fn valid_device_name(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t == s && s.chars().count() <= 128 && s.chars().all(|c| !c.is_control() && !matches!(c, ':' | '"' | '\\' | '|'))
}

/// Where the sound server keeps its socket. A desktop login or a `systemctl --user` service already has
/// `XDG_RUNTIME_DIR`; a system service (`User=` in the unit) does not, and then PulseAudio, pactl, ffmpeg and the game client
/// could each look in a different place. In that case the bot uses one private folder in its account's home for all of them.
pub fn runtime_dir_env() -> Option<(String, String)> {
    if std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    let dir = std::path::PathBuf::from(home).join(".forge-gtv-runtime");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(("XDG_RUNTIME_DIR".into(), dir.to_string_lossy().into_owned()))
}

/// Which SDL the game client is built with, from the library names inside the program file (SDL 1.2 and SDL 2 call their
/// PulseAudio driver differently).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Sdl {
    V1,
    V2,
    Unknown,
}

pub fn detect_sdl(bytes: &[u8]) -> Sdl {
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    if has(b"libSDL2") {
        Sdl::V2
    } else if has(b"libSDL-1.2") {
        Sdl::V1
    } else {
        Sdl::Unknown
    }
}

/// The environment that sends the game client's sound into `sink`: PulseAudio's own variable (read by every PulseAudio
/// program), the driver name for the client's SDL, and OpenAL told to use PulseAudio too (the game may play through OpenAL).
pub fn client_sound_env(sink: &str, sdl: Sdl) -> Vec<(String, String)> {
    let mut e = vec![("PULSE_SINK".to_string(), sink.to_string()), ("ALSOFT_DRIVERS".to_string(), "pulse".to_string())];
    match sdl {
        Sdl::V2 => e.push(("SDL_AUDIODRIVER".into(), "pulseaudio".into())),
        Sdl::V1 => e.push(("SDL_AUDIODRIVER".into(), "pulse".into())),
        Sdl::Unknown => {}
    }
    e
}

/// Game settings that keep the client's sound on: it is never the focused window on a virtual screen, and some builds mute
/// themselves then.
pub fn client_sound_args() -> Vec<String> {
    ["+set", "s_initsound", "1", "+set", "s_muteWhenUnfocused", "0", "+set", "s_muteWhenMinimized", "0"].iter().map(|s| s.to_string()).collect()
}

async fn run(program: &str, args: &[&str], secs: u64) -> Result<String, String> {
    let mut c = tokio::process::Command::new(program);
    c.args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    if let Some((k, v)) = runtime_dir_env() {
        c.env(k, v);
    }
    match tokio::time::timeout(Duration::from_secs(secs), c.output()).await {
        Err(_) => Err(format!("`{}` did not answer in {} s", program, secs)),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("`{}` is not installed", program)),
        Ok(Err(e)) => Err(format!("`{}` could not be run: {}", program, e)),
        Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(Ok(o)) => Err(format!("`{} {}` failed: {}", program, args.join(" "), String::from_utf8_lossy(&o.stderr).trim().chars().take(200).collect::<String>())),
    }
}

/// Is a PulseAudio-compatible sound server answering for this account?
pub async fn server_running() -> bool {
    run("pactl", &["info"], 5).await.is_ok()
}

/// Is `sink` in the output of `pactl list short sinks` (id TAB name TAB ...)?
pub fn has_sink(listing: &str, sink: &str) -> bool {
    listing.lines().any(|l| l.split('\t').nth(1).map(str::trim) == Some(sink))
}

pub const INSTALL_HINT: &str = "install PulseAudio (Debian/Ubuntu: sudo apt-get install pulseaudio pulseaudio-utils; Fedora: sudo dnf install pulseaudio-utils; Arch: sudo pacman -S libpulse) or set the video's Sound to \"No sound\"";

/// Make sure the sound server runs and the server's own sink exists. Returns the module id when this call created the
/// sink (so it can be removed again), None when it was already there.
pub async fn ensure_sink(sink: &str) -> Result<Option<u32>, String> {
    if !valid_pulse_name(sink) {
        return Err(format!("'{}' is not a usable sink name", sink));
    }
    if let Err(e) = run("pactl", &["info"], 5).await {
        if e.contains("not installed") {
            return Err(format!("{} - {}", e, INSTALL_HINT));
        }
        // no sound server yet (normal on a headless machine): start one for this account and keep it running
        if let Err(e) = run("pulseaudio", &["--start", "--exit-idle-time=-1"], 10).await {
            return Err(format!("no sound server is running and one could not be started ({}) - {}", e, INSTALL_HINT));
        }
        let mut up = false;
        for _ in 0..20 {
            if server_running().await {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if !up {
            return Err("the sound server was started but does not answer (see `pactl info`)".into());
        }
    }
    if has_sink(&run("pactl", &["list", "short", "sinks"], 5).await?, sink) {
        return Ok(None);
    }
    let out = run("pactl", &["load-module", "module-null-sink", &format!("sink_name={}", sink), &format!("sink_properties=device.description={}", sink)], 5).await?;
    Ok(out.trim().parse::<u32>().ok())
}

/// Remove a sink this bot created.
pub async fn release_sink(module: u32) {
    let _ = run("pactl", &["unload-module", &module.to_string()], 5).await;
}

/// The audio device names in the output of `ffmpeg -list_devices true -f dshow -i dummy` (both the current
/// `"Name" (audio)` layout and the older one with a "DirectShow audio devices" heading).
pub fn parse_dshow_audio_devices(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_audio_section = false;
    for line in text.lines() {
        if line.contains("DirectShow audio devices") {
            in_audio_section = true;
            continue;
        }
        if line.contains("DirectShow video devices") {
            in_audio_section = false;
            continue;
        }
        if line.contains("Alternative name") {
            continue;
        }
        let Some(start) = line.find('"') else { continue };
        let Some(len) = line[start + 1..].find('"') else { continue };
        let name = &line[start + 1..start + 1 + len];
        let rest = &line[start + 1 + len + 1..];
        if (rest.contains("(audio)") || (in_audio_section && !rest.contains("(video)"))) && !name.is_empty() && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// Ask ffmpeg which sound devices Windows offers.
pub async fn list_dshow_devices(ffmpeg: &str) -> Result<Vec<String>, String> {
    let mut c = tokio::process::Command::new(ffmpeg);
    c.args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]).stdin(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(15), c.output()).await {
        Err(_) => Err("ffmpeg did not answer in 15 s".into()),
        Ok(Err(e)) => Err(format!("ffmpeg ('{}') could not be run: {}", ffmpeg, e)),
        // ffmpeg prints the list on stderr and exits with an error (there is no real input): that is expected
        Ok(Ok(o)) => Ok(parse_dshow_audio_devices(&String::from_utf8_lossy(&o.stderr))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(audio: &str) -> SpectatorConfig {
        SpectatorConfig { audio: audio.into(), ..Default::default() }
    }

    #[test]
    fn the_sound_source_follows_the_setting_and_the_platform() {
        // game sound on Linux needs the bot's sink; without it the video still gets a (silent) track
        assert_eq!(AudioIn::resolve(&cfg("game"), Platform::Linux, Some("gtv_a_0001")), AudioIn::Pulse("gtv_a_0001.monitor".into()));
        assert_eq!(AudioIn::resolve(&cfg("game"), Platform::Linux, None), AudioIn::Silent);
        assert_eq!(AudioIn::resolve(&cfg("silent"), Platform::Linux, Some("gtv_a_0001")), AudioIn::Silent);
        // the older hand-made source still works, but only with a plain name
        let mut p = cfg("pulse");
        assert_eq!(AudioIn::resolve(&p, Platform::Linux, None), AudioIn::Pulse("gtv.monitor".into()));
        p.pulse_source = "x -i /etc/passwd".into();
        assert_eq!(AudioIn::resolve(&p, Platform::Linux, None), AudioIn::Silent);
        // Windows: a named device, or silence
        let mut w = cfg("game");
        assert_eq!(AudioIn::resolve(&w, Platform::Windows, None), AudioIn::Silent);
        w.audio_device = "Stereo Mix (Realtek(R) Audio)".into();
        assert_eq!(AudioIn::resolve(&w, Platform::Windows, None), AudioIn::Dshow("Stereo Mix (Realtek(R) Audio)".into()));
        assert_eq!(AudioIn::resolve(&cfg("pulse"), Platform::Windows, None), AudioIn::Silent);
        assert_eq!(AudioIn::resolve(&cfg("nonsense"), Platform::Linux, Some("s")), AudioIn::Silent);
    }

    #[test]
    fn ffmpeg_gets_one_input_per_source_and_the_client_gets_the_sink() {
        assert_eq!(AudioIn::Silent.ffmpeg_input().join(" "), "-f lavfi -i anullsrc=channel_layout=stereo:sample_rate=44100");
        assert_eq!(AudioIn::Pulse("gtv_a_0001.monitor".into()).ffmpeg_input().join(" "), "-thread_queue_size 1024 -f pulse -i gtv_a_0001.monitor");
        let d = AudioIn::Dshow("Stereo Mix (Realtek(R) Audio)".into()).ffmpeg_input();
        assert_eq!(d.last().unwrap(), "audio=Stereo Mix (Realtek(R) Audio)", "one argument, spaces and all: never split by a shell");
        assert_eq!(AudioIn::Pulse("gtv_a_0001.monitor".into()).sink(), Some("gtv_a_0001"));
        assert_eq!(AudioIn::Pulse("some_source".into()).sink(), None, "a source that is not a monitor names no sink");
        assert_eq!(AudioIn::Silent.sink(), None);
    }

    #[test]
    fn sink_names_are_plain_and_differ_per_server() {
        let a = sink_name("pub");
        assert!(a.starts_with("gtv_pub_") && valid_pulse_name(&a), "{}", a);
        assert_ne!(sink_name("a-b"), sink_name("a_b"), "keys that differ only in punctuation must not share a sink");
        assert_eq!(sink_name("pub"), sink_name("pub"));
        let odd = sink_name("My Server; rm -rf / \"x\"");
        assert!(valid_pulse_name(&odd), "{}", odd);
        assert!(sink_name(&"k".repeat(200)).len() < 60);
    }

    #[test]
    fn names_that_could_be_read_as_syntax_are_refused() {
        for ok in ["gtv.monitor", "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor", "a"] {
            assert!(valid_pulse_name(ok), "{}", ok);
        }
        for bad in ["", "a b", "x;y", "a\nb", "-i", "a/b", &"x".repeat(129)] {
            assert!(!valid_pulse_name(bad), "{}", bad);
        }
        for ok in ["Stereo Mix (Realtek(R) Audio)", "CABLE Output (VB-Audio Virtual Cable)", "virtual-audio-capturer", "Микрофон"] {
            assert!(valid_device_name(ok), "{}", ok);
        }
        for bad in ["", " lead", "trail ", "a:video=Cam", "x\"y", "a\nb", "a|b", "a\\b", &"x".repeat(129)] {
            assert!(!valid_device_name(bad), "{:?}", bad);
        }
    }

    #[test]
    fn the_client_is_pointed_at_the_sink_whatever_sound_library_it_uses() {
        assert_eq!(detect_sdl(b"\x7fELF....libSDL2-2.0.so.0\0..."), Sdl::V2);
        assert_eq!(detect_sdl(b"\x7fELF....libSDL-1.2.so.0\0..."), Sdl::V1);
        assert_eq!(detect_sdl(b"\x7fELF.... nothing"), Sdl::Unknown);
        let env = |sdl| client_sound_env("gtv_a_0001", sdl).into_iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>();
        assert!(env(Sdl::V2).contains(&"PULSE_SINK=gtv_a_0001".to_string()) && env(Sdl::V2).contains(&"SDL_AUDIODRIVER=pulseaudio".to_string()));
        assert!(env(Sdl::V1).contains(&"SDL_AUDIODRIVER=pulse".to_string()) && env(Sdl::V1).contains(&"ALSOFT_DRIVERS=pulse".to_string()));
        assert!(!env(Sdl::Unknown).iter().any(|e| e.starts_with("SDL_AUDIODRIVER")), "unknown SDL: let it choose");
        let a = client_sound_args().join(" ");
        assert!(a.contains("s_initsound 1") && a.contains("s_muteWhenUnfocused 0") && a.contains("s_muteWhenMinimized 0"));
    }

    #[test]
    fn the_sink_list_is_read_by_name_not_by_substring() {
        let listing = "0\tauto_null\tmodule-null-sink.c\ts16le 2ch 44100Hz\tIDLE\n19\tgtv_pub_1a2b\tmodule-null-sink.c\ts16le 2ch 44100Hz\tIDLE\n";
        assert!(has_sink(listing, "gtv_pub_1a2b"));
        assert!(!has_sink(listing, "gtv_pub"));
        assert!(!has_sink(listing, "module-null-sink.c"));
        assert!(!has_sink("", "x"));
    }

    #[test]
    fn windows_sound_devices_are_read_from_both_ffmpeg_layouts() {
        let new = r#"[dshow @ 000001] "Integrated Camera" (video)
[dshow @ 000001]   Alternative name "@device_pnp_\\?\usb#vid"
[dshow @ 000001] "Microphone (Realtek(R) Audio)" (audio)
[dshow @ 000001]   Alternative name "@device_cm_{33D9A762}\wave_{1}"
[dshow @ 000001] "Stereo Mix (Realtek(R) Audio)" (audio)
dummy: Immediate exit requested"#;
        assert_eq!(parse_dshow_audio_devices(new), vec!["Microphone (Realtek(R) Audio)", "Stereo Mix (Realtek(R) Audio)"]);
        let old = r#"[dshow @ 01] DirectShow video devices (some may be both video and audio devices)
[dshow @ 01]  "Integrated Camera"
[dshow @ 01]     Alternative name "@device_pnp_x"
[dshow @ 01] DirectShow audio devices
[dshow @ 01]  "Stereo Mix (Realtek Audio)"
[dshow @ 01]     Alternative name "@device_cm_y"
[dshow @ 01]  "CABLE Output (VB-Audio Virtual Cable)"
"#;
        assert_eq!(parse_dshow_audio_devices(old), vec!["Stereo Mix (Realtek Audio)", "CABLE Output (VB-Audio Virtual Cable)"]);
        assert!(parse_dshow_audio_devices("").is_empty());
    }

    /// With a real PulseAudio: the sink is made, found again without making a second one, and removed.
    /// (Skipped where `pactl`/`pulseaudio` are not installed, e.g. on the build machines.)
    #[cfg(unix)]
    #[tokio::test]
    async fn real_pulseaudio_sink_is_made_reused_and_removed() {
        if std::process::Command::new("pactl").arg("--version").output().is_err() || std::process::Command::new("pulseaudio").arg("--version").output().is_err() {
            return;
        }
        let sink = sink_name(&format!("test-{}", crate::config::random_token(4)));
        let made = match ensure_sink(&sink).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("no usable sound server here, skipping: {}", e);
                return;
            }
        };
        assert!(made.is_some(), "the first call creates the sink");
        let listing = run("pactl", &["list", "short", "sinks"], 5).await.unwrap();
        assert!(has_sink(&listing, &sink), "{}", listing);
        assert_eq!(ensure_sink(&sink).await.unwrap(), None, "the second call reuses it");
        release_sink(made.unwrap()).await;
        assert!(!has_sink(&run("pactl", &["list", "short", "sinks"], 5).await.unwrap(), &sink));
    }
}
