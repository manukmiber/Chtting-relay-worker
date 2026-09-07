//! Metrics storage and the in-memory counters that keep quota checks off the
//! hot path.
//!
//! Three pieces, all shaped by the same constraint — a request handler must
//! never block on the disk:
//!
//! * [`Store`] owns one writer task fed by a channel. Handlers hand over a
//!   finished row and move on; the writer batches whatever has piled up into a
//!   single transaction.
//! * [`QuotaTracker`] answers "has this key used up its day?" from memory,
//!   seeded from SQLite at startup. The Node version ran a `SELECT` per
//!   request, which at a few hundred concurrent callers is a few hundred
//!   synchronous disk reads per second.
//! * [`RateLimiter`] is a sharded sliding window, so one busy key does not
//!   serialise everyone else behind a single mutex.

pub mod schema;

use anyhow::Result;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub use schema::RequestRecord;

/// How many rows the writer will commit in one transaction.
const WRITE_BATCH: usize = 128;

pub struct Store {
    file: PathBuf,
    tx: mpsc::Sender<WriteMsg>,
    /// A second connection used only for dashboard reads. SQLite in WAL mode
    /// lets it read while the writer commits.
    reader: Arc<Mutex<Connection>>,
}

impl Store {
    pub async fn open(file: &Path, logger: Arc<crate::logging::Logger>) -> Result<Arc<Self>> {
        if let Some(dir) = file.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }

        let path = file.to_path_buf();
        let setup = path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = Connection::open(&setup)?;
            conn.execute_batch(schema::CREATE_SQL)?;
            Ok(())
        })
        .await??;

        let reader = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || -> Result<Connection> {
                let conn = Connection::open(&path)?;
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")?;
                Ok(conn)
            })
            .await??
        };

        let (tx, rx) = mpsc::channel::<WriteMsg>(8192);
        spawn_writer(path.clone(), rx, logger);

        Ok(Arc::new(Self {
            file: file.to_path_buf(),
            tx,
            reader: Arc::new(Mutex::new(reader)),
        }))
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn kind(&self) -> &'static str {
        "sqlite"
    }

    /// Hand a finished request to the writer.
    ///
    /// Never blocks and never fails the request: if the queue is full the row
    /// is dropped and said so out loud, because losing a metrics row is always
    /// better than making a user wait on the disk.
    pub fn insert(&self, record: RequestRecord) -> bool {
        // Boxed so the channel's slots stay small; the record is 35 fields wide.
        self.tx.try_send(WriteMsg::Row(Box::new(record))).is_ok()
    }

    /// Wait until everything queued so far has been committed.
    ///
    /// The channel is FIFO, so by the time the writer reaches this marker every
    /// row sent before it has already been written. Used on shutdown, so a
    /// Ctrl-C does not discard the last un-committed batch, and by tests that
    /// need to read back what they just recorded.
    pub async fn flush(&self) -> bool {
        let (ack, wait) = tokio::sync::oneshot::channel();
        if self.tx.send(WriteMsg::Flush(ack)).await.is_err() {
            return false;
        }
        wait.await.is_ok()
    }

    /// Run a blocking query against the read connection.
    pub async fn read<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.reader.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            work(&guard)
        })
        .await?
    }
}

/// What the writer task accepts.
enum WriteMsg {
    Row(Box<RequestRecord>),
    /// Commit whatever is pending, then answer.
    Flush(tokio::sync::oneshot::Sender<()>),
}

fn spawn_writer(
    path: PathBuf,
    mut rx: mpsc::Receiver<WriteMsg>,
    logger: Arc<crate::logging::Logger>,
) {
    tokio::task::spawn_blocking(move || {
        let mut conn = match Connection::open(&path) {
            Ok(c) => c,
            Err(err) => {
                logger.error(format!(
                    "metrics writer cannot open {}: {err}",
                    path.display()
                ));
                return;
            }
        };
        // WAL lets the dashboard read while this connection writes; NORMAL
        // sync is the right trade on a phone, where an fsync per row would
        // dominate the cost of a request.
        if let Err(err) = conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
        ) {
            logger.error(format!("metrics writer cannot configure sqlite: {err}"));
            return;
        }

        let mut batch: Vec<RequestRecord> = Vec::with_capacity(WRITE_BATCH);
        let mut acks: Vec<tokio::sync::oneshot::Sender<()>> = Vec::new();
        // blocking_recv_many is not available, so drain by hand: take one
        // blocking read, then greedily pull whatever else is already queued.
        while let Some(first) = rx.blocking_recv() {
            match first {
                WriteMsg::Row(record) => batch.push(*record),
                WriteMsg::Flush(ack) => acks.push(ack),
            }
            while batch.len() < WRITE_BATCH {
                match rx.try_recv() {
                    Ok(WriteMsg::Row(record)) => batch.push(*record),
                    Ok(WriteMsg::Flush(ack)) => acks.push(ack),
                    Err(_) => break,
                }
            }
            if !batch.is_empty() {
                if let Err(err) = write_batch(&mut conn, &batch) {
                    logger.error(format!(
                        "failed to record {} request(s): {err}",
                        batch.len()
                    ));
                }
                batch.clear();
            }
            // Answer only after the commit, so a waiter can read the rows back.
            for ack in acks.drain(..) {
                let _ = ack.send(());
            }
        }
    });
}

fn write_batch(conn: &mut Connection, batch: &[RequestRecord]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(schema::INSERT_SQL)?;
        for record in batch {
            stmt.execute(rusqlite::params_from_iter(record.as_params()))?;
        }
    }
    tx.commit()?;
    Ok(())
}

/* -------------------------------------------------------------- quotas -- */

#[derive(Debug, Clone, Default)]
pub struct DayUsage {
    pub requests: u64,
    pub tokens: u64,
}

/// Per-key usage for the current local day, held in memory.
pub struct QuotaTracker {
    day: RwLock<String>,
    usage: RwLock<HashMap<String, DayUsage>>,
}

impl QuotaTracker {
    pub fn new(day: String) -> Self {
        Self {
            day: RwLock::new(day),
            usage: RwLock::new(HashMap::new()),
        }
    }

    /// Load today's totals so a restart does not hand everyone a fresh quota.
    pub async fn seed(&self, store: &Store, day: &str) -> Result<()> {
        let day_owned = day.to_string();
        let rows: Vec<(String, u64, u64)> = store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT key_id, COUNT(*), COALESCE(SUM(total_tokens), 0)
                     FROM requests WHERE day = ? GROUP BY key_id",
                )?;
                let rows = stmt
                    .query_map([&day_owned], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)? as u64,
                            r.get::<_, i64>(2)? as u64,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut usage = self.usage.write();
        usage.clear();
        for (key, requests, tokens) in rows {
            usage.insert(key, DayUsage { requests, tokens });
        }
        *self.day.write() = day.to_string();
        Ok(())
    }

    /// Usage so far today, rolling over automatically at local midnight.
    pub fn get(&self, key_id: &str, today: &str) -> DayUsage {
        self.roll_over(today);
        self.usage.read().get(key_id).cloned().unwrap_or_default()
    }

    pub fn record(&self, key_id: &str, today: &str, tokens: u64) {
        self.roll_over(today);
        let mut usage = self.usage.write();
        let entry = usage.entry(key_id.to_string()).or_default();
        entry.requests += 1;
        entry.tokens += tokens;
    }

    fn roll_over(&self, today: &str) {
        if *self.day.read() == today {
            return;
        }
        let mut day = self.day.write();
        // Re-check: another thread may have rolled it while we waited.
        if *day != today {
            *day = today.to_string();
            self.usage.write().clear();
        }
    }
}

/* --------------------------------------------------------- rate limits -- */

const SHARDS: usize = 16;

/// A per-key sliding window over the last minute.
///
/// Sharded by key hash so two busy keys rarely touch the same mutex; a single
/// global map would turn every request into a contention point.
pub struct RateLimiter {
    shards: Vec<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, key: &str) -> &Mutex<HashMap<String, Vec<Instant>>> {
        // FNV-1a: good enough spread, and no hasher state to carry around.
        let mut hash = 0xcbf29ce484222325u64;
        for byte in key.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        &self.shards[(hash as usize) % SHARDS]
    }

    /// `Ok(())` when the call may proceed, `Err(seconds)` when it must wait.
    pub fn check(&self, key: &str, per_minute: u32) -> Result<(), u64> {
        if per_minute == 0 {
            return Ok(());
        }
        let window = Duration::from_secs(60);
        let now = Instant::now();
        let mut shard = self.shard(key).lock();
        let hits = shard.entry(key.to_string()).or_default();
        hits.retain(|t| now.duration_since(*t) < window);

        if hits.len() as u32 >= per_minute {
            let oldest = hits.first().copied().unwrap_or(now);
            let wait = window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_secs().max(1));
        }
        hits.push(now);
        Ok(())
    }

    /// Drop keys that have gone quiet, so an unbounded set of client keys
    /// cannot grow the map forever.
    pub fn sweep(&self) {
        let now = Instant::now();
        for shard in &self.shards {
            let mut map = shard.lock();
            map.retain(|_, hits| {
                hits.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                !hits.is_empty()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_limiter_admits_up_to_the_limit_then_asks_for_a_wait() {
        let limiter = RateLimiter::new();
        for i in 0..3 {
            assert!(limiter.check("k1", 3).is_ok(), "call {i} should be allowed");
        }
        let Err(wait) = limiter.check("k1", 3) else {
            panic!("the fourth call must be refused");
        };
        assert!((1..=60).contains(&wait));
        // A different key has its own window.
        assert!(limiter.check("k2", 3).is_ok());
        // Zero means unlimited.
        for _ in 0..100 {
            assert!(limiter.check("k3", 0).is_ok());
        }
    }

    #[test]
    fn sweeping_drops_keys_that_have_gone_quiet() {
        let limiter = RateLimiter::new();
        let _ = limiter.check("gone", 10);
        let populated: usize = limiter.shards.iter().map(|s| s.lock().len()).sum();
        assert_eq!(populated, 1);
        // Nothing has expired yet, so a sweep keeps it.
        limiter.sweep();
        let still: usize = limiter.shards.iter().map(|s| s.lock().len()).sum();
        assert_eq!(still, 1);
    }

    #[test]
    fn quota_usage_resets_when_the_local_day_rolls_over() {
        let tracker = QuotaTracker::new("2026-09-07".into());
        tracker.record("key1", "2026-09-07", 500);
        tracker.record("key1", "2026-09-07", 250);
        let used = tracker.get("key1", "2026-09-07");
        assert_eq!((used.requests, used.tokens), (2, 750));

        let tomorrow = tracker.get("key1", "2026-09-08");
        assert_eq!((tomorrow.requests, tomorrow.tokens), (0, 0));
    }
}
