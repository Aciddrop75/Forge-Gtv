// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! File-system abstraction: identical calls on local disk or over SFTP.
//!
//! Config paths are plain local paths, or `name:/path` where `name` is an entry in the config's
//! `filesystems` section (SFTP).

use std::collections::HashMap;
use std::future::Future;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use indexmap::IndexMap;
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::config::{FsConfig, expand_tilde, fs_name_of, home_dir};
use crate::util::StopRx;

/// One item of a folder listing (names only - file contents are never read for browsing).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    /// Local files only: the executable bit is set (or it ends in .exe).
    pub exec: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Listing {
    /// The folder shown, with `..` and links resolved.
    pub path: String,
    /// The folder above it (None at the top).
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

pub const LIST_MAX: usize = 3000;

fn sort_entries(v: &mut [Entry]) {
    v.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
}

#[async_trait]
pub trait Fs: Send + Sync {
    fn label(&self) -> String;
    fn join(&self, base: &str, name: &str) -> String;
    async fn size(&self, path: &str) -> Result<u64>;
    async fn read_from(&self, path: &str, offset: u64, max: usize) -> Result<Vec<u8>>;
    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<()>;
    async fn remove(&self, path: &str) -> Result<()>;
    /// List a folder for the admin panel's file picker. An empty path means "the login's home folder".
    async fn list_dir(&self, _path: &str) -> Result<Listing> {
        bail!("browsing is not supported here")
    }
}

// ------------------------------------------------------------------------------------------ local disk

pub struct LocalFs;

#[async_trait]
impl Fs for LocalFs {
    fn label(&self) -> String {
        "local disk".into()
    }

    fn join(&self, base: &str, name: &str) -> String {
        PathBuf::from(base).join(name).to_string_lossy().into_owned()
    }

    async fn size(&self, path: &str) -> Result<u64> {
        Ok(tokio::fs::metadata(path).await.with_context(|| format!("cannot stat {}", path))?.len())
    }

    async fn read_from(&self, path: &str, offset: u64, max: usize) -> Result<Vec<u8>> {
        let mut f = tokio::fs::File::open(path).await.with_context(|| format!("cannot open {}", path))?;
        f.seek(SeekFrom::Start(offset)).await?;
        let mut buf = Vec::new();
        f.take(max as u64).read_to_end(&mut buf).await?;
        Ok(buf)
    }

    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<()> {
        let p = PathBuf::from(path);
        if let Some(d) = p.parent() {
            if !d.as_os_str().is_empty() {
                tokio::fs::create_dir_all(d).await?;
            }
        }
        let tmp = PathBuf::from(format!("{}.tmp", path));
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &p).await.with_context(|| format!("cannot write {}", path))?;
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<()> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Listing> {
        let path = path.trim();
        // Windows: nothing selected -> the drives
        if cfg!(windows) && (path.is_empty() || path == "drives") {
            let entries = ('A'..='Z').filter(|c| std::path::Path::new(&format!("{}:\\", c)).exists())
                .map(|c| Entry { name: format!("{}:\\", c), dir: true, size: 0, exec: false }).collect();
            return Ok(Listing { path: "This PC".into(), parent: None, entries, truncated: false });
        }
        let start = if path.is_empty() { home_dir().unwrap_or_else(|| PathBuf::from("/")) } else { expand_tilde(path) };
        let dir = tokio::fs::canonicalize(&start).await.with_context(|| format!("{} does not exist or cannot be opened", start.display()))?;
        let mut rd = tokio::fs::read_dir(&dir).await.with_context(|| format!("cannot read the folder {} (permission denied?)", dir.display()))?;
        let mut entries = Vec::new();
        let mut truncated = false;
        while let Some(e) = rd.next_entry().await? {
            if entries.len() >= LIST_MAX {
                truncated = true;
                break;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            // follow links so a symlinked folder can be opened; a dangling link is shown as a plain file
            let md = match tokio::fs::metadata(e.path()).await {
                Ok(m) => m,
                Err(_) => match e.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                },
            };
            #[cfg(unix)]
            let exec = {
                use std::os::unix::fs::PermissionsExt;
                !md.is_dir() && md.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let exec = name.to_lowercase().ends_with(".exe");
            entries.push(Entry { dir: md.is_dir(), size: if md.is_dir() { 0 } else { md.len() }, exec, name });
        }
        sort_entries(&mut entries);
        let parent = match dir.parent() {
            Some(p) => Some(p.to_string_lossy().into_owned()),
            None if cfg!(windows) => Some("drives".into()),
            None => None,
        };
        let shown = dir.to_string_lossy().trim_start_matches("\\\\?\\").to_string();
        Ok(Listing { path: shown, parent, entries, truncated })
    }
}

// ------------------------------------------------------------------------------------------ SFTP

/// Why the host key was refused, so the error can say how to fix it.
type Refusal = Arc<StdMutex<Option<String>>>;

struct KeyCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    trust_unknown: bool,
    refusal: Refusal,
}

impl client::Handler for KeyCheck {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.refusal.lock().unwrap() = Some("the server offered an SSH certificate, which is not supported".into());
            return Ok(false);
        };
        let fingerprint = key.fingerprint(Default::default()).to_string();
        let known = if self.known_hosts.is_file() {
            russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts)
        } else {
            Ok(false)
        };
        match known {
            Ok(true) => Ok(true),
            Ok(false) if self.trust_unknown => {
                russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts)
                    .map_err(|e| anyhow!("could not record the host key in {}: {}", self.known_hosts.display(), e))?;
                tracing::info!("trusted new SSH host key of {}:{} ({}) and recorded it in {}", self.host, self.port, fingerprint, self.known_hosts.display());
                Ok(true)
            }
            Ok(false) => {
                *self.refusal.lock().unwrap() = Some(format!(
                    "SSH host key of {host}:{port} is not trusted (fingerprint {fp}). Check that fingerprint, then trust it with: \
                     ssh-keyscan -p {port} {host} >> {kh}   -  or set \"trust_unknown_hosts\": true for this filesystem \
                     (only for a first connection you trust).",
                    host = self.host, port = self.port, fp = fingerprint, kh = self.known_hosts.display()));
                Ok(false)
            }
            Err(e) => {
                *self.refusal.lock().unwrap() = Some(format!(
                    "SSH host key of {}:{} CHANGED ({}); refusing to connect. If the server was reinstalled, remove the old line from {} (the error says which: {}).",
                    self.host, self.port, fingerprint, self.known_hosts.display(), e));
                Ok(false)
            }
        }
    }
}

struct Conn {
    _handle: client::Handle<KeyCheck>,
    sftp: Arc<SftpSession>,
}

pub struct SftpFs {
    name: String,
    cfg: FsConfig,
    password: String,
    conn: Mutex<Option<Conn>>,
}

impl SftpFs {
    pub fn new(name: &str, cfg: &FsConfig) -> Self {
        let mut password = cfg.password.clone();
        if !cfg.password_env.is_empty() {
            if let Ok(v) = std::env::var(&cfg.password_env) {
                password = v;
            }
        }
        SftpFs { name: name.to_string(), cfg: cfg.clone(), password, conn: Mutex::new(None) }
    }

    fn known_hosts_path(&self) -> PathBuf {
        if self.cfg.known_hosts.is_empty() {
            home_dir().unwrap_or_default().join(".ssh").join("known_hosts")
        } else {
            expand_tilde(&self.cfg.known_hosts)
        }
    }

    async fn connect(&self) -> Result<Conn> {
        let refusal: Refusal = Arc::new(StdMutex::new(None));
        let handler = KeyCheck {
            host: self.cfg.host.clone(),
            port: self.cfg.port,
            known_hosts: self.known_hosts_path(),
            trust_unknown: self.cfg.trust_unknown_hosts,
            refusal: refusal.clone(),
        };
        let config = client::Config {
            keepalive_interval: Some(Duration::from_secs(30)),
            inactivity_timeout: None,
            ..Default::default()
        };
        let addr = (self.cfg.host.as_str(), self.cfg.port);
        let mut handle = match tokio::time::timeout(Duration::from_secs(15), client::connect(Arc::new(config), addr, handler)).await {
            Err(_) => bail!("timed out connecting to {}:{}", self.cfg.host, self.cfg.port),
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                if let Some(why) = refusal.lock().unwrap().clone() {
                    bail!("{}", why);
                }
                return Err(anyhow!("cannot connect to {}:{}: {}", self.cfg.host, self.cfg.port, e));
            }
        };

        let user = self.cfg.user.clone();
        let mut authed = false;
        let mut tried: Vec<String> = Vec::new();
        let mut keys: Vec<PathBuf> = Vec::new();
        if !self.cfg.key_file.is_empty() {
            keys.push(expand_tilde(&self.cfg.key_file));
        } else if self.password.is_empty() {
            if let Some(h) = home_dir() {
                for n in ["id_ed25519", "id_ecdsa", "id_rsa"] {
                    let p = h.join(".ssh").join(n);
                    if p.is_file() {
                        keys.push(p);
                    }
                }
            }
        }
        for kp in keys {
            match russh::keys::load_secret_key(&kp, None) {
                Ok(key) => {
                    let hash = handle.best_supported_rsa_hash().await.ok().flatten().flatten();
                    let res = handle.authenticate_publickey(user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?;
                    if res.success() {
                        authed = true;
                        break;
                    }
                    tried.push(format!("key {} was refused", kp.display()));
                }
                Err(e) => tried.push(format!("cannot use key {}: {}", kp.display(), e)),
            }
        }
        if !authed && !self.password.is_empty() {
            let res = handle.authenticate_password(user.clone(), self.password.clone()).await?;
            if res.success() {
                authed = true;
            } else {
                tried.push("the password was refused".into());
            }
        }
        if !authed {
            if tried.is_empty() {
                tried.push("no key or password is configured and no default key was found in ~/.ssh".into());
            }
            bail!("SSH login to {}@{}:{} failed: {}", user, self.cfg.host, self.cfg.port, tried.join("; "));
        }
        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await.map_err(|e| anyhow!("SFTP subsystem failed: {}", e))?;
        tracing::info!("connected to {}", self.label());
        Ok(Conn { _handle: handle, sftp: Arc::new(sftp) })
    }

    async fn session(&self) -> Result<Arc<SftpSession>> {
        let mut g = self.conn.lock().await;
        if g.is_none() {
            *g = Some(self.connect().await?);
        }
        Ok(g.as_ref().unwrap().sftp.clone())
    }

    async fn reset(&self) {
        *self.conn.lock().await = None;
    }

    /// Run `f`, reconnecting once if the link (not the file) was the problem.
    async fn op<T, Fut, F>(&self, f: F) -> Result<T>
    where
        F: Fn(Arc<SftpSession>) -> Fut,
        Fut: Future<Output = Result<T, SftpError>>,
    {
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..2 {
            let s = self.session().await?;
            match f(s).await {
                Ok(v) => return Ok(v),
                Err(SftpError::Status(st)) if matches!(st.status_code, StatusCode::NoSuchFile | StatusCode::PermissionDenied | StatusCode::Failure) => {
                    return Err(anyhow!("{} ({})", st.error_message, st.status_code));
                }
                Err(e) => {
                    self.reset().await;
                    last = Some(anyhow!("{}", e));
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("SFTP operation failed")))
    }

    async fn makedirs(&self, path: &str) -> Result<()> {
        let mut cur = String::new();
        if path.starts_with('/') {
            cur.push('/');
        }
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !cur.is_empty() && !cur.ends_with('/') {
                cur.push('/');
            }
            cur.push_str(part);
            let c = cur.clone();
            let exists = self.op(|s| { let c = c.clone(); async move { s.try_exists(c).await } }).await?;
            if !exists {
                let c = cur.clone();
                self.op(|s| { let c = c.clone(); async move { s.create_dir(c).await } }).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Fs for SftpFs {
    fn label(&self) -> String {
        format!("sftp {}@{}:{}", if self.cfg.user.is_empty() { "?" } else { &self.cfg.user }, self.cfg.host, self.cfg.port)
    }

    fn join(&self, base: &str, name: &str) -> String {
        if base.ends_with('/') { format!("{}{}", base, name) } else { format!("{}/{}", base, name) }
    }

    async fn size(&self, path: &str) -> Result<u64> {
        let p = path.to_string();
        let md = self.op(|s| { let p = p.clone(); async move { s.metadata(p).await } }).await.with_context(|| format!("{}: {}", self.name, path))?;
        Ok(md.size.unwrap_or(0))
    }

    async fn read_from(&self, path: &str, offset: u64, max: usize) -> Result<Vec<u8>> {
        let p = path.to_string();
        self.op(|s| {
            let p = p.clone();
            async move {
                let mut f = s.open(p).await?;
                f.seek(SeekFrom::Start(offset)).await.map_err(|e| SftpError::IO(e.to_string()))?;
                let mut buf = Vec::new();
                (&mut f).take(max as u64).read_to_end(&mut buf).await.map_err(|e| SftpError::IO(e.to_string()))?;
                let _ = f.close().await;
                Ok(buf)
            }
        })
        .await
        .with_context(|| format!("{}: {}", self.name, path))
    }

    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<()> {
        if let Some((dir, _)) = path.rsplit_once('/') {
            if !dir.is_empty() {
                self.makedirs(dir).await?;
            }
        }
        let (path, tmp) = (path.to_string(), format!("{}.tmp", path));
        self.op(|s| {
            let (path, tmp) = (path.clone(), tmp.clone());
            async move {
                let mut f = s.open_with_flags(tmp.clone(), OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE).await?;
                f.write_all(data).await.map_err(|e| SftpError::IO(e.to_string()))?;
                f.shutdown().await.map_err(|e| SftpError::IO(e.to_string()))?;
                if s.rename(tmp.clone(), path.clone()).await.is_err() {
                    let _ = s.remove_file(path.clone()).await;
                    s.rename(tmp, path).await?;
                }
                Ok(())
            }
        })
        .await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let p = path.to_string();
        match self.op(|s| { let p = p.clone(); async move { s.remove_file(p).await } }).await {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().contains("No such file") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn list_dir(&self, path: &str) -> Result<Listing> {
        let asked = if path.trim().is_empty() { ".".to_string() } else { path.trim().to_string() };
        self.session().await?; // connection / login / host-key problems are reported as themselves
        let dir = self.op(|s| { let p = asked.clone(); async move { s.canonicalize(p).await } }).await
            .with_context(|| format!("{}: {} does not exist or cannot be opened", self.name, asked))?;
        let listing = {
            let d = dir.clone();
            self.op(|s| { let d = d.clone(); async move { s.read_dir(d).await.map(|rd| rd.collect::<Vec<_>>()) } }).await
                .with_context(|| format!("{}: cannot read the folder {}", self.name, dir))?
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        for e in listing {
            if entries.len() >= LIST_MAX {
                truncated = true;
                break;
            }
            let name = e.file_name();
            let md = e.metadata();
            let mut is_dir = e.file_type().is_dir();
            if e.file_type().is_symlink() {
                // a link to a folder should open like a folder
                let target = self.join(&dir, &name);
                if let Ok(m) = self.op(|s| { let t = target.clone(); async move { s.metadata(t).await } }).await {
                    is_dir = m.file_type().is_dir();
                }
            }
            let exec = !is_dir && md.permissions.map(|p| p & 0o111 != 0).unwrap_or(false);
            entries.push(Entry { name, dir: is_dir, size: if is_dir { 0 } else { md.size.unwrap_or(0) }, exec });
        }
        sort_entries(&mut entries);
        let parent = if dir == "/" || dir.is_empty() { None } else {
            Some(match dir.trim_end_matches('/').rsplit_once('/') { Some(("", _)) => "/".to_string(), Some((p, _)) => p.to_string(), None => "/".to_string() })
        };
        Ok(Listing { path: dir, parent, entries, truncated })
    }
}

// ------------------------------------------------------------------------------------------ resolver

pub struct FileSystems {
    cfg: IndexMap<String, FsConfig>,
    local: Arc<LocalFs>,
    cache: StdMutex<HashMap<String, Arc<SftpFs>>>,
}

impl FileSystems {
    pub fn new(cfg: &IndexMap<String, FsConfig>) -> Result<Self> {
        for (name, c) in cfg {
            if c.kind != "sftp" {
                bail!("filesystems.{}: unsupported type '{}'", name, c.kind);
            }
            if c.host.is_empty() || c.user.is_empty() {
                bail!("filesystems.{}: host and user are required", name);
            }
        }
        Ok(FileSystems { cfg: cfg.clone(), local: Arc::new(LocalFs), cache: StdMutex::new(HashMap::new()) })
    }

    pub fn get(&self, name: &str) -> Result<Arc<SftpFs>> {
        let c = self.cfg.get(name).ok_or_else(|| anyhow!("unknown filesystem '{}'", name))?;
        let mut cache = self.cache.lock().unwrap();
        Ok(cache.entry(name.to_string()).or_insert_with(|| Arc::new(SftpFs::new(name, c))).clone())
    }

    pub fn resolve(&self, spec: &str) -> Result<(Arc<dyn Fs>, String)> {
        if let Some(name) = fs_name_of(spec) {
            let rest = spec.split_once(':').map(|x| x.1).unwrap_or("");
            if self.cfg.contains_key(&name) {
                return Ok((self.get(&name)?, rest.to_string()));
            }
            if rest.starts_with('/') {
                bail!("unknown filesystem '{}' in '{}' (define it under \"filesystems\")", name, spec);
            }
        }
        Ok((self.local.clone(), expand_tilde(spec).to_string_lossy().into_owned()))
    }

    pub fn describe(&self) -> Vec<String> {
        let mut v = vec!["local disk".to_string()];
        for n in self.cfg.keys() {
            if let Ok(f) = self.get(n) {
                v.push(f.label());
            }
        }
        v
    }
}

// ------------------------------------------------------------------------------------------ typo help

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.to_lowercase().chars().collect(), b.to_lowercase().chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// For a local path that does not exist, find the existing path it was most likely meant to be
/// (`GamesServer` -> `GameServer`, wrong capital letters, a missing letter). None if nothing is close.
pub fn closest_existing(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.exists() {
        return None;
    }
    let mut cur = PathBuf::new();
    let mut fixed_any = false;
    for comp in p.components() {
        let next = cur.join(comp);
        if next.exists() || cur.as_os_str().is_empty() {
            cur = next;
            continue;
        }
        let want = comp.as_os_str().to_string_lossy().to_string();
        let mut best: Option<(usize, String)> = None;
        let mut tie = false;
        for e in std::fs::read_dir(&cur).ok()?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let d = edit_distance(&want, &n);
            if d <= 2.min(want.chars().count() / 3 + 1) {
                match &best {
                    Some((bd, _)) if d > *bd => {}
                    Some((bd, _)) if d == *bd => tie = true,
                    _ => { best = Some((d, n)); tie = false; }
                }
            }
        }
        match best {
            Some((_, n)) if !tie => { cur = cur.join(n); fixed_any = true; }
            _ => return None,
        }
    }
    if fixed_any && cur.exists() { Some(cur.to_string_lossy().into_owned()) } else { None }
}

/// " Did you mean X?" for a failed local path, or an empty string.
pub fn did_you_mean(fs_label: &str, path: &str) -> String {
    if fs_label != "local disk" {
        return String::new();
    }
    closest_existing(path).map(|s| format!("  Did you mean {} ?", s)).unwrap_or_default()
}

// ------------------------------------------------------------------------------------------ tailing

/// Follow a (local or remote) text file starting at its end; `on_line` gets every complete line.
pub async fn tail_file<F: FnMut(String) + Send>(fs: Arc<dyn Fs>, path: String, mut on_line: F, mut stop: StopRx) {
    const MAX_CHUNK: usize = 1 << 20;
    let mut pos: Option<u64> = None;
    let mut last_err = String::new();
    while !stop.is_stopped() {
        let step: Result<()> = async {
            let size = fs.size(&path).await?;
            let p = *pos.get_or_insert(size);
            let p = if size < p { 0 } else { p }; // truncated / rotated
            pos = Some(p);
            if size > p {
                let chunk = fs.read_from(&path, p, MAX_CHUNK).await?;
                if let Some(nl) = chunk.iter().rposition(|b| *b == b'\n') {
                    pos = Some(p + nl as u64 + 1);
                    for ln in String::from_utf8_lossy(&chunk[..nl]).lines() {
                        on_line(ln.to_string());
                    }
                }
            }
            Ok(())
        }
        .await;
        match step {
            Ok(()) => {
                last_err.clear();
                if stop.sleep_secs(0.5).await {
                    return;
                }
            }
            Err(e) => {
                let msg = format!("{:#}{}", e, did_you_mean(&fs.label(), &path));
                if msg != last_err {
                    tracing::warn!("tail {}: {}", path, msg);
                    last_err = msg;
                }
                if stop.sleep_secs(3.0).await {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typos_in_local_paths_get_a_suggestion() {
        let t = std::env::temp_dir().join(format!("gtv-typo-{}", crate::config::random_token(4)));
        std::fs::create_dir_all(t.join("GameServer").join("q3ut4")).unwrap();
        std::fs::write(t.join("GameServer").join("q3ut4").join("games-ctf.log"), "x").unwrap();
        let bad = t.join("GamesServer").join("q3ut4").join("games-ctf.log");
        let want = t.join("GameServer").join("q3ut4").join("games-ctf.log").to_string_lossy().into_owned();
        assert_eq!(closest_existing(&bad.to_string_lossy()), Some(want.clone()));
        // wrong capitals (only a typo where file names are case-sensitive: on Windows that path simply works)
        #[cfg(unix)]
        assert_eq!(closest_existing(&t.join("gameserver").join("q3ut4").join("games-ctf.log").to_string_lossy()), Some(want.clone()));
        assert_eq!(closest_existing(&t.join("GameServe").join("q3ut4").join("games-ctf.log").to_string_lossy()), Some(want.clone()));
        // nothing close, or the path is fine: no suggestion
        assert_eq!(closest_existing(&t.join("Elsewhere").join("games.log").to_string_lossy()), None);
        assert_eq!(closest_existing(&want), None);
        assert!(did_you_mean("local disk", &bad.to_string_lossy()).contains("Did you mean"));
        assert_eq!(did_you_mean("sftp u@h:22", &bad.to_string_lossy()), "");
        let _ = std::fs::remove_dir_all(&t);
    }

    use super::*;
    use crate::util::Stop;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("gtv-fs-{}", crate::config::random_token(5)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn local_roundtrip_and_partial_reads() {
        let d = tmpdir();
        let fs = LocalFs;
        let p = fs.join(d.to_str().unwrap(), "sub/dir/a.txt");
        fs.write_bytes(&p, b"hello world").await.unwrap();
        assert_eq!(fs.size(&p).await.unwrap(), 11);
        assert_eq!(fs.read_from(&p, 6, 100).await.unwrap(), b"world");
        assert_eq!(fs.read_from(&p, 0, 5).await.unwrap(), b"hello");
        fs.remove(&p).await.unwrap();
        fs.remove(&p).await.unwrap(); // already gone is fine
        assert!(fs.size(&p).await.is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn resolves_specs() {
        let mut cfg = IndexMap::new();
        cfg.insert("gs".to_string(), FsConfig { host: "h".into(), user: "u".into(), ..Default::default() });
        let f = FileSystems::new(&cfg).unwrap();
        let (fs, path) = f.resolve("gs:/a/games.log").unwrap();
        assert_eq!(path, "/a/games.log");
        assert!(fs.label().starts_with("sftp u@h:22"));
        assert_eq!(f.resolve("/plain/x").unwrap().1, "/plain/x");
        assert_eq!(f.resolve("C:\\games\\g.log").unwrap().1, "C:\\games\\g.log");
        let e = f.resolve("nope:/x").err().unwrap().to_string();
        assert!(e.contains("unknown filesystem 'nope'"), "{}", e);
        assert_eq!(f.describe().len(), 2);
        cfg.insert("bad".into(), FsConfig { kind: "ftp".into(), ..Default::default() });
        assert!(FileSystems::new(&cfg).is_err());
    }

    #[tokio::test]
    async fn tail_follows_appends_handles_partial_lines_and_rotation() {
        let d = tmpdir();
        let path = d.join("games.log");
        std::fs::write(&path, "old line\n").unwrap();
        let got = Arc::new(StdMutex::new(Vec::<String>::new()));
        let stop = Stop::new();
        let g = got.clone();
        let p = path.to_str().unwrap().to_string();
        let task = tokio::spawn(tail_file(Arc::new(LocalFs), p, move |l| g.lock().unwrap().push(l), stop.rx()));
        tokio::time::sleep(Duration::from_millis(700)).await;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"one\ntw").unwrap(); // second line is incomplete
            f.flush().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(*got.lock().unwrap(), vec!["one"]);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"o\n").unwrap();
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(*got.lock().unwrap(), vec!["one", "two"]);
        std::fs::write(&path, "new\n").unwrap(); // rotated: smaller than before
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(got.lock().unwrap().last().unwrap(), "new");
        stop.stop();
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(d);
    }

    /// Real SSH/SFTP round trip against a throw-away sshd on localhost (skipped when sshd is not installed).
    #[tokio::test]
    async fn sftp_against_a_real_sshd() {
        let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().find(|p| std::path::Path::new(p).exists());
        let sftp_server = ["/usr/lib/openssh/sftp-server", "/usr/libexec/openssh/sftp-server", "/usr/lib/ssh/sftp-server"].into_iter().find(|p| std::path::Path::new(p).exists());
        let (Some(sshd), Some(sftp_server)) = (sshd, sftp_server) else { eprintln!("sshd not installed - skipping"); return };
        let d = tmpdir();
        let sh = |args: &[&str]| assert!(std::process::Command::new(args[0]).args(&args[1..]).status().unwrap().success(), "{:?}", args);
        let (host_key, user_key) = (d.join("host_key"), d.join("user_key"));
        sh(&["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", host_key.to_str().unwrap()]);
        sh(&["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", user_key.to_str().unwrap()]);
        let me = std::env::var("USER").unwrap_or_else(|_| "root".into());
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        std::fs::copy(d.join("user_key.pub"), d.join("authorized_keys")).unwrap();
        std::fs::write(d.join("sshd_config"), format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {hk}\nAuthorizedKeysFile {ak}\nPidFile {pid}\nStrictModes no\nUsePAM no\n\
             PasswordAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin yes\nAllowUsers {me}\nSubsystem sftp {sftp}\n",
            hk = host_key.display(), ak = d.join("authorized_keys").display(), pid = d.join("sshd.pid").display(), sftp = sftp_server)).unwrap();
        let _ = std::fs::create_dir_all("/run/sshd");
        let mut child = match std::process::Command::new(sshd).args(["-D", "-e", "-f", d.join("sshd_config").to_str().unwrap()]).stderr(std::process::Stdio::null()).spawn() {
            Ok(c) => c,
            Err(e) => { eprintln!("cannot start sshd ({}) - skipping", e); return }
        };
        struct Kill(std::process::Child);
        impl Drop for Kill { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let _guard = Kill(child.try_wait().ok().flatten().map(|_| ()).map_or_else(|| child, |_| unreachable!()));
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() { break }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let known = d.join("known_hosts");
        let mut cfg = FsConfig { host: "127.0.0.1".into(), port, user: me, key_file: user_key.to_string_lossy().into_owned(), known_hosts: known.to_string_lossy().into_owned(), ..Default::default() };

        // 1. an unknown host key is refused until the user approves it
        let fs = SftpFs::new("t", &cfg);
        let e = format!("{:#}", fs.size(d.join("nothing").to_str().unwrap()).await.unwrap_err());
        assert!(e.to_lowercase().contains("host key") || e.to_lowercase().contains("known"), "{}", e);
        // 2. approving it learns the key; afterwards it connects without approval
        cfg.trust_unknown_hosts = true;
        let remote = d.join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        let fs = SftpFs::new("t", &cfg);
        let file = fs.join(remote.to_str().unwrap(), "games.log");
        fs.write_bytes(&file, b"line one\nline two\n").await.unwrap();
        assert_eq!(std::fs::read(remote.join("games.log")).unwrap(), b"line one\nline two\n");
        assert_eq!(fs.size(&file).await.unwrap(), 18);
        assert_eq!(fs.read_from(&file, 9, 100).await.unwrap(), b"line two\n");
        assert!(std::fs::read_to_string(&known).unwrap().contains("ssh-ed25519"));
        cfg.trust_unknown_hosts = false;
        let strict = SftpFs::new("t", &cfg);
        assert_eq!(strict.size(&file).await.unwrap(), 18);
        // 3. nested folders are created, files can be removed
        let nested = strict.join(remote.join("a/b").to_str().unwrap(), "x.txt");
        strict.write_bytes(&nested, b"hi").await.unwrap();
        assert_eq!(std::fs::read(remote.join("a/b/x.txt")).unwrap(), b"hi");
        strict.remove(&nested).await.unwrap();
        assert!(!remote.join("a/b/x.txt").exists());
        assert!(strict.size(&nested).await.is_err());
    }
}
