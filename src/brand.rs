// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Clan branding: the name, logo, icon and colour that make the posts look like the clan's - and the pictures the
//! admin panel stores for that. The ownership credit (see `version::credit`) is added to every post here and cannot be
//! switched off or changed by any setting.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

use crate::config::{Config, fix_public_url, parse_color};
use crate::embed::Embed;
use crate::version::credit;

pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// The picture type from its first bytes (never from the file name the browser sent). SVG is refused on purpose: it can
/// carry scripts.
pub fn detect_ext(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("gif")
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

pub fn content_type(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        _ => "image/webp",
    }
}

/// `0123456789abcdef.png` - what `store` produces. Anything else is not served.
pub fn valid_asset_name(n: &str) -> bool {
    match n.split_once('.') {
        Some((stem, ext)) => stem.len() == 16 && stem.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) && ["png", "jpg", "gif", "webp"].contains(&ext),
        None => false,
    }
}

pub fn is_web_address(v: &str) -> bool {
    let low = v.to_lowercase();
    (low.starts_with("https://") || low.starts_with("http://")) && v.len() <= 500 && !v.chars().any(|c| c.is_whitespace() || c.is_control()) && v.contains('.')
}

/// A setting value for `branding.logo` / `branding.icon`: empty, an uploaded file, or a web address.
pub fn valid_asset_ref(v: &str) -> bool {
    v.is_empty() || valid_asset_name(v) || is_web_address(v)
}

pub fn dir(cfg: &Config) -> PathBuf {
    Path::new(&cfg.data_dir).join("brand")
}

/// Keep an uploaded picture; the name is made from its content, so the same file always has the same address.
pub fn store(dir: &Path, bytes: &[u8]) -> Result<String> {
    if bytes.is_empty() {
        bail!("The file is empty.");
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("The picture is {:.1} MB; the limit is {} MB.", bytes.len() as f64 / 1_048_576.0, MAX_IMAGE_BYTES / 1_048_576);
    }
    let Some(ext) = detect_ext(bytes) else {
        bail!("That is not a PNG, JPEG, GIF or WebP picture.");
    };
    let hash = Sha256::digest(bytes);
    let name = format!("{}.{}", hex16(&hash), ext);
    std::fs::create_dir_all(dir)?;
    let path = dir.join(&name);
    if !path.exists() {
        let tmp = dir.join(format!("{}.tmp", name));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(name)
}

fn hex16(h: &[u8]) -> String {
    h.iter().take(8).map(|b| format!("{:02x}", b)).collect()
}

/// Delete an uploaded picture that nothing refers to any more.
pub fn remove(dir: &Path, name: &str) {
    if valid_asset_name(name) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// The settings the running broadcasts read every time they redraw, so a change in the panel shows up within seconds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub name: String,
    pub color: Option<u32>,
    /// Full addresses Discord can fetch; empty when there is nothing to show (no picture, or no public address).
    pub logo_url: String,
    pub icon_url: String,
    pub each_map: bool,
    pub delete_old: bool,
    /// The web player's public address (empty when there is none).
    pub public_url: String,
    /// Map pictures on the scoreboard: switched on, the square size and how the picture is made square.
    pub level_on: bool,
    pub level_size: u32,
    pub level_fit: crate::levelshot::Fit,
}

impl Default for crate::levelshot::Fit {
    fn default() -> Self {
        crate::levelshot::Fit::Crop
    }
}

impl Live {
    pub fn from_config(c: &Config) -> Live {
        let public = c.web.as_ref().map(|w| fix_public_url(&w.public_url).unwrap_or_default()).unwrap_or_default();
        let b = &c.branding;
        Live {
            name: b.name.trim().to_string(),
            color: parse_color(&b.color),
            logo_url: asset_url(&public, &b.logo),
            icon_url: asset_url(&public, &b.icon),
            each_map: c.discord.new_post_each_map,
            delete_old: c.discord.delete_old_posts,
            level_on: c.levelshots.enabled && !public.is_empty(),
            level_size: c.levelshots.size.clamp(crate::levelshot::MIN_SIZE, crate::levelshot::MAX_SIZE),
            level_fit: crate::levelshot::Fit::parse(&c.levelshots.fit).unwrap_or_default(),
            public_url: public,
        }
    }

    /// The post's colour: the clan colour if there is one, otherwise `default`.
    pub fn accent(&self, default: u32) -> u32 {
        self.color.unwrap_or(default)
    }

    /// Put the clan's look on a post, and the ownership credit - always the same credit, whatever the settings.
    pub fn brand(&self, mut e: Embed) -> Embed {
        if !self.name.is_empty() {
            e.author = self.name.clone();
            e.author_icon = self.icon_url.clone();
        }
        if e.thumbnail.is_empty() {
            e.thumbnail = self.logo_url.clone();
        }
        e.footer = credit();
        e.footer_icon.clear();
        e
    }
}

/// A web address for a stored picture or a typed address; `""` when it cannot be reached from outside.
pub fn asset_url(public_url: &str, value: &str) -> String {
    if is_web_address(value) {
        value.to_string()
    } else if valid_asset_name(value) && !public_url.is_empty() {
        format!("{}/brand/{}", public_url.trim_end_matches('/'), value)
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::OWNER;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

    #[test]
    fn pictures_are_recognised_by_content_not_name() {
        assert_eq!(detect_ext(PNG), Some("png"));
        assert_eq!(detect_ext(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("jpg"));
        assert_eq!(detect_ext(b"GIF89a\x01\0"), Some("gif"));
        assert_eq!(detect_ext(b"RIFF\x24\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(detect_ext(b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>"), None);
        assert_eq!(detect_ext(b"<html>"), None);
        assert_eq!(detect_ext(b""), None);
    }

    #[test]
    fn stored_pictures_get_a_content_name_and_limits_apply() {
        let d = std::env::temp_dir().join(format!("gtv-brand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let a = store(&d, PNG).unwrap();
        assert!(valid_asset_name(&a) && a.ends_with(".png"), "{}", a);
        assert_eq!(store(&d, PNG).unwrap(), a); // same content, same file
        assert!(d.join(&a).exists());
        assert!(store(&d, b"not a picture").unwrap_err().to_string().contains("not a PNG"));
        assert!(store(&d, b"").is_err());
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(store(&d, &big).unwrap_err().to_string().contains("limit"));
        remove(&d, "../../etc/passwd"); // not a stored name: ignored
        remove(&d, &a);
        assert!(!d.join(&a).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_stored_names_and_web_addresses_are_accepted() {
        assert!(valid_asset_ref("") && valid_asset_ref("0123456789abcdef.png") && valid_asset_ref("https://x.example/logo.png"));
        for bad in ["../a.png", "0123456789abcdef.svg", "short.png", "0123456789ABCDEF.png", "javascript:alert(1)", "ftp://x.y/a.png", "https://x y.png", "file:///etc/passwd"] {
            assert!(!valid_asset_ref(bad), "{}", bad);
        }
    }

    #[test]
    fn pictures_need_a_public_address_to_be_seen_by_discord() {
        assert_eq!(asset_url("http://h:8080/", "0123456789abcdef.png"), "http://h:8080/brand/0123456789abcdef.png");
        assert_eq!(asset_url("", "0123456789abcdef.png"), "");
        assert_eq!(asset_url("", "https://x.example/a.png"), "https://x.example/a.png");
        assert_eq!(asset_url("http://h", ""), "");
    }

    #[test]
    fn branding_changes_the_look_but_never_the_ownership_credit() {
        let live = Live { name: "Not the owner".into(), color: Some(0x123456), logo_url: "https://x.example/l.png".into(), icon_url: "https://x.example/i.png".into(), ..Default::default() };
        let e = live.brand(Embed::new("t", 1).footer("someone else's footer"));
        assert_eq!((e.author.as_str(), e.author_icon.as_str(), e.thumbnail.as_str()), ("Not the owner", "https://x.example/i.png", "https://x.example/l.png"));
        assert!(e.footer.contains(OWNER) && e.footer.contains("The Forge GTV v"), "{}", e.footer);
        assert_eq!(live.accent(7), 0x123456);
        assert_eq!(Live::default().accent(7), 7);
        // unbranded: no author line, same credit
        let plain = Live::default().brand(Embed::new("t", 1));
        assert!(plain.author.is_empty() && plain.thumbnail.is_empty() && plain.footer.contains(OWNER));
    }

    #[test]
    fn live_settings_follow_the_configuration() {
        let mut c = Config::default();
        c.web = Some(crate::config::WebConfig { public_url: "https//gtv.example.com/:8080".into(), ..Default::default() });
        c.branding = crate::config::BrandingConfig { name: " The Forge ".into(), logo: "0123456789abcdef.png".into(), icon: "https://x.example/i.png".into(), color: "#E67E22".into() };
        let l = Live::from_config(&c);
        assert_eq!(l.name, "The Forge");
        assert_eq!(l.color, Some(0xE67E22));
        assert_eq!(l.logo_url, "https://gtv.example.com:8080/brand/0123456789abcdef.png");
        assert_eq!(l.icon_url, "https://x.example/i.png");
        assert!(l.each_map && !l.delete_old);
    }
}
