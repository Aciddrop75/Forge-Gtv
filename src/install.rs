// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! The self-installer: `forge-gtv install` copies this very program into a versioned folder, creates the
//! configuration, registers a service (systemd on Linux, Task Scheduler on Windows) and starts it.
//!
//!   <dest>/releases/<version>/forge-gtv     every installed version (rollback = switch `current`)
//!   <dest>/current -> releases/<version>  (Windows: current.txt)
//!   <dest>/forge-gtv                        launcher: `forge-gtv status|restart|logs|rollback|...`
//!   <dest>/shared/config.json             survives upgrades, as do data/, logs/ and stream/

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, hash_password, home_dir, must_dir, random_token};
use crate::util::utc_stamp;
use crate::version::{OWNER, VERSION};

pub const SERVICE_NAME: &str = "forge-gtv";
pub const TASK_NAME: &str = "The Forge GTV";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceMode {
    SystemdSystem,
    SystemdUser,
    /// Windows: run at computer start-up (as SYSTEM). Cannot show video (no desktop).
    TaskStartup,
    /// Windows: run when this user logs on (needed for the experimental video).
    TaskLogon,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Existing {
    Fresh,
    Upgrade,
    Clean,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    Nothing,
    NonEmpty,
    /// An earlier run stopped part-way (there is no shared/config.json).
    Partial(Option<String>),
    Install(Option<String>),
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // `yes`/`no_open` are consumed by the CLI layer
pub struct Opts {
    pub dir: Option<PathBuf>,
    pub service: Option<String>,
    pub yes: bool,
    pub admin_port: Option<u16>,
    pub password: Option<String>,
    pub dry_run: bool,
    pub no_open: bool,
    pub existing: Option<String>,
    /// Do not offer to install the spectator-video programs (Xvfb, xdotool, ffmpeg).
    pub no_video_deps: bool,
    /// Test hook: pretend to be this version.
    pub version: Option<String>,
}

// ------------------------------------------------------------------------------------------ I/O abstractions

pub trait Prompter {
    fn say(&mut self, text: &str);
    fn ask(&mut self, prompt: &str, default: &str) -> String;
    /// 0-based index of the chosen option.
    fn choose(&mut self, prompt: &str, options: &[&str], default: usize) -> usize;
    /// Can a person answer (and type a password for sudo)? False for `--yes` and scripts.
    fn interactive(&self) -> bool {
        false
    }
}

pub struct Terminal {
    pub interactive: bool,
}

impl Terminal {
    pub fn new(yes: bool) -> Terminal {
        Terminal { interactive: !yes && std::io::stdin().is_terminal() }
    }
}

impl Prompter for Terminal {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn say(&mut self, text: &str) {
        println!("{}", text);
    }

    fn ask(&mut self, prompt: &str, default: &str) -> String {
        if !self.interactive {
            return default.to_string();
        }
        print!("{} [{}]: ", prompt, default);
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
            return default.to_string();
        }
        let t = line.trim();
        if t.is_empty() { default.to_string() } else { t.to_string() }
    }

    fn choose(&mut self, prompt: &str, options: &[&str], default: usize) -> usize {
        if !self.interactive {
            return default;
        }
        println!("\n{}", prompt);
        for (i, o) in options.iter().enumerate() {
            println!("  {}) {}{}", i + 1, o, if i == default { "   (default)" } else { "" });
        }
        loop {
            let a = self.ask("Choose", &(default + 1).to_string());
            match a.parse::<usize>() {
                Ok(n) if n >= 1 && n <= options.len() => return n - 1,
                _ => println!("  Please enter a number between 1 and {}.", options.len()),
            }
        }
    }
}

pub trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> Result<(i32, String)>;
    /// Like `run`, but the program's output goes to the terminal as it happens and it may read the keyboard (package
    /// managers show progress; sudo asks for a password). Returns the exit code.
    fn run_visible(&self, program: &str, args: &[&str]) -> Result<i32> {
        self.run(program, args).map(|x| x.0)
    }
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<(i32, String)> {
        let out = std::process::Command::new(program).args(args).output().with_context(|| format!("cannot run {}", program))?;
        Ok((out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    }

    fn run_visible(&self, program: &str, args: &[&str]) -> Result<i32> {
        let st = std::process::Command::new(program).args(args).status().with_context(|| format!("cannot run {}", program))?;
        Ok(st.code().unwrap_or(-1))
    }
}

// ------------------------------------------------------------------------------------------ pure helpers

/// Where system units go (overridable so tests never touch /etc).
fn systemd_dir() -> PathBuf {
    std::env::var_os("FORGE_GTV_SYSTEMD_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc/systemd/system"))
}

pub fn exe_name() -> &'static str {
    if cfg!(windows) { "forge-gtv.exe" } else { "forge-gtv" }
}

pub fn detect_existing(dest: &Path) -> Found {
    let cur = || std::fs::read_link(dest.join("current")).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| std::fs::read_to_string(dest.join("current.txt")).ok().map(|s| s.trim().to_string()));
    if dest.join("shared").join("config.json").exists() {
        Found::Install(cur())
    } else if dest.join("current").exists() || dest.join("current.txt").exists() || dest.join("releases").is_dir() || dest.join("shared").is_dir() {
        Found::Partial(cur())
    } else if dest.is_dir() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(false) {
        Found::NonEmpty
    } else {
        Found::Nothing
    }
}

/// True when the running program file lives inside `dest`. A clean reinstall moves `dest` aside and an
/// upgrade rewrites it, which would pull the file we are copying from out from under the installer.
pub fn overlaps(dest: &Path, exe: &Path) -> bool {
    let canon = |p: &Path| {
        let mut probe = p.to_path_buf();
        let mut tail = PathBuf::new();
        while !probe.exists() {
            match probe.file_name().map(|n| n.to_owned()) {
                Some(n) => {
                    tail = PathBuf::from(n).join(tail);
                    probe.pop();
                }
                None => break,
            }
        }
        probe.canonicalize().unwrap_or(probe).join(tail)
    };
    canon(exe).starts_with(canon(dest))
}

pub fn systemd_unit(dest: &Path, user: Option<&str>, system: bool) -> String {
    let exe = dest.join("current").join(exe_name());
    let mut s = format!("[Unit]\nDescription=The Forge GTV (owner: {})\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\n", OWNER);
    if let (true, Some(u)) = (system, user) {
        s.push_str(&format!("User={}\n", u));
    }
    s.push_str(&format!(
        "WorkingDirectory={dest}/shared\nEnvironment=FORGE_GTV_SERVICE=1\nEnvironment=FORGE_GTV_CONFIG={dest}/shared/config.json\nEnvironmentFile=-{dest}/shared/secrets.env\n\
         ExecStart={exe} run\nRestart=always\nRestartSec=5\nKillSignal=SIGTERM\nTimeoutStopSec=25\n\n[Install]\nWantedBy={wanted}\n",
        dest = dest.display(), exe = exe.display(), wanted = if system { "multi-user.target" } else { "default.target" }));
    s
}

#[cfg_attr(unix, allow(dead_code))] // used by the Windows launcher
pub fn launcher_cmd(dest: &Path, version: &str) -> String {
    format!("@echo off\r\n\"{}\" %*\r\n", dest.join("releases").join(version).join("forge-gtv.exe").display())
}

pub fn semver_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn installed_versions(dest: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dest.join("releases")).map(|d| d.filter_map(Result::ok)
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort_by_key(|s| semver_key(s));
    v
}

pub fn current_version(dest: &Path) -> Option<String> {
    match detect_existing(dest) {
        Found::Install(v) | Found::Partial(v) => v,
        _ => None,
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ServiceInfo {
    pub mode: ServiceMode,
    pub user: Option<String>,
}

pub fn read_service_info(dest: &Path) -> Option<ServiceInfo> {
    serde_json::from_str(&std::fs::read_to_string(dest.join("shared").join("service.json")).ok()?).ok()
}

// ------------------------------------------------------------------------------------------ switching versions

pub fn point_current_at(dest: &Path, version: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let tmp = dest.join("current.new");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(format!("releases/{}", version), &tmp)?;
        std::fs::rename(&tmp, dest.join("current"))?;
        let launcher = dest.join(exe_name());
        let _ = std::fs::remove_file(&launcher);
        std::os::unix::fs::symlink(format!("current/{}", exe_name()), &launcher)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(dest.join("current.txt"), format!("{}\n", version))?;
        std::fs::write(dest.join("forge-gtv.cmd"), launcher_cmd(dest, version))?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------ service control

fn detect_user() -> Option<String> {
    std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty() && u != "root").or_else(|| std::env::var("USER").ok()).or_else(|| std::env::var("USERNAME").ok())
}

pub fn is_root(r: &dyn Runner) -> bool {
    if cfg!(windows) { return r.run("net", &["session"]).map(|x| x.0 == 0).unwrap_or(false); }
    r.run("id", &["-u"]).map(|x| x.1.trim() == "0").unwrap_or(false)
}

pub fn service_action(dest: &Path, r: &dyn Runner, action: &str) -> Result<String> {
    let info = read_service_info(dest).context("no service is registered for this install (it was installed with --service none)")?;
    let (code, out) = match info.mode {
        ServiceMode::SystemdSystem => r.run("systemctl", &[action, SERVICE_NAME])?,
        ServiceMode::SystemdUser => r.run("systemctl", &["--user", action, SERVICE_NAME])?,
        ServiceMode::TaskStartup | ServiceMode::TaskLogon => match action {
            "start" | "restart" => {
                let _ = r.run("schtasks", &["/End", "/TN", TASK_NAME]);
                r.run("schtasks", &["/Run", "/TN", TASK_NAME])?
            }
            "stop" => r.run("schtasks", &["/End", "/TN", TASK_NAME])?,
            _ => r.run("schtasks", &["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?,
        },
        ServiceMode::None => bail!("no service is registered for this install"),
    };
    if code != 0 && action != "status" && action != "stop" {
        bail!("{} {} failed: {}", SERVICE_NAME, action, out.trim());
    }
    Ok(out)
}

/// `schtasks` arguments for the Windows task that runs `forge-gtv supervise`.
pub fn task_args(mode: ServiceMode, exe: &Path, user: Option<&str>) -> Result<Vec<String>> {
    let mut a: Vec<String> = ["/Create", "/F", "/TN", TASK_NAME, "/TR"].iter().map(|x| x.to_string()).collect();
    a.push(format!("\"{}\" supervise", exe.display()));
    if mode == ServiceMode::TaskStartup {
        a.extend(["/SC", "ONSTART", "/RU", "SYSTEM", "/RL", "HIGHEST"].iter().map(|x| x.to_string()));
    } else {
        let u = user.filter(|u| !u.is_empty()).context("cannot tell which Windows user to run the task as (USERNAME is not set)")?;
        // /IT: runs only while that user is logged on, and needs no stored password
        a.extend(["/SC", "ONLOGON", "/IT", "/RU"].iter().map(|x| x.to_string()));
        a.push(u.to_string());
    }
    Ok(a)
}

/// `icacls` arguments that leave `shared\\` (token, password hash) readable only by SYSTEM, administrators and the installing user.
pub fn acl_args(shared: &Path, user: Option<&str>) -> Vec<String> {
    let mut a = vec![shared.to_string_lossy().into_owned(), "/inheritance:r".to_string()];
    for who in ["SYSTEM", "BUILTIN\\Administrators"].into_iter().chain(user.filter(|u| !u.is_empty())) {
        a.push("/grant:r".into());
        a.push(format!("{}:(OI)(CI)F", who));
    }
    a
}

fn register_service(dest: &Path, mode: ServiceMode, user: Option<&str>, r: &dyn Runner, say: &mut dyn Prompter) -> Result<()> {
    let exe = if cfg!(windows) { dest.join("releases").join(current_version(dest).unwrap_or_default()).join(exe_name()) } else { dest.join("current").join(exe_name()) };
    match mode {
        ServiceMode::None => {}
        ServiceMode::SystemdSystem => {
            let unit = systemd_dir().join(format!("{}.service", SERVICE_NAME)).to_string_lossy().into_owned();
            std::fs::write(&unit, systemd_unit(dest, user, true)).with_context(|| format!("cannot write {} (run the installer with sudo)", unit))?;
            r.run("systemctl", &["daemon-reload"])?;
            let (c, o) = r.run("systemctl", &["enable", "--now", SERVICE_NAME])?;
            if c != 0 { bail!("systemctl enable failed: {}", o.trim()); }
        }
        ServiceMode::SystemdUser => {
            let dir = home_dir().context("no home folder")?.join(".config/systemd/user");
            must_dir(&dir)?;
            std::fs::write(dir.join(format!("{}.service", SERVICE_NAME)), systemd_unit(dest, user, false))?;
            r.run("systemctl", &["--user", "daemon-reload"])?;
            let (c, o) = r.run("systemctl", &["--user", "enable", "--now", SERVICE_NAME])?;
            if c != 0 { bail!("systemctl --user enable failed: {}", o.trim()); }
            say.say("  To keep it running after you log out:  loginctl enable-linger $USER");
        }
        ServiceMode::TaskStartup | ServiceMode::TaskLogon => {
            let args = task_args(mode, &exe, user)?;
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let (c, o) = r.run("schtasks", &refs)?;
            if c != 0 {
                bail!("could not create the scheduled task: {}\n  (a start-up task needs an administrator window: right-click the installer -> Run as administrator, or choose 'When I log on'. Run the installer again to finish; nothing else is lost.)", o.trim());
            }
            let _ = r.run("schtasks", &["/Run", "/TN", TASK_NAME]);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------ install

#[derive(Debug)]
#[allow(dead_code)] // `existing`/`backup` are asserted in tests
pub struct Report {
    pub dest: PathBuf,
    pub version: String,
    pub existing: Existing,
    pub admin_port: u16,
    pub password: Option<String>,
    pub mode: ServiceMode,
    pub backup: Option<PathBuf>,
}

fn default_dir(root: bool) -> PathBuf {
    if cfg!(windows) {
        let base = if root { std::env::var_os("ProgramData") } else { std::env::var_os("LOCALAPPDATA") };
        return PathBuf::from(base.unwrap_or_else(|| "C:\\".into())).join("forge-gtv");
    }
    if root { PathBuf::from("/opt/forge-gtv") } else { home_dir().unwrap_or_else(|| PathBuf::from(".")).join("forge-gtv") }
}

pub fn install(opts: &Opts, p: &mut dyn Prompter, r: &dyn Runner) -> Result<Report> {
    let version = opts.version.clone().unwrap_or_else(|| VERSION.to_string());
    p.say(&format!("\nThe Forge GTV v{} — installer (owner {})", version, OWNER));
    p.say(&crate::version::short_notice());
    p.say("");
    let exe = std::env::current_exe().context("cannot find the installer's own file")?;
    let root = is_root(r);

    // ---- where
    let dest = match &opts.dir {
        Some(d) => d.clone(),
        None => PathBuf::from(p.ask("Install directory", &default_dir(root).to_string_lossy())),
    };
    let dest = if let Some(rest) = dest.to_string_lossy().strip_prefix("~/") { home_dir().unwrap_or_default().join(rest) } else { dest };
    if !dest.is_absolute() {
        bail!("The install directory must be an absolute path (got '{}').", dest.display());
    }
    if overlaps(&dest, &exe) {
        bail!("{} overlaps the folder this installer is running from ({}).\n  Run the installer from a separate folder (for example your Downloads folder), or choose a different install directory.\n  (Installing into the folder you started from would let the installer's own file be moved or overwritten.)", dest.display(), exe.display());
    }

    // ---- what is already there
    let found = detect_existing(&dest);
    let existing = match (&found, opts.existing.as_deref()) {
        (_, Some("upgrade")) => Existing::Upgrade,
        (_, Some("clean")) => Existing::Clean,
        (_, Some("fresh")) => Existing::Fresh,
        (Found::Nothing, _) => Existing::Fresh,
        (Found::Install(v), _) => {
            p.say(&format!("An existing installation was found in {} (version {}); this installer is v{}.", dest.display(), v.clone().unwrap_or_else(|| "unknown".into()), version));
            match p.choose("What would you like to do?", &["Upgrade: install this version and KEEP my config and data (recommended)", "Clean reinstall: move the old folder aside (backup) and start from scratch", "Abort"], 0) {
                0 => Existing::Upgrade,
                1 => Existing::Clean,
                _ => bail!("Aborted."),
            }
        }
        (Found::Partial(_), _) => {
            p.say(&format!("{} holds an installation that was never finished (there is no shared/config.json).", dest.display()));
            match p.choose("What would you like to do?", &["Finish it (existing folders are kept) (recommended)", "Clean reinstall: move the old folder aside (backup)", "Abort"], 0) {
                0 => Existing::Fresh,
                1 => Existing::Clean,
                _ => bail!("Aborted."),
            }
        }
        (Found::NonEmpty, _) => {
            p.say(&format!("{} already exists and contains files that aren't from this installer.", dest.display()));
            match p.choose("What would you like to do?", &["Install into it anyway (existing files are left alone)", "Move it aside (backup) and start clean", "Abort"], 1) {
                0 => Existing::Fresh,
                1 => Existing::Clean,
                _ => bail!("Aborted."),
            }
        }
    };
    if existing == Existing::Upgrade && !dest.join("shared").join("config.json").exists() {
        bail!("Cannot upgrade: {} does not exist (an earlier install did not finish). Choose 'Finish it' or 'Clean reinstall'.", dest.join("shared/config.json").display());
    }

    // ---- service
    let user = detect_user();
    let mode = match opts.service.as_deref() {
        Some("none") => ServiceMode::None,
        Some("system") => ServiceMode::SystemdSystem,
        Some("user") => ServiceMode::SystemdUser,
        Some("startup") => ServiceMode::TaskStartup,
        Some("logon") => ServiceMode::TaskLogon,
        Some(other) => bail!("unknown --service '{}' (use system, user, startup, logon or none)", other),
        None if existing == Existing::Upgrade => read_service_info(&dest).map(|i| i.mode).unwrap_or(ServiceMode::None),
        None if cfg!(windows) => match p.choose("How should the bot start?", &["At computer start-up (runs as SYSTEM, needs an administrator window; no spectator video)", "When I log on (needed for the experimental spectator video)", "Don't register anything (I'll start it myself)"], if root { 0 } else { 1 }) {
            0 => ServiceMode::TaskStartup,
            1 => ServiceMode::TaskLogon,
            _ => ServiceMode::None,
        },
        None => {
            let sysd = r.run("systemctl", &["--version"]).map(|x| x.0 == 0).unwrap_or(false);
            if !sysd {
                p.say("systemd was not found, so no service will be registered (start it with: <dir>/forge-gtv run).");
                ServiceMode::None
            } else {
                let def = if root { 0 } else { 1 };
                match p.choose("How should the bot start?", &["System service (starts at boot; needs sudo)", "User service (starts when you log in / with linger)", "Don't register anything (I'll start it myself)"], def) {
                    0 => ServiceMode::SystemdSystem,
                    1 => ServiceMode::SystemdUser,
                    _ => ServiceMode::None,
                }
            }
        }
    };
    if mode == ServiceMode::SystemdSystem && !root {
        bail!("A system service needs root: run the installer with sudo, or choose the user service (--service user).");
    }

    // ---- admin panel
    let admin_port = match opts.admin_port {
        Some(n) => n,
        None if existing == Existing::Upgrade => Config::load(&dest.join("shared/config.json")).map(|c| c.admin.port).unwrap_or(8787),
        None => p.ask("Admin panel port (only reachable from this computer)", "8787").parse().unwrap_or(8787),
    };

    p.say("\nThis will:");
    p.say(&format!("  1. {} {}", match existing { Existing::Clean => "move the old folder aside, then create", _ => "create / update" }, dest.display()));
    p.say(&format!("  2. install v{} as releases/{} and make it current", version, version));
    if existing != Existing::Upgrade { p.say("  3. write shared/config.json with a generated admin password"); }
    p.say(&format!("  4. service: {:?}", mode));
    if !opts.no_video_deps { p.say("  5. offer to install the spectator-video tools (Xvfb, xdotool, ffmpeg) if they are missing"); }
    if opts.dry_run {
        p.say("\n(dry run) nothing was changed.");
        return Ok(Report { dest, version, existing, admin_port, password: None, mode, backup: None });
    }

    // ---- do it
    let was_running = existing == Existing::Upgrade && service_action(&dest, r, "stop").is_ok();
    let mut backup = None;
    if existing == Existing::Clean && dest.exists() {
        let b = PathBuf::from(format!("{}.bak-{}", dest.display(), utc_stamp()));
        std::fs::rename(&dest, &b).with_context(|| format!("cannot move the old installation aside to {}", b.display()))?;
        p.say(&format!("  old installation moved to {}", b.display()));
        backup = Some(b);
    }
    for sub in ["releases", "shared/data", "shared/logs", "shared/stream"] {
        must_dir(&dest.join(sub))?;
    }
    let rel = dest.join("releases").join(&version);
    if rel.exists() {
        let kept = dest.join("releases").join(format!(".replaced-{}-{}", version, utc_stamp()));
        std::fs::rename(&rel, &kept)?;
        p.say(&format!("  v{} was already installed; the old copy was kept as {}", version, kept.display()));
    }
    must_dir(&rel)?;
    let target = rel.join(exe_name());
    std::fs::copy(&exe, &target).with_context(|| format!("cannot copy the program to {}", target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    }
    point_current_at(&dest, &version)?;
    p.say(&format!("  current -> releases/{}", version));

    let mut password = None;
    if existing != Existing::Upgrade {
        let pw = opts.password.clone().unwrap_or_else(|| random_token(9));
        let mut cfg = Config::default();
        cfg.admin.password_hash = hash_password(&pw);
        cfg.admin.port = admin_port;
        cfg.data_dir = dest.join("shared/data").to_string_lossy().into_owned();
        cfg.log_file = dest.join("shared/logs/bot.log").to_string_lossy().into_owned();
        cfg.save(&dest.join("shared/config.json"))?;
        p.say("  shared/config.json written (private)");
        password = Some(pw);
    }
    std::fs::write(dest.join("shared/service.json"), serde_json::to_string_pretty(&ServiceInfo { mode, user: user.clone() })?)?;

    // Windows does not make the config file private by itself (no file modes): lock the folder down
    if cfg!(windows) {
        let args = acl_args(&dest.join("shared"), user.as_deref());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match r.run("icacls", &refs) {
            Ok((0, _)) => p.say("  shared/ restricted to you, administrators and SYSTEM"),
            _ => p.say("  note: could not restrict shared/ with icacls - on a shared PC, restrict it yourself (it holds your Discord token)"),
        }
    }
    // A system service runs as a normal user, but the installer (sudo) just created these files as root:
    // hand them over, or the bot cannot read its config or write logs/stream files ("permission denied").
    if mode == ServiceMode::SystemdSystem && root {
        if let Some(u) = user.as_deref().filter(|u| *u != "root") {
            let shared = dest.join("shared");
            let (c, o) = r.run("chown", &["-R", u, &shared.to_string_lossy()])?;
            if c != 0 {
                bail!("could not give '{}' ownership of {}: {}", u, shared.display(), o.trim());
            }
        }
    }
    register_service(&dest, mode, user.as_deref(), r, p)?;
    if mode != ServiceMode::None {
        p.say("  service registered and started");
    } else if was_running {
        p.say("  (the service was stopped for the upgrade; start it again)");
    }
    // the video tools are optional: whatever happens here, the bot itself is installed
    if !opts.no_video_deps {
        let os = crate::deps::Os::current();
        // Windows: without a desktop session (start-up task) there is no video, so ffmpeg is pointless there
        if !(os == crate::deps::Os::Windows && mode == ServiceMode::TaskStartup) {
            crate::deps::offer(p, r, os, root, false, &crate::deps::on_path);
        }
    }
    Ok(Report { dest, version, existing, admin_port, password, mode, backup })
}

pub fn print_done(rep: &Report, p: &mut dyn Prompter) {
    let url = format!("http://127.0.0.1:{}/", rep.admin_port);
    p.say("\n==========================================================");
    p.say(&format!(" Installed The Forge GTV v{} in {}", rep.version, rep.dest.display()));
    p.say(&format!(" Admin panel : {}", url));
    if let Some(pw) = &rep.password {
        p.say(&format!(" Password    : {}   (write it down — change it in Settings)", pw));
    } else {
        p.say(" Password    : unchanged (upgrade keeps your settings)");
    }
    if rep.mode == ServiceMode::None {
        p.say(&format!(" Start it    : {} run", rep.dest.join(exe_name()).display()));
    }
    p.say(&format!(" Control     : {} status | restart | stop | logs | rollback | uninstall", rep.dest.join(if cfg!(windows) { "forge-gtv.cmd" } else { exe_name() }).display()));
    p.say(" Next        : open the admin panel — the Setup tab walks you through the Discord bot token and your game server(s).");
    p.say("==========================================================");
}

/// Poll the admin panel until it answers (or give up).
pub async fn wait_for_panel(port: u16, secs: u64) -> bool {
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    while tokio::time::Instant::now() < end {
        if matches!(crate::doctor::http_get(port, "/api/hello", None).await, Ok((200, _))) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

pub fn open_browser(url: &str) {
    let r = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        // the browser's own warnings (Firefox/snap print AppArmor, dconf and ibus complaints) must not land in this terminal
        std::process::Command::new("xdg-open").arg(url)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()
    } else {
        return;
    };
    let _ = r;
}

// ------------------------------------------------------------------------------------------ rollback / uninstall

pub fn rollback(dest: &Path, to: Option<&str>, r: &dyn Runner) -> Result<String> {
    let cur = current_version(dest).context("this does not look like an installed folder")?;
    let all = installed_versions(dest);
    let target = match to {
        Some(v) => {
            if !all.iter().any(|x| x == v) {
                bail!("version {} is not installed (installed: {})", v, all.join(", "));
            }
            v.to_string()
        }
        None => {
            let idx = all.iter().position(|x| *x == cur).context("the current version is not in releases/")?;
            if idx == 0 {
                bail!("there is no older version to roll back to (installed: {})", all.join(", "));
            }
            all[idx - 1].clone()
        }
    };
    let _ = service_action(dest, r, "stop");
    point_current_at(dest, &target)?;
    if cfg!(windows) {
        if let Some(info) = read_service_info(dest) {
            register_service(dest, info.mode, info.user.as_deref(), r, &mut Terminal { interactive: false })?;
        }
    }
    let _ = service_action(dest, r, "start");
    Ok(format!("rolled back from v{} to v{}", cur, target))
}

#[derive(Clone, Debug, Default)]
pub struct UninstallOpts {
    /// Clean uninstall: also delete the configuration, data, logs and the install folder itself.
    pub clean: bool,
    /// Also delete `<dir>.bak-*` folders left by clean reinstalls.
    pub backups: bool,
}

/// `<dest>.bak-<timestamp>` folders next to the install (made by "Clean reinstall").
pub fn backup_dirs(dest: &Path) -> Vec<PathBuf> {
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name().map(|n| n.to_string_lossy().into_owned())) else { return vec![] };
    let prefix = format!("{}.bak-", name);
    let mut v: Vec<PathBuf> = std::fs::read_dir(parent).map(|d| d.filter_map(Result::ok).map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix))).collect()).unwrap_or_default();
    v.sort();
    v
}

/// What an uninstall would remove, in plain words (shown before anything is deleted).
pub fn uninstall_plan(dest: &Path, o: &UninstallOpts) -> Vec<String> {
    let mut v = vec![format!("stop the bot and remove its service ({:?})", read_service_info(dest).map(|i| i.mode).unwrap_or(ServiceMode::None)),
        format!("delete the program files in {}", dest.join("releases").display())];
    if o.clean {
        v.push(format!("DELETE your settings, data and logs: {}", dest.join("shared").display()));
        v.push(format!("remove the folder {} if nothing else is in it", dest.display()));
    } else {
        v.push(format!("KEEP your settings, data and logs: {}", dest.join("shared").display()));
    }
    if o.backups {
        for b in backup_dirs(dest) {
            v.push(format!("DELETE the backup {}", b.display()));
        }
    } else if !backup_dirs(dest).is_empty() {
        v.push(format!("keep {} backup folder(s) from earlier clean reinstalls (add --backups to delete them)", backup_dirs(dest).len()));
    }
    v
}

/// Stop a bot that is running without a service (started by hand), using the pid file it writes.
fn stop_unmanaged(dest: &Path, r: &dyn Runner, did: &mut Vec<String>) {
    let cfg_dir = dest.join("shared");
    let data = Config::load(&cfg_dir.join("config.json")).map(|c| PathBuf::from(c.data_dir)).unwrap_or_else(|_| cfg_dir.join("data"));
    let pidf = [data.join("forge-gtv.pid"), cfg_dir.join("data").join("forge-gtv.pid")].into_iter().find(|p| p.exists());
    let Some(pidf) = pidf else { return };
    let Some(pid) = std::fs::read_to_string(&pidf).ok().and_then(|t| t.trim().parse::<u32>().ok()) else { return };
    if pid == std::process::id() {
        return;
    }
    let pid_s = pid.to_string();
    let killed = if cfg!(windows) { r.run("taskkill", &["/PID", &pid_s, "/T", "/F"]) } else { r.run("kill", &["-TERM", &pid_s]) };
    if killed.map(|x| x.0 == 0).unwrap_or(false) {
        did.push(format!("stopped the running bot (pid {})", pid));
        if !cfg!(windows) {
            std::thread::sleep(std::time::Duration::from_millis(1500)); // let it close its files and processes
        }
    }
}

pub fn uninstall(dest: &Path, o: &UninstallOpts, r: &dyn Runner) -> Result<Vec<String>> {
    // never delete anything that is not recognisably one of our installs (e.g. a mistyped --dir /home/me)
    match detect_existing(dest) {
        Found::Install(_) | Found::Partial(_) => {}
        Found::Nothing => bail!("{} does not exist - nothing to uninstall.", dest.display()),
        Found::NonEmpty => bail!("{} does not look like an install of The Forge GTV (no releases/ or shared/ folder), so nothing was touched.", dest.display()),
    }
    let mut did = Vec::new();
    if let Some(info) = read_service_info(dest) {
        let _ = service_action(dest, r, "stop");
        match info.mode {
            ServiceMode::SystemdSystem => {
                let _ = r.run("systemctl", &["disable", SERVICE_NAME]);
                let _ = std::fs::remove_file(systemd_dir().join(format!("{}.service", SERVICE_NAME)));
                let _ = r.run("systemctl", &["daemon-reload"]);
                did.push("system service removed".into());
            }
            ServiceMode::SystemdUser => {
                let _ = r.run("systemctl", &["--user", "disable", SERVICE_NAME]);
                if let Some(h) = home_dir() {
                    let _ = std::fs::remove_file(h.join(format!(".config/systemd/user/{}.service", SERVICE_NAME)));
                }
                let _ = r.run("systemctl", &["--user", "daemon-reload"]);
                did.push("user service removed".into());
            }
            ServiceMode::TaskStartup | ServiceMode::TaskLogon => {
                let _ = r.run("schtasks", &["/Delete", "/F", "/TN", TASK_NAME]);
                did.push("scheduled task removed".into());
            }
            ServiceMode::None => {}
        }
    }
    stop_unmanaged(dest, r, &mut did);
    for name in ["current", "current.txt", "forge-gtv", "forge-gtv.cmd"] {
        let p = dest.join(name);
        if p.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&p);
        }
    }
    // Windows cannot delete the program file while it is running: this also happens when the uninstaller is that file
    let own_exe_inside = std::env::current_exe().ok().is_some_and(|e| overlaps(dest, &e));
    if cfg!(windows) && own_exe_inside {
        let target = if o.clean { dest.to_path_buf() } else { dest.join("releases") };
        let cmd = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", target.display());
        let _ = std::process::Command::new("cmd").args(["/C", &cmd]).spawn();
        did.push("program files will be deleted a few seconds after this window closes".into());
    } else {
        std::fs::remove_dir_all(dest.join("releases")).ok();
        if dest.join("releases").exists() {
            bail!("could not delete {} - is the bot still running? Stop it and try again.", dest.join("releases").display());
        }
        did.push("program files removed".into());
    }
    if o.clean {
        if !(cfg!(windows) && own_exe_inside) {
            std::fs::remove_dir_all(dest.join("shared")).ok();
            if dest.join("shared").exists() {
                bail!("could not delete {} - check its permissions (a bot started as root leaves root-owned files; use sudo).", dest.join("shared").display());
            }
            did.push("settings, data and logs deleted".into());
            match std::fs::remove_dir(dest) {
                Ok(_) => did.push(format!("removed {}", dest.display())),
                Err(_) => did.push(format!("left {} in place because other files are in it", dest.display())),
            }
        } else {
            did.push("settings, data and logs will be deleted with the folder".into());
        }
    } else {
        did.push(format!("kept {} (your settings, data and logs)", dest.join("shared").display()));
    }
    if o.backups {
        for b in backup_dirs(dest) {
            if std::fs::remove_dir_all(&b).is_ok() {
                did.push(format!("deleted backup {}", b.display()));
            }
        }
    }
    Ok(did)
}

/// Windows has no systemd: this keeps `forge-gtv run` alive (and restarts it for the panel's "Restart").
pub fn supervise(config: Option<PathBuf>) -> i32 {
    let Ok(exe) = std::env::current_exe() else { return 1 };
    loop {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("run").env("FORGE_GTV_SERVICE", "1");
        if let Some(c) = &config {
            cmd.arg("--config").arg(c);
        }
        match cmd.status() {
            Ok(s) if s.code() == Some(0) => return 0,
            Ok(s) if s.code() == Some(crate::admin::EXIT_RESTART) => std::thread::sleep(std::time::Duration::from_secs(1)),
            _ => std::thread::sleep(std::time::Duration::from_secs(5)),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct Fake {
        pub calls: Mutex<Vec<String>>,
        pub root: bool,
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> Result<(i32, String)> {
            self.calls.lock().unwrap().push(format!("{} {}", program, args.join(" ")));
            if program == "id" {
                return Ok((0, if self.root { "0\n" } else { "1000\n" }.into()));
            }
            Ok((0, String::new()))
        }
    }

    #[derive(Default)]
    pub struct Script {
        pub answers: Vec<String>,
        pub said: Vec<String>,
    }

    impl Prompter for Script {
        fn say(&mut self, t: &str) { self.said.push(t.to_string()); }
        fn ask(&mut self, _: &str, default: &str) -> String {
            if self.answers.is_empty() { default.into() } else { self.answers.remove(0) }
        }
        fn choose(&mut self, _: &str, _: &[&str], default: usize) -> usize {
            if self.answers.is_empty() { default } else { self.answers.remove(0).parse::<usize>().unwrap() - 1 }
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gtv-inst-{}-{}", tag, random_token(4)));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn opts(dir: &Path, version: &str) -> Opts {
        Opts { dir: Some(dir.to_path_buf()), service: Some("none".into()), yes: true, password: Some("pw-for-tests".into()), version: Some(version.into()), no_open: true, no_video_deps: true, ..Default::default() }
    }

    #[cfg(unix)] // systemd is Linux-only
    #[test]
    fn unit_text_for_system_and_user_services() {
        let u = systemd_unit(Path::new("/opt/forge-gtv"), Some("bob"), true);
        assert!(u.contains("User=bob") && u.contains("ExecStart=/opt/forge-gtv/current/forge-gtv run") && u.contains("Restart=always"));
        assert!(u.contains("FORGE_GTV_SERVICE=1") && u.contains("WantedBy=multi-user.target"));
        let u = systemd_unit(Path::new("/home/b/forge-gtv"), Some("b"), false);
        assert!(!u.contains("User=") && u.contains("WantedBy=default.target"));
        assert!(launcher_cmd(Path::new("C:\\urt"), "3.0.0").contains("releases\\3.0.0\\forge-gtv.exe") || launcher_cmd(Path::new("C:\\urt"), "3.0.0").contains("3.0.0"));
    }

    #[test]
    fn fresh_install_layout_config_and_launcher() {
        let d = tmp("fresh").join("dest");
        let mut p = Script::default();
        let rep = install(&opts(&d, "3.1.0"), &mut p, &Fake::default()).unwrap();
        assert_eq!(rep.existing, Existing::Fresh);
        assert!(d.join("releases/3.1.0").join(exe_name()).is_file());
        assert_eq!(current_version(&d).as_deref(), Some("3.1.0"));
        #[cfg(unix)]
        {
            assert_eq!(std::fs::read_link(d.join("current")).unwrap(), PathBuf::from("releases/3.1.0"));
            assert!(d.join(exe_name()).exists()); // launcher symlink resolves
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(d.join("shared/config.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let cfg = Config::load(&d.join("shared/config.json")).unwrap();
        assert!(crate::config::verify_password("pw-for-tests", &cfg.admin.password_hash));
        assert_eq!(cfg.admin.port, 8787);
        assert!(cfg.data_dir.starts_with(d.to_str().unwrap()));
        assert_eq!(detect_existing(&d), Found::Install(Some("3.1.0".into())));
        assert_eq!(rep.password.as_deref(), Some("pw-for-tests"));
        let mut p2 = Script::default();
        print_done(&rep, &mut p2);
        let said = p2.said.join("\n");
        assert!(said.contains("pw-for-tests") && said.contains("http://127.0.0.1:8787/"));
    }

    #[test]
    fn upgrade_keeps_config_and_rollback_switches_back() {
        let d = tmp("up").join("dest");
        install(&opts(&d, "3.0.0"), &mut Script::default(), &Fake::default()).unwrap();
        let mut cfg = Config::load(&d.join("shared/config.json")).unwrap();
        cfg.discord.token = "keep.me.please".into();
        cfg.save(&d.join("shared/config.json")).unwrap();
        std::fs::write(d.join("shared/data/note.txt"), "data").unwrap();
        let mut o = opts(&d, "3.1.0");
        o.password = Some("must-not-be-used".into());
        let rep = install(&o, &mut Script { answers: vec!["1".into()], ..Default::default() }, &Fake::default()).unwrap();
        assert_eq!(rep.existing, Existing::Upgrade);
        assert!(rep.password.is_none());
        let after = Config::load(&d.join("shared/config.json")).unwrap();
        assert_eq!(after.discord.token, "keep.me.please");
        assert!(crate::config::verify_password("pw-for-tests", &after.admin.password_hash));
        assert!(d.join("shared/data/note.txt").exists());
        assert_eq!(installed_versions(&d), vec!["3.0.0", "3.1.0"]);
        assert_eq!(current_version(&d).as_deref(), Some("3.1.0"));
        let msg = rollback(&d, None, &Fake::default()).unwrap();
        assert!(msg.contains("3.1.0 to v3.0.0"), "{}", msg);
        assert_eq!(current_version(&d).as_deref(), Some("3.0.0"));
        assert!(rollback(&d, None, &Fake::default()).is_err()); // nothing older
        assert!(rollback(&d, Some("9.9.9"), &Fake::default()).unwrap_err().to_string().contains("not installed"));
        assert!(rollback(&d, Some("3.1.0"), &Fake::default()).is_ok());
    }

    #[test]
    fn clean_reinstall_moves_the_old_folder_aside() {
        let d = tmp("clean").join("dest");
        let mut o = opts(&d, "3.0.0");
        o.password = Some("first-password".into());
        install(&o, &mut Script::default(), &Fake::default()).unwrap();
        o.existing = Some("clean".into());
        o.password = Some("second-password".into());
        let rep = install(&o, &mut Script::default(), &Fake::default()).unwrap();
        let b = rep.backup.unwrap();
        assert!(Config::load(&b.join("shared/config.json")).unwrap().admin.password_hash.len() > 10);
        assert!(crate::config::verify_password("second-password", &Config::load(&d.join("shared/config.json")).unwrap().admin.password_hash));
        assert!(crate::config::verify_password("first-password", &Config::load(&b.join("shared/config.json")).unwrap().admin.password_hash));
    }

    #[test]
    fn same_version_reinstall_keeps_the_old_copy() {
        let d = tmp("same").join("dest");
        install(&opts(&d, "3.0.0"), &mut Script::default(), &Fake::default()).unwrap();
        let mut o = opts(&d, "3.0.0");
        o.existing = Some("upgrade".into());
        install(&o, &mut Script::default(), &Fake::default()).unwrap();
        let kept: Vec<_> = std::fs::read_dir(d.join("releases")).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with(".replaced-")).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(installed_versions(&d), vec!["3.0.0"]); // hidden copies are not versions
    }

    #[test]
    fn an_unfinished_install_is_offered_finish_it_and_upgrade_is_refused() {
        let d = tmp("partial").join("dest");
        std::fs::create_dir_all(d.join("releases/1.0.0")).unwrap();
        assert_eq!(detect_existing(&d), Found::Partial(None));
        let mut o = opts(&d, "3.0.0");
        o.existing = Some("upgrade".into());
        assert!(install(&o, &mut Script::default(), &Fake::default()).unwrap_err().to_string().contains("Cannot upgrade"));
        o.existing = None;
        let rep = install(&o, &mut Script { answers: vec!["1".into()], ..Default::default() }, &Fake::default()).unwrap();
        assert_eq!(rep.existing, Existing::Fresh);
        assert!(d.join("shared/config.json").exists());
    }

    #[test]
    fn installing_into_the_folder_the_installer_runs_from_is_refused() {
        let exe = std::env::current_exe().unwrap();
        let here = exe.parent().unwrap().to_path_buf();
        assert!(overlaps(&here, &exe));
        assert!(!overlaps(&here.join("sub/dir"), &exe)); // harmless: the program is not inside it
        assert!(overlaps(here.parent().unwrap(), &exe)); // a folder above it
        assert!(!overlaps(&tmp("elsewhere").join("dest"), &exe));
        let mut o = opts(&here, "3.0.0");
        o.existing = Some("clean".into());
        let e = install(&o, &mut Script::default(), &Fake::default()).unwrap_err().to_string();
        assert!(e.contains("overlaps the folder this installer is running from"), "{}", e);
        assert!(exe.exists()); // nothing was moved
    }

    #[test]
    fn relative_directories_and_bad_service_names_are_refused() {
        assert!(install(&Opts { dir: Some("relative/dir".into()), ..opts(Path::new("/x"), "3.0.0") }, &mut Script::default(), &Fake::default()).unwrap_err().to_string().contains("absolute"));
        let mut o = opts(&tmp("badsvc").join("d"), "3.0.0");
        o.service = Some("bogus".into());
        assert!(install(&o, &mut Script::default(), &Fake::default()).unwrap_err().to_string().contains("unknown --service"));
    }

    #[test]
    fn dry_run_changes_nothing() {
        let d = tmp("dry").join("dest");
        let mut o = opts(&d, "3.0.0");
        o.dry_run = true;
        install(&o, &mut Script::default(), &Fake::default()).unwrap();
        assert!(!d.exists());
    }

    #[cfg(unix)] // systemd is Linux-only
    #[test]
    fn system_service_needs_root_and_user_service_runs_systemctl() {
        let d = tmp("svc").join("dest");
        let mut o = opts(&d, "3.0.0");
        o.service = Some("system".into());
        assert!(install(&o, &mut Script::default(), &Fake { root: false, ..Default::default() }).unwrap_err().to_string().contains("needs root"));
        o.service = Some("user".into());
        let fake = Fake::default();
        let home = tmp("home");
        // SAFETY of env use in tests: only this test sets HOME
        unsafe { std::env::set_var("HOME", &home) };
        install(&o, &mut Script::default(), &fake).unwrap();
        let calls = fake.calls.lock().unwrap().join("\n");
        assert!(calls.contains("systemctl --user daemon-reload") && calls.contains("systemctl --user enable --now forge-gtv"), "{}", calls);
        assert!(std::fs::read_to_string(home.join(".config/systemd/user/forge-gtv.service")).unwrap().contains("ExecStart="));
        assert_eq!(read_service_info(&d).unwrap().mode, ServiceMode::SystemdUser);
        service_action(&d, &fake, "restart").unwrap();
        assert!(fake.calls.lock().unwrap().last().unwrap().contains("--user restart forge-gtv"));
        let did = uninstall(&d, &UninstallOpts::default(), &fake).unwrap();
        assert!(did.iter().any(|x| x.contains("user service removed")) && d.join("shared/config.json").exists() && !d.join("releases").exists());
        assert!(!home.join(".config/systemd/user/forge-gtv.service").exists());
        let did = uninstall(&d, &UninstallOpts { clean: true, backups: false }, &fake).unwrap();
        assert!(did.iter().any(|x| x.contains("deleted")) && !d.exists());
    }

    #[test]
    fn system_service_hands_shared_files_to_the_service_user() {
        let d = tmp("own").join("dest");
        let mut o = opts(&d, "3.0.0");
        o.service = Some("system".into());
        let fake = Fake { root: true, ..Default::default() };
        let units = tmp("units");
        unsafe { std::env::set_var("FORGE_GTV_SYSTEMD_DIR", &units) };
        install(&o, &mut Script::default(), &fake).unwrap();
        assert!(std::fs::read_to_string(units.join("forge-gtv.service")).unwrap().contains("ExecStart="));
        let calls = fake.calls.lock().unwrap().join("\n");
        if let Some(u) = detect_user().filter(|u| u != "root") {
            assert!(calls.contains(&format!("chown -R {} {}", u, d.join("shared").display())), "{}", calls);
        } else {
            assert!(!calls.contains("chown"), "{}", calls);
        }
    }

    #[test]
    fn clean_uninstall_removes_everything_but_only_ours() {
        let base = tmp("unin");
        let d = base.join("bot");
        install(&opts(&d, "3.0.0"), &mut Script::default(), &Fake::default()).unwrap();
        std::fs::write(d.join("shared/data/stats.json"), "{}").unwrap();
        // a clean reinstall leaves a backup next to it
        let mut o = opts(&d, "3.0.0");
        o.existing = Some("clean".into());
        let rep = install(&o, &mut Script::default(), &Fake::default()).unwrap();
        let backup = rep.backup.unwrap();
        assert_eq!(backup_dirs(&d), vec![backup.clone()]);
        // the plan says what will happen before anything is deleted
        let plan = uninstall_plan(&d, &UninstallOpts { clean: true, backups: false }).join("\n");
        assert!(plan.contains("DELETE your settings") && plan.contains("keep 1 backup folder"), "{}", plan);
        // plain uninstall keeps settings
        let keep = tmp("unin-keep").join("bot");
        install(&opts(&keep, "3.0.0"), &mut Script::default(), &Fake::default()).unwrap();
        uninstall(&keep, &UninstallOpts::default(), &Fake::default()).unwrap();
        assert!(keep.join("shared/config.json").exists() && !keep.join("releases").exists() && !keep.join("forge-gtv").exists());
        // clean uninstall deletes settings and the folder; backups only when asked
        let did = uninstall(&d, &UninstallOpts { clean: true, backups: false }, &Fake::default()).unwrap();
        assert!(!d.exists() && backup.exists(), "{:?}", did);
        uninstall(&keep, &UninstallOpts { clean: true, backups: true }, &Fake::default()).unwrap();
        assert!(!keep.exists());
        let did = uninstall(&base.join("bot"), &UninstallOpts { clean: true, backups: true }, &Fake::default());
        assert!(did.unwrap_err().to_string().contains("nothing to uninstall"));
        let _ = std::fs::remove_dir_all(backup);
    }

    #[test]
    fn uninstall_refuses_folders_that_are_not_ours_and_keeps_foreign_files() {
        let d = tmp("unin-foreign");
        std::fs::write(d.join("my-photos.txt"), "precious").unwrap();
        let e = uninstall(&d, &UninstallOpts { clean: true, backups: true }, &Fake::default()).unwrap_err().to_string();
        assert!(e.contains("does not look like an install of The Forge GTV"), "{}", e);
        assert!(d.join("my-photos.txt").exists());
        // an install that someone put an extra file into: the folder stays, our parts go
        let dest = d.join("bot");
        install(&opts(&dest, "3.0.0"), &mut Script::default(), &Fake::default()).unwrap();
        std::fs::write(dest.join("notes.txt"), "mine").unwrap();
        let did = uninstall(&dest, &UninstallOpts { clean: true, backups: false }, &Fake::default()).unwrap().join("\n");
        assert!(dest.join("notes.txt").exists() && !dest.join("shared").exists() && did.contains("left"), "{}", did);
    }

    #[test]
    fn uninstall_stops_a_bot_that_was_started_by_hand() {
        let d = tmp("unin-pid").join("bot");
        install(&opts(&d, "3.0.0"), &mut Script::default(), &Fake::default()).unwrap();
        std::fs::write(d.join("shared/data/forge-gtv.pid"), "424242").unwrap();
        let fake = Fake::default();
        let did = uninstall(&d, &UninstallOpts { clean: true, backups: false }, &fake).unwrap().join("\n");
        assert!(fake.calls.lock().unwrap().iter().any(|c| c == if cfg!(windows) { "taskkill /PID 424242 /T /F" } else { "kill -TERM 424242" }), "{:?}", fake.calls.lock().unwrap());
        assert!(did.contains("stopped the running bot"), "{}", did);
    }

    #[test]
    fn windows_folder_permissions_are_locked_down() {
        let a = acl_args(Path::new("C:\\ProgramData\\forge-gtv\\shared"), Some("PC\\bob"));
        assert_eq!(a[1], "/inheritance:r");
        assert!(a.contains(&"SYSTEM:(OI)(CI)F".to_string()) && a.contains(&"BUILTIN\\Administrators:(OI)(CI)F".to_string()) && a.contains(&"PC\\bob:(OI)(CI)F".to_string()));
        assert_eq!(a.iter().filter(|x| *x == "/grant:r").count(), 3);
        assert_eq!(acl_args(Path::new("x"), None).iter().filter(|x| *x == "/grant:r").count(), 2);
    }

    #[test]
    fn windows_style_tasks_are_registered_with_the_right_arguments() {
        let exe = Path::new("C:\\urt\\current\\forge-gtv.exe");
        let logon = task_args(ServiceMode::TaskLogon, exe, Some("DESKTOP\\bob")).unwrap();
        assert_eq!(logon[..5], ["/Create", "/F", "/TN", "The Forge GTV", "/TR"]);
        assert!(logon[5].ends_with("forge-gtv.exe\" supervise"));
        assert_eq!(logon[6..], ["/SC", "ONLOGON", "/IT", "/RU", "DESKTOP\\bob"]);
        let boot = task_args(ServiceMode::TaskStartup, exe, None).unwrap();
        assert_eq!(boot[6..], ["/SC", "ONSTART", "/RU", "SYSTEM", "/RL", "HIGHEST"]);
        assert!(task_args(ServiceMode::TaskLogon, exe, None).unwrap_err().to_string().contains("which Windows user"));
        assert!(task_args(ServiceMode::TaskLogon, exe, Some("")).is_err());
    }

    #[test]
    fn interactive_walkthrough_uses_the_answers() {
        let d = tmp("walk").join("dest");
        let mut p = Script { answers: vec![d.to_string_lossy().into_owned(), "3".into(), "9090".into()], ..Default::default() };
        let fake = Fake::default();
        let o = Opts { yes: false, version: Some("3.0.0".into()), password: Some("walkthrough-pw".into()), ..Default::default() };
        let rep = install(&o, &mut p, &fake).unwrap();
        assert_eq!(rep.dest, d);
        assert_eq!(rep.admin_port, 9090);
        assert_eq!(rep.mode, ServiceMode::None);
        assert_eq!(Config::load(&d.join("shared/config.json")).unwrap().admin.port, 9090);
    }

    #[test]
    fn the_video_tools_offer_is_part_of_the_install_unless_switched_off() {
        // no_video_deps (the default in these tests): nothing about video is said or run
        let d = tmp("vd1");
        let (mut sc, fk) = (Script::default(), Fake::default());
        install(&opts(&d.join("a"), "3.0.0"), &mut sc, &fk).unwrap();
        assert!(!sc.said.join("\n").contains("Spectator video tools"));
        assert!(!fk.calls.lock().unwrap().iter().any(|c| c.contains("apt-get") || c.contains("winget")));
        // dry run lists the step; switched off, it does not
        let mut sc = Script::default();
        install(&Opts { dry_run: true, no_video_deps: false, ..opts(&d.join("b"), "3.0.0") }, &mut sc, &Fake::default()).unwrap();
        assert!(sc.said.join("\n").contains("spectator-video tools"));
        // a real run with the step on mentions the tools, and the bot is installed either way
        let mut sc = Script::default();
        install(&Opts { no_video_deps: false, ..opts(&d.join("c"), "3.0.0") }, &mut sc, &Fake::default()).unwrap();
        assert!(sc.said.join("\n").contains("Spectator video tools"));
        assert!(d.join("c/releases/3.0.0").is_dir());
    }
}
