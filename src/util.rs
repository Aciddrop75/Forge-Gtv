// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Small shared helpers: cancellation, time formatting, shell-free process helpers.

use std::time::Duration;

use tokio::sync::watch;

/// The sending half: call `stop()` once to wake every `StopRx`.
pub struct Stop(watch::Sender<bool>);

#[derive(Clone)]
pub struct StopRx(watch::Receiver<bool>);

impl Default for Stop {
    fn default() -> Self {
        Stop::new()
    }
}

impl Stop {
    pub fn new() -> Self {
        Stop(watch::channel(false).0)
    }
    pub fn rx(&self) -> StopRx {
        StopRx(self.0.subscribe())
    }
    pub fn stop(&self) {
        let _ = self.0.send(true);
    }
}

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

impl StopRx {
    pub fn is_stopped(&self) -> bool {
        *self.0.borrow()
    }

    /// Sleep for `d`; returns true early (and immediately next time) if stopped.
    pub async fn sleep(&mut self, d: Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let stopped = tokio::select! {
            _ = tokio::time::sleep(d) => false,
            _ = self.0.wait_for(|v| *v) => true,
        };
        stopped || self.is_stopped()
    }

    pub async fn sleep_secs(&mut self, secs: f64) -> bool {
        self.sleep(Duration::from_secs_f64(secs.max(0.0))).await
    }
}

pub fn utc_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

pub fn utc_stamp() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_wakes_sleepers() {
        let s = Stop::new();
        let mut a = s.rx();
        let t = tokio::spawn(async move { a.sleep(Duration::from_secs(30)).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        s.stop();
        assert!(t.await.unwrap());
        let mut b = s.rx();
        assert!(b.sleep(Duration::from_secs(30)).await); // already stopped: returns at once
    }

    #[tokio::test]
    async fn sleep_times_out_when_not_stopped() {
        let s = Stop::new();
        let mut a = s.rx();
        assert!(!a.sleep(Duration::from_millis(20)).await);
    }

    #[tokio::test]
    async fn dropping_the_sender_stops() {
        let s = Stop::new();
        let mut a = s.rx();
        drop(s);
        assert!(a.sleep(Duration::from_secs(30)).await);
    }
}

/// True when the bot was started by its service wrapper (systemd unit or Windows keep-alive loop).
/// `URT_GTV_SERVICE` is the name used before the project was renamed to The Forge GTV; it still counts.
pub fn under_service() -> bool {
    std::env::var_os("FORGE_GTV_SERVICE").is_some() || std::env::var_os("URT_GTV_SERVICE").is_some()
}
