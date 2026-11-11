// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Level pictures ("levelshots"): the picture of the map being played, shown on the scoreboard post.
//!
//! The game keeps one per map (`levelshots/<map>.jpg` or `.tga`, usually inside a `.pk3` file). This module finds it in
//! the folders it is told about, resizes it to ONE fixed square so every post looks the same however the map's picture
//! was made, and stores the result under `data/levelshots/` with a name that carries a hash of its content. The web
//! player serves those files publicly (Discord's servers fetch them by address), and only files with that exact name
//! pattern can be reached.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::imageops::{self, FilterType};
use image::{DynamicImage, ExtendedColorType, ImageFormat, ImageReader, Limits, RgbImage};
use sha2::{Digest, Sha256};

use crate::config::Config;

pub const MIN_SIZE: u32 = 128;
pub const MAX_SIZE: u32 = 640;
pub const DEFAULT_SIZE: u32 = 320;
/// Pictures bigger than this are not read at all (a levelshot is a few tens of kilobytes).
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOURCE_SIDE: u32 = 4096;
/// How long "no picture for this map" is remembered before the folders are searched again.
const MISSING_RETRY: Duration = Duration::from_secs(300);
/// Background of the square when the whole picture is fitted inside it (close to Discord's dark card colour).
const PAD_COLOUR: [u8; 3] = [30, 31, 34];
const SOURCE_EXTS: [&str; 4] = ["jpg", "jpeg", "tga", "png"];

pub fn dir(cfg: &Config) -> PathBuf {
    Path::new(&cfg.data_dir).join("levelshots")
}

/// How a picture that is not square is made square.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Fill the square, cutting the edges of the longer side (centred).
    Crop,
    /// Keep the whole picture and fill the rest of the square with a dark background.
    Pad,
}

impl Fit {
    pub fn parse(s: &str) -> Option<Fit> {
        match s.trim().to_lowercase().as_str() {
            "" | "crop" => Some(Fit::Crop),
            "pad" => Some(Fit::Pad),
            _ => None,
        }
    }
    fn letter(self) -> char {
        match self {
            Fit::Crop => 'c',
            Fit::Pad => 'p',
        }
    }
}

/// The map names that are looked up: letters, digits, `_` and `-` only (so a name can never walk out of a folder), lower case.
pub fn clean_map(m: &str) -> Option<String> {
    let t = m.trim().to_lowercase();
    (!t.is_empty() && t.len() <= 64 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(t)
}

/// `ut4_turnpike-320c-0123abcd.jpg` - what `resolve` produces. Anything else is not served.
pub fn valid_file_name(n: &str) -> bool {
    let Some(stem) = n.strip_suffix(".jpg") else { return false };
    let mut parts = stem.rsplitn(3, '-');
    let (hash, shape, map) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let shape_ok = shape.len() == 4 && shape[..3].chars().all(|c| c.is_ascii_digit()) && matches!(shape.as_bytes()[3], b'c' | b'p');
    hash.len() == 8 && hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) && shape_ok && clean_map(map).is_some_and(|m| m == map)
}

fn format_of(ext: &str) -> Option<ImageFormat> {
    match ext {
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "tga" => Some(ImageFormat::Tga),
        _ => None,
    }
}

// ------------------------------------------------------------------------------------------ finding the picture

fn read_limited(path: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// A picture lying around loose: `<dir>/<map>.jpg` or `<dir>/levelshots/<map>.jpg` (any letter case).
fn find_loose(dir: &Path, map: &str) -> Option<(Vec<u8>, String, String)> {
    for base in [dir.to_path_buf(), dir.join("levelshots")] {
        let Ok(rd) = std::fs::read_dir(&base) else { continue };
        let names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().map(String::from)).collect();
        for ext in SOURCE_EXTS {
            let want = format!("{}.{}", map, ext);
            if let Some(real) = names.iter().find(|n| n.to_lowercase() == want) {
                let p = base.join(real);
                if let Some(b) = read_limited(&p) {
                    return Some((b, ext.to_string(), p.display().to_string()));
                }
            }
        }
    }
    None
}

/// The picture inside one `.pk3` (a zip file): `levelshots/<map>.jpg` or `.tga`, any letter case.
fn find_in_pk3(pk3: &Path, map: &str) -> Option<(Vec<u8>, String, String)> {
    let file = std::fs::File::open(pk3).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let found: Vec<(String, &'static str)> = {
        let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|c| c.into_owned())).collect();
        SOURCE_EXTS
            .iter()
            .filter_map(|ext| {
                let want = format!("levelshots/{}.{}", map, ext);
                names.iter().find(|n| n.replace('\\', "/").to_lowercase() == want).map(|n| (n.clone(), *ext))
            })
            .collect()
    };
    for (name, ext) in found {
        let Ok(entry) = zip.by_name(&name) else { continue };
        if entry.size() > MAX_SOURCE_BYTES {
            continue;
        }
        let mut buf = Vec::new();
        if entry.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut buf).is_ok() && buf.len() as u64 <= MAX_SOURCE_BYTES {
            return Some((buf, ext.to_string(), format!("{} ({})", pk3.display(), name)));
        }
    }
    None
}

/// Search the folders in order: loose pictures first, then every `.pk3` in the folder, the last one by name first (the
/// game lets later archives replace earlier ones). Returns the picture's bytes, its type and where it came from.
pub fn find_source(sources: &[PathBuf], map: &str) -> Option<(Vec<u8>, String, String)> {
    for dir in sources {
        if let Some(found) = find_loose(dir, map) {
            return Some(found);
        }
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut pk3s: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pk3")))
            .collect();
        pk3s.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        for p in pk3s.iter().rev() {
            if let Some(found) = find_in_pk3(p, map) {
                return Some(found);
            }
        }
    }
    None
}

// ------------------------------------------------------------------------------------------ making the square

/// Decode, make square (`size` x `size`) and encode as JPEG. Never panics on a broken or hostile file.
pub fn render(bytes: &[u8], ext: &str, size: u32, fit: Fit) -> Result<Vec<u8>, String> {
    let format = format_of(ext).ok_or_else(|| format!("'{}' is not a picture type I read (jpg, png, tga)", ext))?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let img: DynamicImage = reader.decode().map_err(|e| format!("the picture could not be read: {}", e))?;
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return Err("the picture is empty".into());
    }
    let square: RgbImage = match fit {
        Fit::Crop => {
            let s = w.min(h);
            let cropped = imageops::crop_imm(&rgb, (w - s) / 2, (h - s) / 2, s, s).to_image();
            imageops::resize(&cropped, size, size, FilterType::Lanczos3)
        }
        Fit::Pad => {
            let (nw, nh) = if w >= h { (size, ((h as u64 * size as u64) / w as u64).max(1) as u32) } else { (((w as u64 * size as u64) / h as u64).max(1) as u32, size) };
            let scaled = imageops::resize(&rgb, nw, nh, FilterType::Lanczos3);
            let mut canvas = RgbImage::from_pixel(size, size, image::Rgb(PAD_COLOUR));
            imageops::replace(&mut canvas, &scaled, ((size - nw) / 2) as i64, ((size - nh) / 2) as i64);
            canvas
        }
    };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 88)
        .encode(square.as_raw(), size, size, ExtendedColorType::Rgb8)
        .map_err(|e| format!("the picture could not be saved: {}", e))?;
    Ok(out)
}

// ------------------------------------------------------------------------------------------ the service

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Pending,
    Found(String),
    Missing(Instant),
}

type Key = (Vec<PathBuf>, String, u32, Fit);

/// Finds, makes and remembers the pictures. One per bot; the scoreboards ask it for the current map.
pub struct Levelshots {
    out: PathBuf,
    cache: Mutex<HashMap<Key, Entry>>,
}

impl Levelshots {
    pub fn new(out: PathBuf) -> Arc<Levelshots> {
        Arc::new(Levelshots { out, cache: Mutex::new(HashMap::new()) })
    }

    #[cfg(test)]
    pub fn out_dir(&self) -> &Path {
        &self.out
    }

    /// The picture for `map`, finding and making it now. Returns the stored file's name, or the reason there is none.
    pub fn resolve(&self, sources: &[PathBuf], map: &str, size: u32, fit: Fit) -> Result<String, String> {
        let map = clean_map(map).ok_or("That is not a map name (letters, digits, _ and - only).")?;
        let size = size.clamp(MIN_SIZE, MAX_SIZE);
        let (bytes, ext, from) = find_source(sources, &map).ok_or_else(|| {
            if sources.is_empty() {
                "There is no folder to look in - set the server's level pictures folder, or its UrT folder (spectator video).".to_string()
            } else {
                format!("No picture for {} in: {}", map, sources.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "))
            }
        })?;
        let jpg = render(&bytes, &ext, size, fit).map_err(|e| format!("{} ({})", e, from))?;
        let hash = hex::encode(Sha256::digest(&jpg));
        let name = format!("{}-{}{}-{}.jpg", map, size, fit.letter(), &hash[..8]);
        let path = self.out.join(&name);
        if !path.exists() {
            std::fs::create_dir_all(&self.out).map_err(|e| format!("cannot create {}: {}", self.out.display(), e))?;
            let tmp = self.out.join(format!(".{}.tmp", name));
            std::fs::write(&tmp, &jpg).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
        }
        Ok(name)
    }

    /// What is known right now about `map`: the file name once it has been made. The first call starts the search in the
    /// background and answers None; the caller asks again on its next redraw.
    pub fn lookup(self: &Arc<Self>, sources: &[PathBuf], map: &str, size: u32, fit: Fit) -> Option<String> {
        let clean = clean_map(map)?;
        let key: Key = (sources.to_vec(), clean.clone(), size, fit);
        {
            let mut cache = self.cache.lock().unwrap();
            match cache.get(&key) {
                Some(Entry::Found(n)) => return Some(n.clone()),
                Some(Entry::Pending) => return None,
                Some(Entry::Missing(at)) if at.elapsed() < MISSING_RETRY => return None,
                _ => {}
            }
            cache.insert(key.clone(), Entry::Pending);
        }
        let me = self.clone();
        let (sources, key2) = (sources.to_vec(), key.clone());
        let work = move || {
            let entry = match me.resolve(&sources, &clean, size, fit) {
                Ok(name) => Entry::Found(name),
                Err(e) => {
                    tracing::debug!("no level picture for {}: {}", clean, e);
                    Entry::Missing(Instant::now())
                }
            };
            me.cache.lock().unwrap().insert(key2, entry);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn_blocking(work);
            }
            Err(_) => work(),
        }
        None
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write;

    pub fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gtv-ls-{}-{}", tag, crate::config::random_token(4)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A picture with a different colour in each corner, so cropping and padding can be told apart.
    pub fn sample(w: u32, h: u32, ext: &str) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| match (x < w / 2, y < h / 2) {
            (true, true) => image::Rgb([220, 40, 40]),
            (false, true) => image::Rgb([40, 200, 60]),
            (true, false) => image::Rgb([40, 80, 220]),
            (false, false) => image::Rgb([230, 210, 50]),
        });
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img).write_to(&mut out, format_of(ext).unwrap()).unwrap();
        out.into_inner()
    }

    pub fn write_pk3(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, data) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    fn size_of(jpg: &[u8]) -> (u32, u32) {
        image::load_from_memory(jpg).unwrap().to_rgb8().dimensions()
    }

    #[test]
    fn only_plain_map_names_are_looked_up() {
        assert_eq!(clean_map("UT4_Turnpike").as_deref(), Some("ut4_turnpike"));
        assert_eq!(clean_map("ut4-casa_b3").as_deref(), Some("ut4-casa_b3"));
        for bad in ["", "../etc/passwd", "a/b", "a b", "a.jpg", "x\0y", &"a".repeat(65)] {
            assert_eq!(clean_map(bad), None, "{:?}", bad);
        }
    }

    #[test]
    fn only_the_files_this_module_makes_can_be_served() {
        assert!(valid_file_name("ut4_turnpike-320c-0123abcd.jpg"));
        assert!(valid_file_name("ut4-casa-640p-ffffffff.jpg"));
        for bad in ["../x-320c-0123abcd.jpg", "ut4_turnpike-320c-0123ABCD.jpg", "ut4_turnpike-320x-0123abcd.jpg", "ut4_turnpike-32c-0123abcd.jpg",
                    "ut4_turnpike-320c-0123abc.jpg", "ut4_turnpike-320c-0123abcd.png", "a/b-320c-0123abcd.jpg", "-320c-0123abcd.jpg", "x.jpg", ""] {
            assert!(!valid_file_name(bad), "{:?}", bad);
        }
    }

    #[test]
    fn every_picture_comes_out_exactly_square_whatever_it_started_as() {
        for (w, h, ext) in [(512, 384, "jpg"), (256, 192, "png"), (384, 512, "jpg"), (64, 48, "tga"), (800, 800, "png"), (1024, 100, "jpg")] {
            for fit in [Fit::Crop, Fit::Pad] {
                for size in [128, 320, 640] {
                    let out = render(&sample(w, h, ext), ext, size, fit).unwrap();
                    assert_eq!(size_of(&out), (size, size), "{}x{} {} {:?} {}", w, h, ext, fit, size);
                }
            }
        }
    }

    #[test]
    fn crop_fills_the_square_and_pad_keeps_the_whole_picture() {
        let src = sample(512, 384, "jpg");
        let crop = image::load_from_memory(&render(&src, "jpg", 320, Fit::Crop).unwrap()).unwrap().to_rgb8();
        let pad = image::load_from_memory(&render(&src, "jpg", 320, Fit::Pad).unwrap()).unwrap().to_rgb8();
        let near = |p: &image::Rgb<u8>, c: [u8; 3]| p.0.iter().zip(c).all(|(a, b)| (*a as i32 - b as i32).abs() < 24);
        // crop: the very corners are the picture
        assert!(near(crop.get_pixel(2, 2), [220, 40, 40]) && near(crop.get_pixel(317, 317), [230, 210, 50]));
        // pad: a 4:3 picture leaves bars above and below, the picture's own colours in the middle band
        assert!(near(pad.get_pixel(160, 2), PAD_COLOUR) && near(pad.get_pixel(160, 317), PAD_COLOUR));
        assert!(near(pad.get_pixel(2, 160 - 10), [220, 40, 40]) && near(pad.get_pixel(317, 160 + 10), [230, 210, 50]));
    }

    #[test]
    fn broken_and_hostile_files_are_refused_not_crashed_on() {
        assert!(render(b"not a picture", "jpg", 320, Fit::Crop).is_err());
        assert!(render(&[], "png", 320, Fit::Crop).is_err());
        assert!(render(&sample(8, 8, "jpg"), "gif", 320, Fit::Crop).is_err());
        let mut cut = sample(64, 64, "png");
        cut.truncate(cut.len() / 2);
        assert!(render(&cut, "png", 320, Fit::Crop).is_err());
    }

    #[test]
    fn finds_loose_files_and_pk3_entries_in_any_letter_case() {
        let t = tmp("find");
        let loose = t.join("loose");
        std::fs::create_dir_all(loose.join("levelshots")).unwrap();
        std::fs::write(loose.join("levelshots").join("UT4_Loose.JPG"), sample(64, 48, "jpg")).unwrap();
        let (_, ext, from) = find_source(std::slice::from_ref(&loose), "ut4_loose").unwrap();
        assert_eq!(ext, "jpg");
        assert!(from.contains("UT4_Loose.JPG"), "{}", from);

        let game = t.join("q3ut4");
        std::fs::create_dir_all(&game).unwrap();
        write_pk3(&game.join("ut4_turnpike.pk3"), &[("maps/ut4_turnpike.bsp", vec![1, 2, 3]), ("levelshots/ut4_turnpike.jpg", sample(256, 192, "jpg"))]);
        write_pk3(&game.join("zz_tga.pk3"), &[("LevelShots/UT4_Tga.tga", sample(64, 64, "tga"))]);
        let (b, ext, from) = find_source(std::slice::from_ref(&game), "ut4_turnpike").unwrap();
        assert_eq!((ext.as_str(), image::load_from_memory(&b).unwrap().to_rgb8().dimensions()), ("jpg", (256, 192)));
        assert!(from.contains("ut4_turnpike.pk3"));
        assert_eq!(find_source(std::slice::from_ref(&game), "ut4_tga").unwrap().1, "tga");
        assert!(find_source(std::slice::from_ref(&game), "ut4_nothere").is_none());
        assert!(find_source(&[t.join("does-not-exist")], "ut4_turnpike").is_none());
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn a_later_archive_replaces_an_earlier_one_and_loose_files_win() {
        let t = tmp("order");
        write_pk3(&t.join("a_old.pk3"), &[("levelshots/ut4_x.jpg", sample(100, 100, "jpg"))]);
        write_pk3(&t.join("z_new.pk3"), &[("levelshots/ut4_x.jpg", sample(200, 200, "jpg"))]);
        let dims = |b: &[u8]| image::load_from_memory(b).unwrap().to_rgb8().dimensions();
        assert_eq!(dims(&find_source(std::slice::from_ref(&t), "ut4_x").unwrap().0), (200, 200));
        std::fs::write(t.join("ut4_x.jpg"), sample(300, 300, "jpg")).unwrap();
        assert_eq!(dims(&find_source(std::slice::from_ref(&t), "ut4_x").unwrap().0), (300, 300));
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn resolve_stores_one_stable_valid_file_and_explains_a_miss() {
        let t = tmp("resolve");
        let game = t.join("q3ut4");
        std::fs::create_dir_all(&game).unwrap();
        write_pk3(&game.join("m.pk3"), &[("levelshots/ut4_turnpike.jpg", sample(256, 192, "jpg"))]);
        let ls = Levelshots::new(t.join("out"));
        let name = ls.resolve(std::slice::from_ref(&game), "UT4_Turnpike", 320, Fit::Crop).unwrap();
        assert!(valid_file_name(&name) && name.starts_with("ut4_turnpike-320c-"), "{}", name);
        assert_eq!(size_of(&std::fs::read(ls.out_dir().join(&name)).unwrap()), (320, 320));
        assert_eq!(ls.resolve(std::slice::from_ref(&game), "ut4_turnpike", 320, Fit::Crop).unwrap(), name, "same picture, same name");
        let other = ls.resolve(std::slice::from_ref(&game), "ut4_turnpike", 256, Fit::Pad).unwrap();
        assert_ne!(other, name);
        assert!(ls.resolve(std::slice::from_ref(&game), "ut4_missing", 320, Fit::Crop).unwrap_err().contains("No picture for ut4_missing"));
        assert!(ls.resolve(&[], "ut4_turnpike", 320, Fit::Crop).unwrap_err().contains("no folder"));
        assert!(ls.resolve(std::slice::from_ref(&game), "../x", 320, Fit::Crop).is_err());
        assert!(std::fs::read_dir(ls.out_dir()).unwrap().filter_map(|e| e.ok()).all(|e| valid_file_name(&e.file_name().to_string_lossy())), "no temp files left behind");
        let _ = std::fs::remove_dir_all(t);
    }

    #[tokio::test]
    async fn lookup_answers_later_remembers_and_retries_misses_only_after_a_while() {
        let t = tmp("lookup");
        let game = t.join("q3ut4");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("ut4_here.jpg"), sample(256, 192, "jpg")).unwrap();
        let ls = Levelshots::new(t.join("out"));
        let src = vec![game.clone()];
        let mut got = ls.lookup(&src, "ut4_here", 320, Fit::Crop);
        for _ in 0..200 {
            if got.is_some() { break }
            tokio::time::sleep(Duration::from_millis(20)).await;
            got = ls.lookup(&src, "ut4_here", 320, Fit::Crop);
        }
        assert!(got.as_deref().is_some_and(valid_file_name), "{:?}", got);

        // a miss is remembered (no new search each redraw) ...
        assert_eq!(ls.lookup(&src, "ut4_gone", 320, Fit::Crop), None);
        for _ in 0..200 {
            if matches!(ls.cache.lock().unwrap().get(&(src.clone(), "ut4_gone".into(), 320, Fit::Crop)), Some(Entry::Missing(_))) { break }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::write(game.join("ut4_gone.jpg"), sample(64, 48, "jpg")).unwrap();
        assert_eq!(ls.lookup(&src, "ut4_gone", 320, Fit::Crop), None, "still remembered as missing");
        // ... until the retry time has passed
        ls.cache.lock().unwrap().insert((src.clone(), "ut4_gone".into(), 320, Fit::Crop), Entry::Missing(Instant::now() - MISSING_RETRY - Duration::from_secs(1)));
        let mut again = ls.lookup(&src, "ut4_gone", 320, Fit::Crop);
        for _ in 0..200 {
            if again.is_some() { break }
            tokio::time::sleep(Duration::from_millis(20)).await;
            again = ls.lookup(&src, "ut4_gone", 320, Fit::Crop);
        }
        assert!(again.is_some());
        let _ = std::fs::remove_dir_all(t);
    }
}
