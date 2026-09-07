//! Leveled logging to stderr and, optionally, to `data/logs/relay.log`.
//!
//! Request handlers must never wait on a disk write, so file logging goes
//! through a channel drained by one background task. If that task falls behind
//! the channel drops lines rather than applying backpressure to the relay —
//! losing a log line is always better than stalling a user's request.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
    Silent = 4,
}

impl Level {
    pub fn parse(s: &str) -> Level {
        match s.to_lowercase().as_str() {
            "debug" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            "silent" | "off" | "none" => Level::Silent,
            _ => Level::Info,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Silent => "silent",
        }
    }
}

pub struct Logger {
    level: AtomicU8,
    file: Option<PathBuf>,
    tx: Option<mpsc::Sender<String>>,
}

impl Logger {
    /// Stderr only.
    pub fn console(level: Level) -> Arc<Self> {
        Arc::new(Self {
            level: AtomicU8::new(level as u8),
            file: None,
            tx: None,
        })
    }

    /// Stderr plus a background file writer.
    pub fn with_file(level: Level, file: &Path) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel::<String>(4096);
        let path = file.to_path_buf();

        tokio::spawn(async move {
            if let Some(dir) = path.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            let mut handle = match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
            {
                Ok(f) => f,
                Err(err) => {
                    eprintln!("cannot open log file {}: {err}", path.display());
                    return;
                }
            };
            // Batch whatever has piled up into one write syscall.
            let mut batch = Vec::with_capacity(64);
            while rx.recv_many(&mut batch, 64).await > 0 {
                let joined = batch.concat();
                batch.clear();
                if handle.write_all(joined.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        Arc::new(Self {
            level: AtomicU8::new(level as u8),
            file: Some(file.to_path_buf()),
            tx: Some(tx),
        })
    }

    pub fn set_level(&self, level: Level) {
        self.level.store(level as u8, Ordering::Relaxed);
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    fn enabled(&self, level: Level) -> bool {
        (level as u8) >= self.level.load(Ordering::Relaxed)
    }

    pub fn log(&self, level: Level, message: &str) {
        if !self.enabled(level) || level == Level::Silent {
            return;
        }
        let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let line = format!("{stamp} {:<5} {message}\n", level.label());
        eprint!("{line}");
        if let Some(tx) = &self.tx {
            // try_send, never send: a full channel must not block a request.
            let _ = tx.try_send(line);
        }
    }

    pub fn debug(&self, message: impl AsRef<str>) {
        self.log(Level::Debug, message.as_ref());
    }
    pub fn info(&self, message: impl AsRef<str>) {
        self.log(Level::Info, message.as_ref());
    }
    pub fn warn(&self, message: impl AsRef<str>) {
        self.log(Level::Warn, message.as_ref());
    }
    pub fn error(&self, message: impl AsRef<str>) {
        self.log(Level::Error, message.as_ref());
    }
}
