// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Logging: stderr + rotating file + an in-memory ring the admin panel can show.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const RING: usize = 1000;
const MAX_FILE: u64 = 5 * 1024 * 1024;
const KEEP: usize = 5;

#[derive(Default)]
struct Sink {
    ring: VecDeque<String>,
    partial: String,
    file: Option<(PathBuf, std::fs::File, u64)>,
}

#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Sink>>);

impl LogBuffer {
    pub fn new() -> Self {
        LogBuffer::default()
    }

    pub fn set_file(&self, path: PathBuf) {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.0.lock().unwrap().file = Some((path, f, size));
        }
    }

    /// The newest `n` lines (oldest first).
    pub fn tail(&self, n: usize) -> Vec<String> {
        let g = self.0.lock().unwrap();
        let skip = g.ring.len().saturating_sub(n);
        g.ring.iter().skip(skip).cloned().collect()
    }

    #[allow(dead_code)]
    pub fn push(&self, line: &str) {
        let mut g = self.0.lock().unwrap();
        if g.ring.len() >= RING {
            g.ring.pop_front();
        }
        g.ring.push_back(line.to_string());
    }
}

fn rotate(path: &PathBuf) {
    for i in (1..KEEP).rev() {
        let from = PathBuf::from(format!("{}.{}", path.display(), i));
        let to = PathBuf::from(format!("{}.{}", path.display(), i + 1));
        let _ = std::fs::rename(from, to);
    }
    let _ = std::fs::rename(path, format!("{}.1", path.display()));
}

pub struct TeeWriter(LogBuffer);

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        let mut g = (self.0).0.lock().unwrap();
        g.partial.push_str(&String::from_utf8_lossy(buf));
        while let Some(i) = g.partial.find('\n') {
            let line: String = g.partial.drain(..=i).collect();
            let line = line.trim_end().to_string();
            if g.ring.len() >= RING {
                g.ring.pop_front();
            }
            g.ring.push_back(line.clone());
            if let Some((path, file, size)) = g.file.as_mut() {
                if *size > MAX_FILE {
                    rotate(path);
                    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&*path) {
                        *file = f;
                        *size = 0;
                    }
                }
                if writeln!(file, "{}", line).is_ok() {
                    *size += line.len() as u64 + 1;
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = TeeWriter;
    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter(self.clone())
    }
}

pub fn init(buf: &LogBuffer) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,serenity=warn,tracing::span=off,russh=warn,hyper=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(buf.clone()).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_kept_in_order_and_split_on_newlines() {
        let b = LogBuffer::new();
        let mut w = TeeWriter(b.clone());
        w.write_all(b"one\ntw").unwrap();
        w.write_all(b"o\nthree\n").unwrap();
        assert_eq!(b.tail(10), vec!["one", "two", "three"]);
        assert_eq!(b.tail(2), vec!["two", "three"]);
    }

    #[test]
    fn ring_is_bounded() {
        let b = LogBuffer::new();
        for i in 0..RING + 50 {
            b.push(&format!("l{}", i));
        }
        let t = b.tail(5000);
        assert_eq!(t.len(), RING);
        assert_eq!(t[0], "l50");
    }

    #[test]
    fn file_gets_the_lines() {
        let d = std::env::temp_dir().join(format!("gtv-log-{}", crate::config::random_token(4)));
        let b = LogBuffer::new();
        b.set_file(d.join("bot.log"));
        TeeWriter(b.clone()).write_all(b"hello\n").unwrap();
        assert_eq!(std::fs::read_to_string(d.join("bot.log")).unwrap(), "hello\n");
        let _ = std::fs::remove_dir_all(d);
    }
}
