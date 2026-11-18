// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! `forge-gtv run`: start everything and wait for a stop signal.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::app::App;
use crate::config::Config;
use crate::logbuf::LogBuffer;
use crate::version::{VERSION, banner};
use crate::{admin, discord, web};

/// Where the configuration lives: --config, $FORGE_GTV_CONFIG, an installed layout, or ./config.json.
pub fn default_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("FORGE_GTV_CONFIG").or_else(|| std::env::var_os("URT_GTV_CONFIG")) {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe().and_then(|e| e.canonicalize()) {
        // <dest>/releases/<version>/forge-gtv  ->  <dest>/shared/config.json
        if let Some(dest) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            let p = dest.join("shared").join("config.json");
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("config.json")
}

/// Record which versions have run here; returns the previous version when it changed.
pub fn record_start(data_dir: &Path) -> Option<String> {
    let _ = std::fs::create_dir_all(data_dir);
    let path = data_dir.join("version_history.json");
    let mut history: Vec<serde_json::Value> = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let previous = history.last().and_then(|h| h["version"].as_str()).map(str::to_string);
    if previous.as_deref() != Some(VERSION) {
        history.push(serde_json::json!({"version": VERSION, "first_started": crate::util::utc_iso(), "previous": previous}));
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&history).unwrap_or_default());
        return previous;
    }
    None
}

pub fn load_or_create(path: &Path) -> Result<Config> {
    if path.exists() {
        return Config::load(path);
    }
    let cfg = Config::default();
    cfg.save(path).with_context(|| format!("cannot create {}", path.display()))?;
    eprintln!("Created a new configuration at {}", path.display());
    Ok(cfg)
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn run(config_path: PathBuf, logs: LogBuffer) -> Result<i32> {
    let config_path = if config_path.is_absolute() { config_path } else { std::env::current_dir()?.join(config_path) };
    let cfg = load_or_create(&config_path)?;
    // relative paths in the config (data_dir, work_dir, ...) are relative to the config file's folder
    if let Some(dir) = config_path.parent() {
        let _ = std::env::set_current_dir(dir);
    }
    if !cfg.log_file.is_empty() {
        logs.set_file(crate::config::expand_tilde(&cfg.log_file));
    }
    tracing::info!("{}", banner());
    let previous = record_start(Path::new(&cfg.data_dir));
    if let Some(p) = &previous {
        tracing::info!("updated from v{} to v{}", p, VERSION);
    }
    for p in cfg.problems() {
        tracing::warn!("config: {}", p);
    }
    let pid_path = Path::new(&cfg.data_dir).join("forge-gtv.pid");
    let _ = std::fs::write(&pid_path, std::process::id().to_string());

    let app = App::new(config_path.clone(), cfg.clone(), logs, previous);

    if cfg.web.is_some() {
        let w = cfg.web.clone().unwrap();
        match web::bind(&w.host, w.port).await {
            Ok(l) => {
                tracing::info!("web player on {}:{}", w.host, w.port);
                tokio::spawn(web::serve(l, web::router(app.clone()), app.shutdown.rx()));
            }
            Err(e) => tracing::error!("{}", e),
        }
    }
    if cfg.admin.enabled {
        match web::bind(&cfg.admin.host, cfg.admin.port).await {
            Ok(l) => {
                let shown = if cfg.admin.host == "0.0.0.0" || cfg.admin.host == "::" { "127.0.0.1".to_string() } else { cfg.admin.host.clone() };
                tracing::info!("admin panel: http://{}:{}/", shown, cfg.admin.port);
                if cfg.admin.remote() {
                    let who = if cfg.admin.allowed_ips.is_empty() { "any address".to_string() } else { cfg.admin.allowed_ips.join(", ") };
                    tracing::warn!("the admin panel is open to other computers ({}) over plain HTTP - keep it behind a VPN, an SSH tunnel or an HTTPS proxy", who);
                }
                if cfg.admin.password_hash.is_empty() {
                    tracing::warn!("no admin password is set yet: open the panel on this computer to choose one");
                }
                tokio::spawn(admin::serve(l, admin::router(app.clone()), app.shutdown.rx()));
            }
            Err(e) => tracing::error!("{}", e),
        }
    }
    discord::connect(&app).await;
    if cfg.discord_token().is_empty() {
        tracing::info!("no Discord token yet - add it in the admin panel (Setup tab)");
    }

    wait_for_signal().await;
    tracing::info!("shutting down");
    shutdown(&app).await;
    let _ = std::fs::remove_file(&pid_path);
    Ok(0)
}

pub async fn shutdown(app: &Arc<App>) {
    app.stop_all().await;
    discord::disconnect(app).await;
    app.shutdown.stop();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_history_records_changes_once() {
        let d = std::env::temp_dir().join(format!("gtv-vh-{}", crate::config::random_token(4)));
        assert_eq!(record_start(&d), None); // first start: nothing before it
        assert_eq!(record_start(&d), None); // same version again: not an update
        let text = std::fs::read_to_string(d.join("version_history.json")).unwrap();
        assert_eq!(text.matches("first_started").count(), 1);
        let mut h: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        h[0]["version"] = "0.0.1".into();
        std::fs::write(d.join("version_history.json"), serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(record_start(&d).as_deref(), Some("0.0.1"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_missing_config_is_created_with_defaults() {
        let d = std::env::temp_dir().join(format!("gtv-lc-{}", crate::config::random_token(4)));
        let p = d.join("config.json");
        let c = load_or_create(&p).unwrap();
        assert!(c.servers.is_empty() && p.exists());
        assert!(load_or_create(&p).is_ok());
        let _ = std::fs::remove_dir_all(d);
    }
}
