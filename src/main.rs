// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

mod admin;
mod app;
mod brand;
mod levelshot;
mod rcon;
mod config;
mod deps;
mod discord;
mod doctor;
mod embed;
mod fs;
mod gpu;
mod hls;
mod install;
mod logbuf;
mod runtime;
mod spectator;
mod urt;
mod util;
mod version;
mod viewers;
mod watch;
mod web;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use install::{Opts, RealRunner, Terminal};

#[derive(Parser)]
#[command(name = "forge-gtv", version = version::VERSION, about = "The Forge GTV - broadcast an Urban Terror server into Discord")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the bot (this is what the service runs)
    Run {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Install (or upgrade) this program: copies it, writes the config, registers a service
    Install {
        /// Install directory
        #[arg(long)]
        dir: Option<PathBuf>,
        /// system | user (Linux), startup | logon (Windows), none
        #[arg(long)]
        service: Option<String>,
        /// Don't ask questions; use defaults and the options given
        #[arg(long, short = 'y')]
        yes: bool,
        #[arg(long)]
        admin_port: Option<u16>,
        /// Admin password (default: a random one is generated and shown once)
        #[arg(long)]
        password: Option<String>,
        /// Show what would happen, change nothing
        #[arg(long)]
        dry_run: bool,
        /// Don't open the admin panel in a browser afterwards
        #[arg(long)]
        no_open: bool,
        /// Don't offer to install the spectator-video tools (Xvfb, xdotool, ffmpeg)
        #[arg(long)]
        no_video_deps: bool,
        /// upgrade | clean | fresh (skip the question when something is already installed)
        #[arg(long)]
        existing: Option<String>,
    },
    /// Remove the bot. Asks whether to keep your settings; --clean deletes everything.
    Uninstall {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Clean uninstall: also delete settings, data, logs and the install folder
        #[arg(long, alias = "purge")]
        clean: bool,
        /// Also delete the .bak-* folders left by clean reinstalls
        #[arg(long)]
        backups: bool,
        /// Don't ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Switch back to the previous (or a named) installed version
    Rollback {
        version: Option<String>,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    Start,
    Stop,
    Restart,
    Status,
    /// Show the log (last lines)
    Logs {
        #[arg(short = 'n', long, default_value_t = 60)]
        lines: usize,
        #[arg(short, long)]
        follow: bool,
    },
    /// Check the configuration file and say what is wrong in plain words
    CheckConfig {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Try the real connections to each game server (UDP status, games.log)
    CheckServers {
        #[arg(long)]
        config: Option<PathBuf>,
        server: Option<String>,
    },
    /// Walk the spectator-video pipeline stage by stage
    CheckStream {
        #[arg(long)]
        config: Option<PathBuf>,
        server: Option<String>,
    },
    /// Detect GPUs and test-encode with CPU and every GPU encoder this ffmpeg has
    CheckGpu {
        #[arg(long)]
        ffmpeg: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Use this server's video settings (size, fps, ffmpeg path)
        server: Option<String>,
    },
    /// Copy servers, filesystems and the Discord token from the Python version's config.json
    ImportConfig {
        file: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Install the programs the spectator video needs: Xvfb (virtual screen for headless servers), xdotool, ffmpeg and the game client's libraries (Linux); ffmpeg via winget (Windows)
    InstallVideoDeps {
        /// Don't ask; install if this account may (root, or sudo without a password)
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Set a new admin password (when you forgot it)
    ResetAdminPassword {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        password: Option<String>,
    },
    /// Show or change who can open the admin panel: only this computer (local) or other computers too (remote)
    AdminAccess {
        /// local | remote | show (default: show)
        mode: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// With remote: only these addresses / ranges may open the panel (repeat the option), e.g. --allow 203.0.113.7 --allow 10.0.0.0/8
        #[arg(long)]
        allow: Vec<String>,
        /// With remote: the new admin password (at least 12 characters; default: a random one is made and shown once)
        #[arg(long)]
        password: Option<String>,
    },
    /// Keep `run` alive and restart it when the panel asks (used by the Windows task)
    Supervise {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print the version
    Version,
    /// Show the licence (GNU GPL v3 or later), the no-warranty notice and the non-affiliation notice
    License {
        /// Print the complete licence text
        #[arg(long)]
        full: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let double_clicked = cli.cmd.is_none();
    let code = match real_main(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {:#}", e);
            1
        }
    };
    if double_clicked && cfg!(windows) {
        eprintln!("\nPress Enter to close this window.");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
    std::process::exit(code);
}

/// <dest> when this program is running from an installed layout: <dest>/releases/<ver>/forge-gtv
fn installed_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dest = exe.parent()?.parent()?.parent()?;
    (exe.parent()?.parent()?.file_name()? == "releases" && dest.join("shared").is_dir()).then(|| dest.to_path_buf())
}

fn target_dir(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    dir.or_else(installed_root).ok_or_else(|| anyhow::anyhow!("this copy is not installed - pass --dir <install folder>"))
}

fn cfg_path(p: Option<PathBuf>) -> PathBuf {
    p.unwrap_or_else(runtime::default_config_path)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("async runtime")
}

fn real_main(cli: Cli) -> anyhow::Result<i32> {
    use anyhow::Context;
    let cmd = match cli.cmd {
        Some(c) => c,
        None => {
            // started with no arguments (double-click): install if not installed, else show status
            if installed_root().is_some() {
                println!("{}\n\nThis copy is installed. Commands: status | start | stop | restart | logs | rollback | uninstall (forge-gtv --help)", version::banner());
                return Ok(0);
            }
            Cmd::Install { dir: None, service: None, yes: false, admin_port: None, password: None, dry_run: false, no_open: false, no_video_deps: false, existing: None }
        }
    };
    match cmd {
        Cmd::Version => {
            println!("{}\n{}", version::banner(), version::short_notice());
            Ok(0)
        }
        Cmd::License { full } => {
            println!("{}", if full { version::LICENSE_TEXT.to_string() } else { version::notice() });
            Ok(0)
        }
        Cmd::Run { config } => {
            let logs = logbuf::LogBuffer::new();
            logbuf::init(&logs);
            rt().block_on(runtime::run(cfg_path(config), logs))
        }
        Cmd::Supervise { config } => Ok(install::supervise(config)),
        Cmd::Install { dir, service, yes, admin_port, password, dry_run, no_open, no_video_deps, existing } => {
            let opts = Opts { dir, service, yes, admin_port, password, dry_run, no_open, no_video_deps, existing, version: None };
            let mut term = Terminal::new(yes);
            let rep = install::install(&opts, &mut term, &RealRunner)?;
            if dry_run {
                return Ok(0);
            }
            install::print_done(&rep, &mut term);
            if rep.mode != install::ServiceMode::None {
                let up = rt().block_on(install::wait_for_panel(rep.admin_port, 20));
                if up {
                    println!("The bot is running.");
                    if !no_open {
                        install::open_browser(&format!("http://127.0.0.1:{}/", rep.admin_port));
                    }
                } else {
                    println!("The service was registered but the panel did not answer within 20 s - check:  {} logs", rep.dest.join(install::exe_name()).display());
                }
            }
            Ok(0)
        }
        Cmd::Uninstall { dir, clean, backups, yes } => {
            use install::Prompter;
            let dest = target_dir(dir)?;
            let mut term = Terminal::new(yes);
            let mut o = install::UninstallOpts { clean, backups };
            if term.interactive && !clean {
                let pick = term.choose(&format!("Uninstall The Forge GTV from {}?", dest.display()),
                    &["Remove the program but KEEP my settings, data and logs (you can reinstall later and carry on)",
                      "CLEAN uninstall: remove everything, including settings, data and logs", "Cancel"], 0);
                match pick { 0 => {} 1 => o.clean = true, _ => { println!("Cancelled."); return Ok(0); } }
            }
            if term.interactive && !o.backups && !install::backup_dirs(&dest).is_empty() {
                o.backups = term.choose(&format!("There are {} backup folder(s) from earlier clean reinstalls next to it. Delete them too?", install::backup_dirs(&dest).len()), &["No, keep them", "Yes, delete them"], 0) == 1;
            }
            term.say("\nThis will:");
            for l in install::uninstall_plan(&dest, &o) {
                term.say(&format!("  - {}", l));
            }
            if term.interactive {
                let word = if o.clean { "DELETE" } else { "yes" };
                if term.ask(&format!("\nType {} to continue, anything else cancels", word), "no") != word {
                    println!("Cancelled - nothing was changed.");
                    return Ok(0);
                }
            } else if o.clean && !yes {
                anyhow::bail!("a clean uninstall deletes your settings and data - add --yes to confirm when running without a terminal");
            }
            for line in install::uninstall(&dest, &o, &RealRunner)? {
                println!("  {}", line);
            }
            println!("\nUninstalled.");
            Ok(0)
        }
        Cmd::Rollback { version, dir } => {
            println!("{}", install::rollback(&target_dir(dir)?, version.as_deref(), &RealRunner)?);
            Ok(0)
        }
        Cmd::Start | Cmd::Stop | Cmd::Restart | Cmd::Status => {
            let action = match std::env::args().nth(1).as_deref() {
                Some("start") => "start",
                Some("stop") => "stop",
                Some("restart") => "restart",
                _ => "status",
            };
            print!("{}", install::service_action(&target_dir(None)?, &RealRunner, action)?);
            Ok(0)
        }
        Cmd::Logs { lines, follow } => {
            let dest = target_dir(None)?;
            let path = config::Config::load(&dest.join("shared/config.json")).ok().map(|c| config::expand_tilde(&c.log_file)).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| dest.join("shared/logs/bot.log"));
            let text = std::fs::read_to_string(&path).with_context(|| format!("no log yet at {}", path.display()))?;
            let all: Vec<&str> = text.lines().collect();
            for l in &all[all.len().saturating_sub(lines)..] {
                println!("{}", l);
            }
            if follow {
                let mut pos = text.len() as u64;
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if len < pos {
                        pos = 0;
                    }
                    if len > pos {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut f = std::fs::File::open(&path)?;
                        f.seek(SeekFrom::Start(pos))?;
                        let mut s = String::new();
                        f.read_to_string(&mut s)?;
                        print!("{}", s);
                        pos = len;
                    }
                }
            }
            Ok(0)
        }
        Cmd::CheckConfig { config } => {
            let path = cfg_path(config);
            let cfg = config::Config::load(&path)?;
            let probs = cfg.problems();
            println!("{}: {} server(s), {} filesystem(s)", path.display(), cfg.servers.len(), cfg.filesystems.len());
            for p in &probs {
                println!("  - {}", p);
            }
            if probs.is_empty() {
                println!("  no problems found");
            }
            Ok(if probs.is_empty() { 0 } else { 1 })
        }
        Cmd::CheckServers { config, server } => {
            let cfg = config::Config::load(&cfg_path(config))?;
            let mut bad = 0;
            for (k, sc) in cfg.servers.iter().filter(|(k, _)| server.as_deref().is_none_or(|s| s == k.as_str())) {
                let checks = rt().block_on(doctor::check_server(&cfg, sc));
                print!("{}", doctor::render(&format!("server '{}'", k), &checks));
                bad += doctor::problems(&checks);
            }
            Ok(if bad == 0 { 0 } else { 1 })
        }
        Cmd::CheckStream { config, server } => {
            let path = cfg_path(config);
            let cfg = config::Config::load(&path)?;
            if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::env::set_current_dir(d);
            }
            let mut bad = 0;
            for (k, sc) in cfg.servers.iter().filter(|(k, _)| server.as_deref().is_none_or(|s| s == k.as_str())) {
                let checks = rt().block_on(doctor::check_stream(&cfg, k, sc, spectator::Platform::current()));
                print!("{}", doctor::render(&format!("video for '{}'", k), &checks));
                bad += doctor::problems(&checks);
            }
            Ok(if bad == 0 { 0 } else { 1 })
        }
        Cmd::CheckGpu { ffmpeg, config, server } => {
            let mut sp = config::SpectatorConfig::default();
            if let Ok(cfg) = config::Config::load(&cfg_path(config)) {
                if let Some(s) = cfg.servers.iter().filter(|(k, _)| server.as_deref().is_none_or(|w| w == k.as_str())).find_map(|(_, s)| s.spectator.clone()) {
                    sp = s;
                }
            }
            if let Some(f) = ffmpeg {
                sp.ffmpeg_binary = f;
            }
            let report = rt().block_on(gpu::detect(&sp, spectator::Platform::current()));
            print!("{}", gpu::render(&report));
            Ok(if report.ffmpeg_found { 0 } else { 1 })
        }
        Cmd::ImportConfig { file, config } => {
            let path = cfg_path(config);
            let imported = config::Config::load(&file)?;
            let mut cfg = config::Config::load(&path).unwrap_or_default();
            if !imported.discord.token.is_empty() {
                cfg.discord.token = imported.discord.token.clone();
            }
            if !imported.discord.restrict_to_user_ids.is_empty() {
                cfg.discord.restrict_to_user_ids = imported.discord.restrict_to_user_ids.clone();
            }
            if imported.web.is_some() {
                cfg.web = imported.web.clone();
            }
            let n = imported.servers.len();
            cfg.filesystems.extend(imported.filesystems);
            cfg.servers.extend(imported.servers);
            cfg.save(&path)?;
            println!("imported {} server(s) into {}", n, path.display());
            for p in cfg.problems() {
                println!("  - {}", p);
            }
            Ok(0)
        }
        Cmd::AdminAccess { mode, config, allow, password } => {
            let path = cfg_path(config);
            let mut cfg = config::Config::load(&path)?;
            match mode.as_deref().unwrap_or("show") {
                "show" => {}
                "local" => {
                    cfg.admin.host = config::LOCAL_HOST.into();
                    cfg.save(&path)?;
                    println!("The admin panel will only open on this computer (127.0.0.1:{}). Restart the bot to apply: forge-gtv restart", cfg.admin.port);
                }
                "remote" => {
                    for e in &allow {
                        anyhow::ensure!(config::parse_cidr(e).is_some(), "'{}' is not an address like 203.0.113.7 or a range like 10.0.0.0/8", e);
                    }
                    let pw = password.unwrap_or_else(|| config::random_token(9));
                    anyhow::ensure!(pw.chars().count() >= config::REMOTE_MIN_PASSWORD, "remote access needs a password of at least {} characters", config::REMOTE_MIN_PASSWORD);
                    cfg.admin.host = config::REMOTE_HOST.into();
                    cfg.admin.allowed_ips = allow;
                    cfg.admin.password_hash = config::hash_password(&pw);
                    cfg.save(&path)?;
                    println!("Remote access is ON: the admin panel will open from other computers at http://<this computer>:{}", cfg.admin.port);
                    println!("new admin password: {}   (shown once - the old password no longer works)", pw);
                    if cfg.admin.allowed_ips.is_empty() {
                        println!("Anyone who can reach that port and knows the password can sign in. Limit it with --allow <address>, a firewall, a VPN or an SSH tunnel.");
                    }
                    println!("The panel speaks plain HTTP: put it behind a VPN, an SSH tunnel or an HTTPS reverse proxy - do not use it over the open internet as is.");
                    println!("Restart the bot to apply: forge-gtv restart");
                }
                other => anyhow::bail!("unknown mode '{}': use local, remote or show", other),
            }
            if mode.as_deref().unwrap_or("show") == "show" {
                let a = &cfg.admin;
                println!("Admin panel: {} (listens on {}:{})", if a.remote() { "REMOTE - other computers can open it" } else { "local - only this computer" }, a.host, a.port);
                if a.remote() {
                    if a.allowed_ips.is_empty() { println!("Allowed addresses: any"); } else { println!("Allowed addresses: {}", a.allowed_ips.join(", ")); }
                }
            }
            Ok(0)
        }
        Cmd::InstallVideoDeps { yes } => {
            let mut term = Terminal::new(yes);
            let root = install::is_root(&RealRunner);
            deps::offer(&mut term, &RealRunner, deps::Os::current(), root, true, &deps::on_path);
            Ok(0)
        }
        Cmd::ResetAdminPassword { config, password } => {
            let path = cfg_path(config);
            let mut cfg = config::Config::load(&path)?;
            let pw = password.unwrap_or_else(|| config::random_token(9));
            anyhow::ensure!(!cfg.admin.remote() || pw.chars().count() >= config::REMOTE_MIN_PASSWORD,
                "the panel is open to other computers, so the password needs at least {} characters", config::REMOTE_MIN_PASSWORD);
            cfg.admin.password_hash = config::hash_password(&pw);
            cfg.save(&path)?;
            println!("new admin password: {}   (restart the bot to apply: forge-gtv restart)", pw);
            Ok(0)
        }
    }
}

#[allow(dead_code)]
fn _path_used(_: &Path) {}
