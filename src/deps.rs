// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! The programs the optional spectator video needs, and offering to install them.
//!
//! Linux (headless server): Xvfb (a virtual screen, so no monitor is needed), xdotool (presses the camera keys),
//! ffmpeg (encodes the picture) and the graphics / SDL / sound libraries the Urban Terror client loads.
//! Windows: the client runs on the desktop and ffmpeg captures it, so only a full ffmpeg is needed.
//!
//! Nothing here runs without a yes (or `--yes` plus the rights to do it), and a failure never fails the install.

use crate::install::{Prompter, Runner};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) { Os::Windows } else { Os::Linux }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pm {
    Apt,
    Dnf,
    Yum,
    Pacman,
    Zypper,
}

impl Pm {
    pub fn name(self) -> &'static str {
        match self {
            Pm::Apt => "apt-get",
            Pm::Dnf => "dnf",
            Pm::Yum => "yum",
            Pm::Pacman => "pacman",
            Pm::Zypper => "zypper",
        }
    }
}

/// Is `name` a program on the PATH?
pub fn on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    std::env::split_paths(&paths).any(|d| exts.iter().any(|e| d.join(format!("{}{}", name, e)).is_file()))
}

pub fn detect_pm(have: &dyn Fn(&str) -> bool) -> Option<Pm> {
    [Pm::Apt, Pm::Dnf, Pm::Yum, Pm::Pacman, Pm::Zypper].into_iter().find(|pm| have(pm.name()))
}

/// Which of the video programs are not installed.
pub fn missing_tools(os: Os, have: &dyn Fn(&str) -> bool) -> Vec<&'static str> {
    let wanted: &[&str] = match os {
        Os::Linux => &["Xvfb", "xdotool", "ffmpeg"],
        Os::Windows => &["ffmpeg"],
    };
    wanted.iter().copied().filter(|t| !have(t)).collect()
}

/// What to install with one package manager.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub pm: Pm,
    /// Needed: one command, it must succeed.
    pub core: Vec<&'static str>,
    /// Libraries the game client loads (software OpenGL, SDL 1.2, OpenAL): each tried on its own, a miss is only reported.
    pub extras: Vec<&'static str>,
    /// Things the package manager cannot do for you.
    pub notes: Vec<String>,
}

/// Package names per distribution family. `missing` are program names from `missing_tools`.
pub fn plan(pm: Pm, missing: &[&str]) -> Plan {
    let mut core: Vec<&'static str> = vec![];
    let mut notes = vec![];
    let want = |t: &str| missing.contains(&t);
    match pm {
        Pm::Apt => {
            if want("Xvfb") { core.push("xvfb"); }
            if want("xdotool") { core.push("xdotool"); }
            if want("ffmpeg") { core.push("ffmpeg"); }
        }
        Pm::Pacman => {
            if want("Xvfb") { core.push("xorg-server-xvfb"); }
            if want("xdotool") { core.push("xdotool"); }
            if want("ffmpeg") { core.push("ffmpeg"); }
        }
        Pm::Dnf | Pm::Yum | Pm::Zypper => {
            if want("Xvfb") { core.push("xorg-x11-server-Xvfb"); }
            if want("xdotool") { core.push("xdotool"); }
            if want("ffmpeg") {
                // the distributions' own ffmpeg has no H.264 encoder (patents): the video needs the full one
                notes.push(match pm {
                    Pm::Zypper => "ffmpeg: the openSUSE default repositories lack the H.264 encoder - add the Packman repository (https://en.opensuse.org/Additional_package_repositories) and install its ffmpeg".to_string(),
                    _ => "ffmpeg: Fedora/RHEL's own ffmpeg lacks the H.264 encoder - enable RPM Fusion (https://rpmfusion.org/Configuration), then: sudo dnf install ffmpeg".to_string(),
                });
            }
        }
    }
    let extras = match pm {
        Pm::Apt => vec!["libgl1-mesa-dri", "libsdl1.2debian", "libopenal1"],
        Pm::Pacman => vec!["mesa", "sdl12-compat", "openal"],
        Pm::Dnf | Pm::Yum => vec!["mesa-dri-drivers", "SDL", "openal-soft"],
        Pm::Zypper => vec!["Mesa-dri", "libSDL-1_2-0", "libopenal1"],
    };
    Plan { pm, core, extras, notes }
}

fn install_cmd(pm: Pm, pkgs: &[&str]) -> Vec<String> {
    let mut c: Vec<String> = match pm {
        Pm::Apt => vec!["env".into(), "DEBIAN_FRONTEND=noninteractive".into(), "apt-get".into(), "install".into(), "-y".into()],
        Pm::Dnf => vec!["dnf".into(), "install".into(), "-y".into()],
        Pm::Yum => vec!["yum".into(), "install".into(), "-y".into()],
        Pm::Pacman => vec!["pacman".into(), "-S".into(), "--needed".into(), "--noconfirm".into()],
        Pm::Zypper => vec!["zypper".into(), "--non-interactive".into(), "install".into()],
    };
    c.extend(pkgs.iter().map(|s| s.to_string()));
    c
}

/// The commands, in order, as (command, must_succeed). `sudo` is put in front unless we already are root.
pub fn commands(plan: &Plan, root: bool) -> Vec<(Vec<String>, bool)> {
    let mut v: Vec<(Vec<String>, bool)> = vec![];
    if plan.pm == Pm::Apt {
        v.push((vec!["apt-get".into(), "update".into()], false));
    }
    if !plan.core.is_empty() {
        v.push((install_cmd(plan.pm, &plan.core), true));
    }
    for e in &plan.extras {
        v.push((install_cmd(plan.pm, &[e]), false));
    }
    if !root {
        for (c, _) in v.iter_mut() {
            c.insert(0, "sudo".into());
        }
    }
    v
}

fn shell_line(c: &[String]) -> String {
    c.join(" ")
}

fn run_all(p: &mut dyn Prompter, r: &dyn Runner, cmds: &[(Vec<String>, bool)]) -> bool {
    let mut ok = true;
    for (c, must) in cmds {
        p.say(&format!("  $ {}", shell_line(c)));
        let args: Vec<&str> = c[1..].iter().map(String::as_str).collect();
        match r.run_visible(&c[0], &args) {
            Ok(0) => {}
            Ok(code) => {
                p.say(&format!("    (exit code {}{})", code, if *must { " - this one is needed" } else { " - optional, skipped" }));
                if *must { ok = false; }
            }
            Err(e) => {
                p.say(&format!("    could not run it: {}", e));
                if *must { ok = false; }
            }
        }
    }
    ok
}

/// Offer to install what the video needs. `force` = the person asked for it explicitly (the `install-video-deps` command):
/// ask even if the programs are already there, and install the libraries too.
pub fn offer(p: &mut dyn Prompter, r: &dyn Runner, os: Os, root: bool, force: bool, have: &dyn Fn(&str) -> bool) {
    p.say("\nSpectator video tools (optional - only needed if you want video in the player):");
    let missing = missing_tools(os, have);
    match os {
        Os::Linux => offer_linux(p, r, root, force, have, &missing),
        Os::Windows => offer_windows(p, r, force, have, &missing),
    }
}

fn offer_linux(p: &mut dyn Prompter, r: &dyn Runner, root: bool, force: bool, have: &dyn Fn(&str) -> bool, missing: &[&str]) {
    if missing.is_empty() && !force {
        p.say("  Xvfb, xdotool and ffmpeg are already installed.");
        return;
    }
    let shown = if missing.is_empty() { "the game client's libraries".to_string() } else { missing.join(", ") };
    let Some(pm) = detect_pm(have) else {
        p.say(&format!("  Missing: {}. No supported package manager was found (apt, dnf, yum, pacman, zypper): install Xvfb, xdotool and a full ffmpeg yourself.", shown));
        return;
    };
    let pl = plan(pm, missing);
    let cmds = commands(&pl, root);
    p.say(&format!("  Missing: {}. Xvfb is the virtual screen that lets the game client run with no monitor attached (headless); xdotool presses its camera keys; ffmpeg encodes the video.", shown));
    for n in &pl.notes {
        p.say(&format!("  note: {}", n));
    }
    if cmds.is_empty() {
        return;
    }
    let sudo_ok = root || (have("sudo") && r.run("sudo", &["-n", "true"]).map(|x| x.0 == 0).unwrap_or(false));
    let sudo_possible = root || have("sudo");
    if !sudo_possible {
        p.say("  This needs administrator rights and sudo was not found. As root, run:");
        for (c, _) in &cmds { p.say(&format!("    {}", shell_line(c))); }
        return;
    }
    if !p.interactive() && !sudo_ok {
        p.say("  Not installed (this needs administrator rights and no one can type a password here). To do it later:  forge-gtv install-video-deps   - or run:");
        for (c, _) in &cmds { p.say(&format!("    {}", shell_line(c))); }
        return;
    }
    let size = if pl.pm == Pm::Apt { " (about 100-300 MB of downloads)" } else { "" };
    let q = format!("Install them now with {}{}{}?", pl.pm.name(), if root { "" } else { " (sudo will ask for your password)" }, size);
    if p.choose(&q, &["Yes, install them", "No, skip (you can run: forge-gtv install-video-deps)"], 0) != 0 {
        p.say("  Skipped. Later:  forge-gtv install-video-deps");
        return;
    }
    let ok = run_all(p, r, &cmds);
    let still = missing_tools(Os::Linux, have_now());
    if ok && still.is_empty() {
        p.say("  Done: Xvfb, xdotool and ffmpeg are installed. The panel's Video tab checks the rest (the Urban Terror client and its game files are yours to install).");
    } else if !still.is_empty() {
        p.say(&format!("  Still missing after the install: {}. Run it by hand to see the message, or ask for help with the output.", still.join(", ")));
    } else {
        p.say("  Some commands failed (see above). The video check in the panel tells you what is still missing.");
    }
}

/// `on_path` for the check after an install (the PATH of this process is already right).
fn have_now() -> &'static dyn Fn(&str) -> bool {
    &on_path
}

fn offer_windows(p: &mut dyn Prompter, r: &dyn Runner, force: bool, have: &dyn Fn(&str) -> bool, missing: &[&str]) {
    p.say("  Windows needs no Xvfb or xdotool: the game client runs on your desktop and ffmpeg captures it (the PC must stay logged in).");
    if missing.is_empty() && !force {
        p.say("  ffmpeg is already installed.");
        return;
    }
    if !have("winget") {
        p.say("  ffmpeg was not found. Install a FULL build (it must include gdigrab and libx264), for example from https://www.gyan.dev/ffmpeg/builds/ (\"release full\"), and add its bin folder to PATH - or set the full path in the panel (Servers → Spectator video).");
        return;
    }
    let q = "ffmpeg was not found. Install it now with winget (Gyan.FFmpeg)?";
    if p.choose(q, &["Yes, install it", "No, skip"], 0) != 0 {
        p.say("  Skipped. Later:  winget install --id Gyan.FFmpeg -e");
        return;
    }
    p.say("  $ winget install --id Gyan.FFmpeg -e --accept-source-agreements --accept-package-agreements");
    match r.run_visible("winget", &["install", "--id", "Gyan.FFmpeg", "-e", "--accept-source-agreements", "--accept-package-agreements"]) {
        Ok(0) => p.say("  ffmpeg installed. Windows only puts it on the PATH of NEW sessions: restart the bot (or log off and on) so it is found."),
        Ok(c) => p.say(&format!("  winget finished with code {} - install ffmpeg by hand (see above).", c)),
        Err(e) => p.say(&format!("  could not run winget: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec {
        calls: Mutex<Vec<String>>,
        fail: Vec<&'static str>,
        sudo_n_ok: bool,
    }

    impl Runner for Rec {
        fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<(i32, String)> {
            self.calls.lock().unwrap().push(format!("{} {}", program, args.join(" ")));
            if program == "sudo" && args == ["-n", "true"] {
                return Ok((if self.sudo_n_ok { 0 } else { 1 }, String::new()));
            }
            let line = format!("{} {}", program, args.join(" "));
            Ok((if self.fail.iter().any(|f| line.contains(f)) { 100 } else { 0 }, String::new()))
        }
    }

    struct P {
        said: Vec<String>,
        interactive: bool,
        answer: usize,
        asked: usize,
    }

    impl Prompter for P {
        fn say(&mut self, t: &str) { self.said.push(t.to_string()); }
        fn ask(&mut self, _: &str, d: &str) -> String { d.into() }
        fn choose(&mut self, _: &str, _: &[&str], _: usize) -> usize { self.asked += 1; self.answer }
        fn interactive(&self) -> bool { self.interactive }
    }

    fn p(interactive: bool, answer: usize) -> P {
        P { said: vec![], interactive, answer, asked: 0 }
    }

    fn have_only(list: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |n| list.contains(&n)
    }

    #[test]
    fn package_manager_and_missing_tools_are_detected() {
        assert_eq!(detect_pm(&have_only(&["apt-get", "dnf"])), Some(Pm::Apt));
        assert_eq!(detect_pm(&have_only(&["pacman"])), Some(Pm::Pacman));
        assert_eq!(detect_pm(&have_only(&[])), None);
        assert_eq!(missing_tools(Os::Linux, &have_only(&["ffmpeg"])), vec!["Xvfb", "xdotool"]);
        assert!(missing_tools(Os::Linux, &have_only(&["Xvfb", "xdotool", "ffmpeg"])).is_empty());
        assert_eq!(missing_tools(Os::Windows, &have_only(&[])), vec!["ffmpeg"], "Windows never needs Xvfb or xdotool");
    }

    #[test]
    fn package_names_per_family() {
        let a = plan(Pm::Apt, &["Xvfb", "xdotool", "ffmpeg"]);
        assert_eq!(a.core, vec!["xvfb", "xdotool", "ffmpeg"]);
        assert!(a.extras.contains(&"libgl1-mesa-dri") && a.extras.contains(&"libsdl1.2debian") && a.notes.is_empty());
        assert_eq!(plan(Pm::Apt, &["xdotool"]).core, vec!["xdotool"], "only what is missing");
        assert_eq!(plan(Pm::Pacman, &["Xvfb"]).core, vec!["xorg-server-xvfb"]);
        let f = plan(Pm::Dnf, &["Xvfb", "ffmpeg"]);
        assert_eq!(f.core, vec!["xorg-x11-server-Xvfb"], "no ffmpeg from the stock repositories: it has no H.264");
        assert!(f.notes[0].contains("RPM Fusion"));
        assert!(plan(Pm::Zypper, &["ffmpeg"]).notes[0].contains("Packman"));
    }

    #[test]
    fn commands_are_noninteractive_use_sudo_only_when_needed_and_keep_extras_separate() {
        let pl = plan(Pm::Apt, &["Xvfb", "xdotool", "ffmpeg"]);
        let as_root = commands(&pl, true);
        assert_eq!(as_root[0].0, vec!["apt-get", "update"]);
        assert_eq!(as_root[1].0.join(" "), "env DEBIAN_FRONTEND=noninteractive apt-get install -y xvfb xdotool ffmpeg");
        assert!(as_root[1].1 && !as_root[0].1, "the install must succeed, the update may not");
        assert_eq!(as_root.len(), 2 + 3);
        assert!(as_root[2..].iter().all(|(c, must)| !must && c.len() == 6), "one optional library per command");
        let user = commands(&pl, false);
        assert!(user.iter().all(|(c, _)| c[0] == "sudo"));
        assert_eq!(commands(&plan(Pm::Pacman, &["ffmpeg"]), true)[0].0.join(" "), "pacman -S --needed --noconfirm ffmpeg");
        assert_eq!(commands(&plan(Pm::Zypper, &["xdotool"]), true)[0].0.join(" "), "zypper --non-interactive install xdotool");
    }

    #[test]
    fn nothing_to_do_when_everything_is_there() {
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, true, false, &have_only(&["Xvfb", "xdotool", "ffmpeg", "apt-get"]));
        assert!(r.calls.lock().unwrap().is_empty() && pr.asked == 0);
        assert!(pr.said.join("\n").contains("already installed"));
    }

    #[test]
    fn yes_installs_as_root_and_no_does_nothing() {
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, true, false, &have_only(&["apt-get"]));
        let calls = r.calls.lock().unwrap().join("|");
        assert!(calls.contains("apt-get update") && calls.contains("apt-get install -y xvfb xdotool ffmpeg") && calls.contains("libsdl1.2debian"), "{}", calls);
        assert!(!calls.contains("sudo"));
        assert!(pr.said.join("\n").contains("headless"));
        let r = Rec::default();
        let mut pr = p(true, 1);
        offer(&mut pr, &r, Os::Linux, true, false, &have_only(&["apt-get"]));
        assert!(r.calls.lock().unwrap().is_empty());
        assert!(pr.said.join("\n").contains("install-video-deps"));
    }

    #[test]
    fn a_normal_user_goes_through_sudo_and_unattended_runs_never_hang_on_a_password() {
        let have = have_only(&["apt-get", "sudo"]);
        // interactive: sudo may ask for the password itself
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, false, false, &have);
        let calls = r.calls.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c.starts_with("sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y xvfb")), "{:?}", calls);
        assert!(pr.said.join("\n").contains("sudo will ask") || pr.asked == 1);
        // unattended (--yes) with no passwordless sudo: only print the commands
        let r = Rec { sudo_n_ok: false, ..Default::default() };
        let mut pr = p(false, 0);
        offer(&mut pr, &r, Os::Linux, false, false, &have);
        assert!(r.calls.lock().unwrap().iter().all(|c| c.starts_with("sudo -n")), "nothing but the sudo probe ran");
        assert_eq!(pr.asked, 0);
        assert!(pr.said.join("\n").contains("sudo apt-get update"));
        // unattended with passwordless sudo: do it
        let r = Rec { sudo_n_ok: true, ..Default::default() };
        let mut pr = p(false, 0);
        offer(&mut pr, &r, Os::Linux, false, false, &have);
        assert!(r.calls.lock().unwrap().iter().any(|c| c.starts_with("sudo env DEBIAN_FRONTEND")));
    }

    #[test]
    fn no_package_manager_or_no_sudo_gives_instructions_not_errors() {
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, true, false, &have_only(&[]));
        assert!(pr.said.join("\n").contains("No supported package manager") && r.calls.lock().unwrap().is_empty());
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, false, false, &have_only(&["dnf"]));
        let said = pr.said.join("\n");
        assert!(said.contains("sudo was not found") && said.contains("dnf install -y xorg-x11-server-Xvfb xdotool"), "{}", said);
        assert!(said.contains("RPM Fusion"));
        assert!(r.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failing_optional_library_does_not_stop_the_rest() {
        let r = Rec { fail: vec!["libsdl1.2debian"], ..Default::default() };
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, true, false, &have_only(&["apt-get"]));
        let calls = r.calls.lock().unwrap().join("|");
        assert!(calls.contains("libopenal1"), "the library after the failing one was still tried: {}", calls);
        assert!(pr.said.join("\n").contains("optional, skipped"));
    }

    #[test]
    fn forced_run_installs_the_libraries_even_when_the_programs_exist() {
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Linux, true, true, &have_only(&["Xvfb", "xdotool", "ffmpeg", "apt-get"]));
        let calls = r.calls.lock().unwrap().join("|");
        assert!(calls.contains("libgl1-mesa-dri") && !calls.contains("install -y xvfb"), "{}", calls);
    }

    #[test]
    fn windows_only_wants_ffmpeg_and_uses_winget_when_it_is_there() {
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Windows, false, false, &have_only(&["winget"]));
        let calls = r.calls.lock().unwrap().join("|");
        assert!(calls.contains("winget install --id Gyan.FFmpeg -e"), "{}", calls);
        assert!(!calls.contains("Xvfb") && !calls.contains("xdotool"));
        assert!(pr.said.join("\n").contains("no Xvfb or xdotool") || pr.said.join("\n").contains("needs no Xvfb"));
        // without winget: say what to download
        let r = Rec::default();
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Windows, false, false, &have_only(&[]));
        assert!(pr.said.join("\n").contains("gyan.dev") && r.calls.lock().unwrap().is_empty());
        // already there
        let mut pr = p(true, 0);
        offer(&mut pr, &r, Os::Windows, false, false, &have_only(&["ffmpeg", "winget"]));
        assert!(pr.said.join("\n").contains("already installed"));
    }
}
