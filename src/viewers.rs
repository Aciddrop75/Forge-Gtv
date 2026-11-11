// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! How many people have the web player open. The player page reports in every few seconds with a random id it makes
//! up for that browser tab; nothing else (no address, no account) is kept, and only in memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A viewer that has not reported for this long is gone (the page reports every 2.5 s).
pub const TTL: Duration = Duration::from_secs(12);
const MAX_PER_STREAM: usize = 5000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Playing the video right now.
    pub watching: u32,
    /// Has the page open (including people still waiting for the delay to build up).
    pub open: u32,
}

#[derive(Default)]
pub struct Viewers {
    inner: Mutex<HashMap<String, HashMap<String, (Instant, bool)>>>,
}

/// Ids are made by the page: letters and digits only, a sensible length.
pub fn valid_id(id: &str) -> bool {
    (8..=40).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Viewers {
    pub fn beat(&self, stream: &str, id: &str, playing: bool) {
        self.beat_at(stream, id, playing, Instant::now());
    }

    pub fn beat_at(&self, stream: &str, id: &str, playing: bool, now: Instant) {
        if !valid_id(id) {
            return;
        }
        let mut all = self.inner.lock().unwrap();
        let m = all.entry(stream.to_string()).or_default();
        if m.len() >= MAX_PER_STREAM {
            m.retain(|_, (t, _)| now.duration_since(*t) < TTL);
            if m.len() >= MAX_PER_STREAM && !m.contains_key(id) {
                return; // flooded: keep what we have
            }
        }
        m.insert(id.to_string(), (now, playing));
    }

    pub fn counts(&self, stream: &str) -> Counts {
        self.counts_at(stream, Instant::now())
    }

    pub fn counts_at(&self, stream: &str, now: Instant) -> Counts {
        let mut all = self.inner.lock().unwrap();
        let Some(m) = all.get_mut(stream) else { return Counts::default() };
        m.retain(|_, (t, _)| now.saturating_duration_since(*t) < TTL);
        Counts { watching: m.values().filter(|(_, p)| *p).count() as u32, open: m.len() as u32 }
    }

    /// Forget a stream (its broadcast ended).
    pub fn clear(&self, stream: &str) {
        self.inner.lock().unwrap().remove(stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_playing_and_open_viewers_and_forgets_the_silent() {
        let v = Viewers::default();
        let t0 = Instant::now();
        v.beat_at("main", "aaaaaaaa1", true, t0);
        v.beat_at("main", "bbbbbbbb2", false, t0);
        v.beat_at("other", "cccccccc3", true, t0);
        assert_eq!(v.counts_at("main", t0), Counts { watching: 1, open: 2 });
        assert_eq!(v.counts_at("other", t0).watching, 1);
        assert_eq!(v.counts_at("nobody", t0), Counts::default());
        // the same page reporting again is still one viewer, and can start playing
        v.beat_at("main", "bbbbbbbb2", true, t0 + Duration::from_secs(5));
        assert_eq!(v.counts_at("main", t0 + Duration::from_secs(6)), Counts { watching: 2, open: 2 });
        // the first one went quiet
        assert_eq!(v.counts_at("main", t0 + Duration::from_secs(14)), Counts { watching: 1, open: 1 });
        assert_eq!(v.counts_at("main", t0 + Duration::from_secs(60)), Counts::default());
    }

    #[test]
    fn bad_ids_are_ignored_and_a_flood_is_capped() {
        let v = Viewers::default();
        let t0 = Instant::now();
        for bad in ["", "short", "has space in it!!", &"x".repeat(41), "semi;colon;;"] {
            v.beat_at("main", bad, true, t0);
        }
        assert_eq!(v.counts_at("main", t0), Counts::default());
        for i in 0..MAX_PER_STREAM + 50 {
            v.beat_at("main", &format!("viewer{:08}", i), true, t0);
        }
        assert_eq!(v.counts_at("main", t0).open as usize, MAX_PER_STREAM);
        v.clear("main");
        assert_eq!(v.counts_at("main", t0), Counts::default());
    }
}
