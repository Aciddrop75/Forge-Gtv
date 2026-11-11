// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! HLS playlist helpers: the server-side anti-ghosting delay.

use std::collections::HashSet;

/// Segments advertised in the live playlist.
pub const WINDOW: usize = 6;

pub fn is_segment_name(name: &str) -> bool {
    name.strip_prefix("seg_")
        .and_then(|r| r.strip_suffix(".ts"))
        .map(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// Returns (playlist text or None, names of segments that may be served) for a delayed live view.
/// Hides the newest `delay` seconds of video; only older segments are advertised or served.
pub fn delayed_view(text: &str, delay: f64, window: usize) -> (Option<String>, HashSet<String>) {
    let (mut seq, mut target) = (0i64, 2i64);
    let mut segs: Vec<(f64, String)> = Vec::new();
    let mut cur: Option<f64> = None;
    for ln in text.lines() {
        let ln = ln.trim();
        if let Some(v) = ln.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = ln.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = v.trim().parse().unwrap_or(2);
        } else if let Some(v) = ln.strip_prefix("#EXTINF:") {
            cur = v.split(',').next().and_then(|d| d.trim().parse().ok());
        } else if !ln.is_empty() && !ln.starts_with('#') {
            if let Some(d) = cur.take() {
                segs.push((d, ln.to_string()));
            }
        }
    }
    let (mut held, mut cut) = (0.0f64, segs.len());
    while cut > 0 && held < delay {
        cut -= 1;
        held += segs[cut].0;
    }
    let avail = &segs[..cut];
    if avail.is_empty() {
        return (None, HashSet::new());
    }
    let start = avail.len().saturating_sub(window);
    let chosen = &avail[start..];
    let first_seq = seq + start as i64;
    let mut out = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n", target, first_seq);
    for (d, n) in chosen {
        out.push_str(&format!("#EXTINF:{:.3},\n{}\n", d, n));
    }
    (Some(out), avail.iter().map(|(_, n)| n.clone()).collect())
}

/// What a viewer needs to be told while the picture is not there yet.
#[derive(Debug, PartialEq)]
pub enum Phase {
    /// No playlist at all: the game client / ffmpeg are still starting.
    Starting,
    /// Segments exist but the anti-ghosting delay has not been built up yet.
    Warming { wait: f64 },
    /// The playlist stopped growing.
    Stalled,
    Live,
}

/// `raw` is the playlist ffmpeg wrote (None if the file does not exist), `age` how long ago it last changed.
pub fn phase(raw: Option<&str>, delay: f64, age: Option<f64>) -> (Phase, f64) {
    let Some(raw) = raw else { return (Phase::Starting, 0.0) };
    let buffered: f64 = raw.lines().filter_map(|l| l.trim().strip_prefix("#EXTINF:")).filter_map(|v| v.split(',').next()?.trim().parse::<f64>().ok()).sum();
    if buffered <= 0.0 {
        return (Phase::Starting, 0.0);
    }
    if age.is_some_and(|a| a > 20.0) {
        return (Phase::Stalled, buffered);
    }
    match delayed_view(raw, delay, WINDOW).0 {
        Some(_) => (Phase::Live, buffered),
        None => (Phase::Warming { wait: (delay - buffered).max(0.0) + 2.0 }, buffered),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist() -> String {
        let mut s = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:10\n");
        for i in 10..30 {
            s.push_str(&format!("#EXTINF:2.000,\nseg_{:06}.ts\n", i));
        }
        s
    }

    #[test]
    fn no_delay_shows_newest() {
        let (v, _) = delayed_view(&playlist(), 0.0, WINDOW);
        let v = v.unwrap();
        assert!(v.contains("seg_000029.ts"));
        assert!(v.contains("MEDIA-SEQUENCE:24"));
    }

    #[test]
    fn delay_hides_newest() {
        let (v, allowed) = delayed_view(&playlist(), 10.0, WINDOW);
        let v = v.unwrap();
        assert!(v.contains("seg_000024.ts"));
        assert!(!v.contains("seg_000025.ts"));
        assert!(!allowed.contains("seg_000025.ts"));
        assert!(allowed.contains("seg_000010.ts"));
    }

    #[test]
    fn delay_longer_than_buffer_is_not_ready() {
        assert!(delayed_view(&playlist(), 500.0, WINDOW).0.is_none());
    }

    #[test]
    fn viewers_are_told_why_the_picture_is_not_there_yet() {
        assert_eq!(phase(None, 30.0, None).0, Phase::Starting);
        assert_eq!(phase(Some("#EXTM3U\n"), 30.0, Some(1.0)).0, Phase::Starting);
        let head = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n";
        let few = format!("{}#EXTINF:2.000,\nseg_000000.ts\n#EXTINF:2.000,\nseg_000001.ts\n", head);
        match phase(Some(&few), 30.0, Some(1.0)).0 {
            Phase::Warming { wait } => assert!((wait - 28.0).abs() < 0.01, "{}", wait),
            other => panic!("{:?}", other),
        }
        assert_eq!(phase(Some(&few), 1.0, Some(1.0)).0, Phase::Live);
        assert_eq!(phase(Some(&few), 0.0, Some(60.0)).0, Phase::Stalled);
        assert_eq!(phase(Some(&playlist()), 10.0, Some(0.5)).0, Phase::Live);
    }

    #[test]
    fn segment_names_are_strict() {
        assert!(is_segment_name("seg_000001.ts"));
        for bad in ["seg_.ts", "seg_1.m3u8", "../seg_1.ts", "seg_1.ts/x", "live.m3u8", "seg_a.ts"] {
            assert!(!is_segment_name(bad), "{}", bad);
        }
    }
}
